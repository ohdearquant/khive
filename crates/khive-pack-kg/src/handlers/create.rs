//! `create` verb handler.

use std::sync::Arc;

use serde_json::{json, Value};

use khive_runtime::{
    create_records_atomic, BulkCreatedRecord, BulkNoteCreateSpec, BulkPostCommitFailure,
    BulkPostCommitFailureStage, BulkRecordCreateOutcome, BulkRecordCreateSpec, EntityCreateSpec,
    KindHook, NamespaceToken, RuntimeError, VerbRegistry,
};

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, immutable_event_error,
    normalize_entity_timestamps, parse_relation, reconcile_specific, remap_note_status,
    resolve_kind_spec, resolve_uuid_unfiltered, to_json, validate_entity_type, validate_weight,
    CreateParams, KindSpec,
};
use crate::KgPack;

struct PreparedBulkEntry {
    spec: BulkRecordCreateSpec,
    hook: Option<Arc<dyn KindHook>>,
    hook_params: Value,
    idempotent_note: bool,
}

/// Normalize the optional note natural key into `properties.external_id`.
/// The top-level convenience field is removed so hooks, runtime callers, and
/// storage observe a single canonical representation.
fn normalize_note_external_id(
    params: &mut Value,
    context: &str,
) -> Result<Option<String>, RuntimeError> {
    let root = params.as_object_mut().ok_or_else(|| {
        RuntimeError::InvalidInput(format!("{context}: create params must be an object"))
    })?;

    let explicit = match root.remove("external_id") {
        None => None,
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value),
        Some(_) => {
            return Err(RuntimeError::InvalidInput(format!(
                "{context}: external_id must be a non-empty string"
            )));
        }
    };

    let property_value = match root.get("properties") {
        Some(Value::Object(properties)) => match properties.get("external_id") {
            None => None,
            Some(Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "{context}: properties.external_id must be a non-empty string"
                )));
            }
        },
        Some(Value::Null) | None => None,
        Some(_) if explicit.is_some() => {
            return Err(RuntimeError::InvalidInput(format!(
                "{context}: external_id cannot be merged into non-object properties"
            )));
        }
        Some(_) => None,
    };

    if let (Some(explicit), Some(property)) = (&explicit, &property_value) {
        if explicit != property {
            return Err(RuntimeError::InvalidInput(format!(
                "{context}: external_id disagrees with properties.external_id"
            )));
        }
    }

    let canonical = explicit.or(property_value);
    if let Some(external_id) = &canonical {
        if matches!(root.get("properties"), None | Some(Value::Null)) {
            root.insert(
                "properties".to_string(),
                Value::Object(serde_json::Map::new()),
            );
        }
        let properties = root
            .get_mut("properties")
            .expect("properties inserted immediately above");
        let object = properties.as_object_mut().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "{context}: external_id cannot be merged into non-object properties"
            ))
        })?;
        object.insert("external_id".to_string(), json!(external_id));
    }

    Ok(canonical)
}

fn append_post_commit_stage(
    failures: &mut Vec<BulkPostCommitFailure>,
    note_id: uuid::Uuid,
    stage: BulkPostCommitFailureStage,
) {
    if let Some(existing) = failures
        .iter_mut()
        .find(|failure| failure.note_id == note_id)
    {
        existing.stages.push(stage);
    } else {
        failures.push(BulkPostCommitFailure {
            note_id,
            stages: vec![stage],
        });
    }
}

pub(super) fn add_embedding_truncation_warning(response: &mut Value, truncated: bool) {
    if !truncated {
        return;
    }
    if let Some(obj) = response.as_object_mut() {
        obj.insert(
            "warnings".to_string(),
            json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        );
    }
}

fn required_singleton_kind(params: &Value) -> Result<String, RuntimeError> {
    match params.get("kind") {
        None => Err(RuntimeError::InvalidInput("create requires 'kind'".into())),
        Some(Value::String(kind)) => Ok(kind.clone()),
        Some(value) => Err(RuntimeError::InvalidInput(format!(
            "create: `kind` must be a string; got {value}"
        ))),
    }
}

fn optional_singleton_kind_alias(
    params: &Value,
    field: &str,
) -> Result<Option<String>, RuntimeError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.trim().is_empty() => Err(RuntimeError::InvalidInput(
            format!("create: `{field}` must not be empty"),
        )),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(value) => Err(RuntimeError::InvalidInput(format!(
            "create: `{field}` must be a string or null; got {value}"
        ))),
    }
}

impl KgPack {
    async fn prepare_bulk_entry(
        &self,
        token: &NamespaceToken,
        idx: usize,
        entry: super::params::BulkCreateEntry,
        registry: &VerbRegistry,
    ) -> Result<PreparedBulkEntry, RuntimeError> {
        let item_kind_spec = resolve_kind_spec(&entry.kind, registry)
            .map_err(|error| RuntimeError::InvalidInput(format!("items[{idx}].kind: {error}")))?;

        match item_kind_spec {
            KindSpec::Entity { specific } => {
                if entry.content.is_some()
                    || entry.note_kind.is_some()
                    || entry.salience.is_some()
                    || entry.external_id.is_some()
                {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: content, note_kind, salience, and external_id are note-only fields"
                    )));
                }
                let canonical = reconcile_specific(
                    specific,
                    entry.entity_kind.as_deref(),
                    |value| canonical_entity_kind(value, registry),
                    "entity_kind",
                )
                .map_err(|error| RuntimeError::InvalidInput(format!("items[{idx}]: {error}")))?
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "items[{idx}]: kind=entity requires a specific kind — use kind=<concept|…> or kind=entity + entity_kind=<…>"
                    ))
                })?;

                // Generic bulk entity creation must cross the same pack-owned
                // kind-hook boundary as singleton creation. In particular,
                // workspace's hook validates properties.schema_version before
                // persistence. Keep the hook-mutated params for winner-only
                // after_create below.
                let mut hook_params = serde_json::to_value(&entry)
                    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
                let root = hook_params
                    .as_object_mut()
                    .expect("bulk entry serializes as object");
                root.insert("kind".into(), json!("entity"));
                root.insert("entity_kind".into(), json!(canonical));
                root.insert("namespace".into(), json!(token.namespace().as_str()));

                let hook = registry.find_kind_hook(&canonical);
                if let Some(ref kind_hook) = hook {
                    kind_hook
                        .prepare_create(&self.runtime, &mut hook_params)
                        .await
                        .map_err(|error| {
                            RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                        })?;
                }
                let p: CreateParams = deser(hook_params.clone()).map_err(|error| {
                    RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                })?;
                let entity_type =
                    validate_entity_type(&canonical, p.entity_type.as_deref(), registry).map_err(
                        |error| RuntimeError::InvalidInput(format!("items[{idx}]: {error}")),
                    )?;
                let name = p.name.ok_or_else(|| {
                    RuntimeError::InvalidInput(format!("items[{idx}]: kind=entity requires 'name'"))
                })?;
                if name.trim().is_empty() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: name must not be empty"
                    )));
                }
                Ok(PreparedBulkEntry {
                    spec: BulkRecordCreateSpec::Entity(EntityCreateSpec {
                        kind: canonical,
                        entity_type,
                        name,
                        description: p.description,
                        properties: p.properties,
                        tags: p.tags.unwrap_or_default(),
                    }),
                    hook,
                    hook_params,
                    idempotent_note: false,
                })
            }
            KindSpec::Note { specific } => {
                if entry.entity_kind.is_some() || entry.entity_type.is_some() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: entity_kind and entity_type are entity-only fields"
                    )));
                }
                let canonical = reconcile_specific(
                    specific,
                    entry.note_kind.as_deref(),
                    |value| canonical_note_kind(value, registry),
                    "note_kind",
                )
                .map_err(|error| RuntimeError::InvalidInput(format!("items[{idx}]: {error}")))?
                .unwrap_or_else(|| "observation".to_string());
                if canonical == "scheduled_event" {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: kind=scheduled_event is not creatable via `create`; use `schedule.remind` or `schedule.schedule`"
                    )));
                }

                let mut hook_params = serde_json::to_value(&entry)
                    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
                let root = hook_params
                    .as_object_mut()
                    .expect("bulk entry serializes as object");
                root.insert("kind".into(), json!("note"));
                root.insert("note_kind".into(), json!(canonical));
                root.insert("namespace".into(), json!(token.namespace().as_str()));

                let hook = registry.find_kind_hook(&canonical);
                if let Some(ref kind_hook) = hook {
                    kind_hook
                        .prepare_create(&self.runtime, &mut hook_params)
                        .await
                        .map_err(|error| {
                            RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                        })?;
                }
                let external_id =
                    normalize_note_external_id(&mut hook_params, &format!("items[{idx}]"))?;
                let p: CreateParams = deser(hook_params.clone()).map_err(|error| {
                    RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                })?;
                if p.embedding_content.is_some()
                    || p.annotates
                        .as_ref()
                        .is_some_and(|values| !values.is_empty())
                {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: embedding_content and annotates are singleton-note-only fields"
                    )));
                }
                let content = p.content.ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "items[{idx}]: kind=note requires 'content'"
                    ))
                })?;
                if content.trim().is_empty() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: content must not be empty"
                    )));
                }
                let properties =
                    super::common::merge_note_tags(p.properties, p.tags).map_err(|error| {
                        RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                    })?;
                Ok(PreparedBulkEntry {
                    spec: BulkRecordCreateSpec::Note(BulkNoteCreateSpec {
                        kind: canonical,
                        name: p.name,
                        content,
                        salience: p.salience,
                        properties,
                        external_id: external_id.clone(),
                    }),
                    hook,
                    hook_params,
                    idempotent_note: external_id.is_some(),
                })
            }
            KindSpec::Event => Err(immutable_event_error()),
            KindSpec::Edge => Err(RuntimeError::InvalidInput(format!(
                "items[{idx}]: kind=edge is not creatable via `create`; use `link`"
            ))),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(format!(
                "items[{idx}]: kind=proposal is not creatable via `create`; use `propose`"
            ))),
        }
    }

    async fn handle_bulk_create(
        &self,
        token: &NamespaceToken,
        raw_entries: Vec<Value>,
        enclosing_params: &Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        if enclosing_params.get("embedding_content").is_some() {
            return Err(RuntimeError::InvalidInput(
                "embedding_content is only valid for a singleton kind=note create, not bulk `items`"
                    .into(),
            ));
        }
        if enclosing_params.get("external_id").is_some() {
            return Err(RuntimeError::InvalidInput(
                "external_id belongs on each bulk note item, not beside `items`".into(),
            ));
        }

        let attempted = raw_entries.len();
        if attempted > 1000 {
            return Err(RuntimeError::InvalidInput(
                "bulk create limited to 1000 entries per request".into(),
            ));
        }
        // Best-effort is the bulk default. Callers that require all-or-nothing
        // persistence opt in with `atomic=true`.
        let bulk_bool = |name: &str| match enclosing_params.get(name) {
            None => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            Some(_) => Err(RuntimeError::InvalidInput(format!(
                "bulk create `{name}` must be a boolean"
            ))),
        };
        let atomic = bulk_bool("atomic")?;
        let verbose = bulk_bool("verbose")?;

        let mut prepared: Vec<(usize, PreparedBulkEntry)> = Vec::with_capacity(attempted);
        let mut result_slots: Vec<Option<Value>> = vec![None; attempted];
        let mut errors = Vec::new();

        // Deserialize and validate each input independently. This is what lets
        // one malformed note coexist with successful siblings in best-effort
        // mode, including serde/unknown-field errors.
        for (idx, raw) in raw_entries.into_iter().enumerate() {
            let entry =
                serde_json::from_value::<super::params::BulkCreateEntry>(raw).map_err(|error| {
                    RuntimeError::InvalidInput(format!(
                        "items[{idx}]: malformed bulk entry: {error}"
                    ))
                });
            let outcome = match entry {
                Ok(entry) => self.prepare_bulk_entry(token, idx, entry, registry).await,
                Err(error) => Err(error),
            };
            match outcome {
                Ok(entry) => prepared.push((idx, entry)),
                Err(error) => {
                    let message = error.to_string();
                    result_slots[idx] = Some(json!({
                        "index": idx,
                        "ok": false,
                        "error": message.clone(),
                    }));
                    errors.push(json!({"index": idx, "error": message}));
                }
            }
        }

        if atomic && !errors.is_empty() {
            return Err(RuntimeError::InvalidInput(format!(
                "atomic bulk validation failed; no records were written: {}",
                errors[0]["error"].as_str().unwrap_or("invalid item")
            )));
        }

        let mut successes: Vec<(usize, usize, BulkRecordCreateOutcome)> = Vec::new();
        let mut embedding_input_truncated = false;
        let mut post_commit_failures = Vec::new();

        if atomic {
            let result = create_records_atomic(
                &self.runtime,
                token,
                prepared
                    .iter()
                    .map(|(_, entry)| entry.spec.clone())
                    .collect(),
            )
            .await?;
            embedding_input_truncated = result.embedding_truncation.any_truncated();
            post_commit_failures = result.post_commit_failures;
            successes.extend(
                result
                    .outcomes
                    .into_iter()
                    .enumerate()
                    .map(|(position, outcome)| (position, prepared[position].0, outcome)),
            );
        } else {
            for (position, (idx, entry)) in prepared.iter().enumerate() {
                let outcome = match &entry.spec {
                    BulkRecordCreateSpec::Entity(spec) => {
                        match self.runtime.create_many(token, vec![spec.clone()]).await {
                            Ok(mut entities) => entities
                                .pop()
                                .map(|entity| BulkRecordCreateOutcome {
                                    record: BulkCreatedRecord::Entity(entity),
                                    created: true,
                                })
                                .ok_or_else(|| {
                                    RuntimeError::Internal(
                                        "single-item entity create returned no record".into(),
                                    )
                                }),
                            Err(error) => Err(error),
                        }
                    }
                    BulkRecordCreateSpec::Note(spec) => match self
                        .runtime
                        .create_note_with_embedding_content_and_outcome(
                            token,
                            &spec.kind,
                            spec.name.as_deref(),
                            &spec.content,
                            None,
                            spec.salience,
                            spec.properties.clone(),
                            Vec::new(),
                        )
                        .await
                    {
                        Ok((note, report, created, stages)) => {
                            embedding_input_truncated |= report.any_truncated();
                            if !stages.is_empty() {
                                post_commit_failures.push(BulkPostCommitFailure {
                                    note_id: note.id,
                                    stages,
                                });
                            }
                            Ok(BulkRecordCreateOutcome {
                                record: BulkCreatedRecord::Note(note),
                                created,
                            })
                        }
                        Err(error) => Err(error),
                    },
                };
                match outcome {
                    Ok(outcome) => successes.push((position, *idx, outcome)),
                    Err(error) => {
                        let message = error.to_string();
                        result_slots[*idx] = Some(json!({
                            "index": idx,
                            "ok": false,
                            "error": message.clone(),
                        }));
                        errors.push(json!({"index": idx, "error": message}));
                    }
                }
            }
        }

        for (position, idx, outcome) in &successes {
            let entry = &prepared[*position].1;
            if outcome.created {
                if let Some(ref hook) = entry.hook {
                    if let Err(error) = hook
                        .after_create(&self.runtime, outcome.record.id(), &entry.hook_params)
                        .await
                    {
                        tracing::warn!(
                            index = *idx,
                            id = %outcome.record.id(),
                            error = %error,
                            "bulk kind hook after_create failed (storage write already committed)"
                        );
                        if let BulkCreatedRecord::Note(note) = &outcome.record {
                            append_post_commit_stage(
                                &mut post_commit_failures,
                                note.id,
                                BulkPostCommitFailureStage {
                                    stage: "after_create".to_string(),
                                    model: None,
                                },
                            );
                        }
                    }
                }
            }

            let substrate = match &outcome.record {
                BulkCreatedRecord::Entity(_) => "entity",
                BulkCreatedRecord::Note(_) => "note",
            };
            result_slots[*idx] = Some(json!({
                "index": idx,
                "ok": true,
                "id": outcome.record.id(),
                "substrate": substrate,
                "created": outcome.created,
                "deduplicated": !outcome.created,
            }));
        }

        let created = successes
            .iter()
            .filter(|(_, _, outcome)| outcome.created)
            .count();
        let created_notes = successes
            .iter()
            .filter(|(_, _, outcome)| {
                outcome.created && matches!(&outcome.record, BulkCreatedRecord::Note(_))
            })
            .count();
        let skipped = successes.len() - created;
        let results = result_slots
            .into_iter()
            .enumerate()
            .map(|(idx, value)| {
                value.unwrap_or_else(|| {
                    json!({
                        "index": idx,
                        "ok": false,
                        "error": "internal: bulk item produced no outcome",
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut response = json!({
            "attempted": attempted,
            "created": created,
            "created_notes": created_notes,
            "skipped": skipped,
            "failed": errors.len(),
            "results": results,
            "errors": errors,
        });

        let idempotent_notes = successes
            .iter()
            .filter(|(position, _, _)| prepared[*position].1.idempotent_note)
            .map(|(_, idx, outcome)| {
                json!({
                    "index": idx,
                    "id": outcome.record.id(),
                    "created": outcome.created,
                    "deduplicated": !outcome.created,
                })
            })
            .collect::<Vec<_>>();
        if !idempotent_notes.is_empty() {
            response["idempotent_notes"] = Value::Array(idempotent_notes);
        }

        if verbose {
            let mut entities = Vec::new();
            let mut notes = Vec::new();
            for (_, _, outcome) in &successes {
                match &outcome.record {
                    BulkCreatedRecord::Entity(entity) => entities.push(to_json(entity)?),
                    BulkCreatedRecord::Note(note) => notes.push(remap_note_status(
                        normalize_entity_timestamps(to_json(note)?),
                    )),
                }
            }
            response["entities"] = Value::Array(entities);
            response["notes"] = Value::Array(notes);
        }

        add_embedding_truncation_warning(&mut response, embedding_input_truncated);
        if !post_commit_failures.is_empty() {
            let warning = format!(
                "{} committed note(s) require repair after post-commit indexing or side effects failed",
                post_commit_failures.len()
            );
            if let Some(object) = response.as_object_mut() {
                object
                    .entry("warnings".to_string())
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .expect("warnings is always an array")
                    .push(json!(warning));
                object.insert(
                    "post_commit_failures".to_string(),
                    json!(post_commit_failures
                        .iter()
                        .map(|failure| failure.note_id)
                        .collect::<Vec<_>>()),
                );
                object.insert(
                    "post_commit_failure_details".to_string(),
                    json!(post_commit_failures),
                );
            }
        }
        Ok(response)
    }

    pub(crate) async fn handle_create(
        &self,
        token: &NamespaceToken,
        mut params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        const CREATE_USER_KEYS: &[&str] = &[
            "kind",
            "name",
            "entity_kind",
            "note_kind",
            "entity_type",
            "content",
            "description",
            "tags",
            "properties",
            "salience",
            "annotates",
            "embedding_content",
            "external_id",
            "skip_dedup_check",
            "edges",
            "title",
            "priority",
            "status",
            "assignee",
            "due",
            "start",
            "end",
            "depends_on",
            "context_entity_id",
            "items",
            "atomic",
            "verbose",
        ];
        if let Some(obj) = params.as_object() {
            for key in obj.keys() {
                if !CREATE_USER_KEYS.contains(&key.as_str()) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "create: unknown field `{key}`; allowed: {}",
                        CREATE_USER_KEYS.join(", ")
                    )));
                }
            }
        }

        // ── Bulk path ──────────────────────────────────────────────────────────
        // Keep entries as raw JSON until inside the per-item loop so a serde
        // failure in one entry does not abort valid siblings in best-effort mode.
        if params.get("items").is_some() {
            let raw = params["items"].clone();
            let entries = serde_json::from_value::<Vec<Value>>(raw).map_err(|error| {
                RuntimeError::InvalidInput(format!(
                    "create: malformed `items` — expected an array: {error}"
                ))
            })?;
            return self
                .handle_bulk_create(token, entries, &params, registry)
                .await;
        }
        // ── End bulk path ──────────────────────────────────────────────────────

        // Validate the raw singleton discriminants before resolving a hook or
        // replacing them with canonical values. `Value::as_str` would turn a
        // malformed present value into `None`, allowing (for example) an
        // integer `note_kind` to silently fall back to `observation`. Both
        // legacy aliases are checked eagerly even when the selected `kind`
        // makes one of them irrelevant, so malformed caller input is never
        // hidden by canonicalization.
        let raw_kind = required_singleton_kind(&params)?;
        let raw_entity_kind = optional_singleton_kind_alias(&params, "entity_kind")?;
        let raw_note_kind = optional_singleton_kind_alias(&params, "note_kind")?;
        let spec = resolve_kind_spec(&raw_kind, registry)?;

        let (sub_kind, hook) = match &spec {
            KindSpec::Entity { specific } => {
                let canonical = reconcile_specific(
                    specific.clone(),
                    raw_entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "kind=entity requires a specific kind: either kind=<concept|document|dataset|project|person|org|artifact|service> directly, or kind=entity + entity_kind=<…>".into(),
                    )
                })?;
                let hook = registry.find_kind_hook(&canonical);
                (Some(canonical), hook)
            }
            KindSpec::Note { specific } => {
                let canonical = reconcile_specific(
                    specific.clone(),
                    raw_note_kind.as_deref(),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?
                .unwrap_or_else(|| "observation".to_string());
                if canonical == "scheduled_event" {
                    return Err(RuntimeError::InvalidInput(
                        "kind=scheduled_event is not creatable via `create` — its \
                         `created_by_actor` is a trust boundary for replay dispatch and must \
                         be derived from the authenticated caller, not caller-supplied \
                         properties; use `schedule.remind` or `schedule.schedule` instead"
                            .into(),
                    ));
                }
                let hook = registry.find_kind_hook(&canonical);
                (Some(canonical), hook)
            }
            KindSpec::Event => {
                return Err(immutable_event_error());
            }
            KindSpec::Edge => {
                return Err(RuntimeError::InvalidInput(
                    "kind=edge is not creatable via `create` — use `link` for edges".into(),
                ));
            }
            KindSpec::Proposal => {
                return Err(RuntimeError::InvalidInput(
                    "kind=proposal is not creatable via `create` — use `propose` to create a proposal".into(),
                ));
            }
        };

        if let Some(obj) = params.as_object_mut() {
            obj.insert("kind".into(), json!(spec.substrate_label()));
            if let Some(ref canonical) = sub_kind {
                match &spec {
                    KindSpec::Entity { .. } => {
                        obj.insert("entity_kind".into(), json!(canonical));
                    }
                    KindSpec::Note { .. } => {
                        obj.insert("note_kind".into(), json!(canonical));
                    }
                    KindSpec::Edge | KindSpec::Event | KindSpec::Proposal => {}
                }
            }
        }

        if let Some(obj) = params.as_object_mut() {
            obj.entry("namespace")
                .or_insert_with(|| json!(token.namespace().as_str()));
        }

        // Validate the caller's raw shared-create fields before a kind hook
        // can normalize or replace them. Task creation, for example, derives
        // `name`, `content`, and `salience`; without this first pass a malformed
        // caller value in one of those fields could be overwritten by the hook
        // and therefore escape the canonical `CreateParams` type boundary.
        // `CreateParams` intentionally accepts the flavored hook-only keys as
        // unknown fields, so this validates the shared subset without
        // precluding pack-specific input.
        let _: CreateParams = deser(params.clone())?;

        if let Some(ref h) = hook {
            h.prepare_create(&self.runtime, &mut params).await?;
        }

        let singleton_external_id = match &spec {
            KindSpec::Note { .. } => normalize_note_external_id(&mut params, "create")?,
            KindSpec::Entity { .. } => {
                if params.get("external_id").is_some() {
                    return Err(RuntimeError::InvalidInput(
                        "create: external_id is only valid for kind=note".into(),
                    ));
                }
                None
            }
            KindSpec::Edge | KindSpec::Event | KindSpec::Proposal => None,
        };
        let p: CreateParams = deser(params.clone())?;
        let skip_dedup = p.skip_dedup_check.unwrap_or(false);

        let dedup_name: Option<String> = if !skip_dedup && p.kind == "entity" {
            p.name.clone()
        } else {
            None
        };
        let dedup_kind: Option<String> = if !skip_dedup && p.kind == "entity" {
            sub_kind.clone()
        } else {
            None
        };

        let creates_note = p.kind == "note";
        let (mut response, new_id, embedding_input_truncated, created, mut post_commit_stages) =
            match p.kind.as_str() {
                "entity" => {
                    if p.embedding_content.is_some() {
                        return Err(RuntimeError::InvalidInput(
                            "embedding_content is only valid for kind=note".into(),
                        ));
                    }
                    let canonical = sub_kind.clone().expect("entity_kind canonicalized above");
                    let name = p.name.ok_or_else(|| {
                        RuntimeError::InvalidInput("kind=entity requires 'name'".into())
                    })?;
                    if name.trim().is_empty() {
                        return Err(RuntimeError::InvalidInput("name must not be empty".into()));
                    }
                    let tags = p.tags.unwrap_or_default();
                    let validated_type =
                        validate_entity_type(&canonical, p.entity_type.as_deref(), registry)?;
                    let (entity, embedding_report) = self
                        .runtime
                        .create_entity_with_embedding_report(
                            token,
                            &canonical,
                            validated_type.as_deref(),
                            &name,
                            p.description.as_deref(),
                            p.properties,
                            tags,
                        )
                        .await?;
                    let id = entity.id;
                    (
                        normalize_entity_timestamps(to_json(&entity)?),
                        id,
                        embedding_report.any_truncated(),
                        true,
                        Vec::new(),
                    )
                }
                "note" => {
                    let canonical = sub_kind
                        .clone()
                        .unwrap_or_else(|| "observation".to_string());
                    let content = p.content.ok_or_else(|| {
                        RuntimeError::InvalidInput("kind=note requires 'content'".into())
                    })?;
                    let mut annotates = Vec::new();
                    for s in p.annotates.unwrap_or_default() {
                        annotates.push(resolve_uuid_unfiltered(&s, &self.runtime, token).await?);
                    }
                    let properties = super::common::merge_note_tags(p.properties, p.tags)?;
                    let (note, embedding_report, created, post_commit_stages) = self
                        .runtime
                        .create_note_with_embedding_content_and_outcome(
                            token,
                            &canonical,
                            p.name.as_deref(),
                            &content,
                            p.embedding_content.as_deref(),
                            p.salience,
                            properties,
                            annotates,
                        )
                        .await?;
                    let id = note.id;
                    (
                        remap_note_status(normalize_entity_timestamps(to_json(&note)?)),
                        id,
                        embedding_report.any_truncated(),
                        created,
                        post_commit_stages,
                    )
                }
                other => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "unknown kind {other:?}; valid: entity | note"
                    )));
                }
            };

        add_embedding_truncation_warning(&mut response, embedding_input_truncated);
        if singleton_external_id.is_some() {
            if let Some(object) = response.as_object_mut() {
                object.insert("created".to_string(), json!(created));
                object.insert("deduplicated".to_string(), json!(!created));
            }
        }

        if created {
            if let Some(ref h) = hook {
                if let Err(e) = h.after_create(&self.runtime, new_id, &params).await {
                    tracing::warn!(
                        kind = %sub_kind.as_deref().unwrap_or(""),
                        id = %new_id,
                        error = %e,
                        "kind hook after_create failed (storage write already committed)"
                    );
                    if creates_note {
                        post_commit_stages.push(BulkPostCommitFailureStage {
                            stage: "after_create".to_string(),
                            model: None,
                        });
                    }
                }
            }
        }

        if !post_commit_stages.is_empty() {
            if let Some(object) = response.as_object_mut() {
                object
                    .entry("warnings".to_string())
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .expect("warnings is always an array")
                    .push(json!(
                        "note committed but post-commit indexing or side effects require repair"
                    ));
                object.insert("post_commit_failures".to_string(), json!([new_id]));
                object.insert(
                    "post_commit_failure_details".to_string(),
                    json!([{
                        "id": new_id,
                        "stages": post_commit_stages,
                    }]),
                );
            }
        }

        if let (Some(ref name), Some(ref kind)) = (&dedup_name, &dedup_kind) {
            const DEDUP_LIMIT: u32 = 3;
            const DEDUP_SCORE_THRESHOLD: f64 = 0.1;
            match self
                .runtime
                .hybrid_search(
                    token,
                    name,
                    None,
                    DEDUP_LIMIT + 1,
                    Some(kind.as_str()),
                    None,
                    &[],
                    None,
                )
                .await
            {
                Ok(hits) => {
                    let similar: Vec<Value> = hits
                        .into_iter()
                        .filter(|h| {
                            h.entity_id != new_id && h.score.to_f64() >= DEDUP_SCORE_THRESHOLD
                        })
                        .take(DEDUP_LIMIT as usize)
                        .map(|h| {
                            json!({
                                "id": h.entity_id.to_string(),
                                "name": h.title,
                                "score": h.score.to_f64(),
                            })
                        })
                        .collect();
                    if !similar.is_empty() {
                        if let Some(obj) = response.as_object_mut() {
                            obj.insert("similar_existing".to_string(), json!(similar));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        id = %new_id,
                        error = %e,
                        "dedup similarity search failed (entity already created)"
                    );
                }
            }
        }

        // A natural-key retry returns the canonical row without replaying
        // singleton side effects such as requested edge creation.
        if let Some(edge_specs) = if created { p.edges } else { None } {
            if !edge_specs.is_empty() {
                let mut edge_results: Vec<Value> = Vec::with_capacity(edge_specs.len());
                let mut edge_errors: Vec<Value> = Vec::with_capacity(edge_specs.len());
                for (idx, spec) in edge_specs.into_iter().enumerate() {
                    let target = match resolve_uuid_unfiltered(
                        &spec.target_id,
                        &self.runtime,
                        token,
                    )
                    .await
                    {
                        Ok(id) => id,
                        Err(e) => {
                            edge_errors.push(json!({
                                "index": idx,
                                "target_id": spec.target_id,
                                "error": format!("{e}"),
                            }));
                            continue;
                        }
                    };
                    let relation = match parse_relation(&spec.relation) {
                        Ok(r) => r,
                        Err(e) => {
                            edge_errors.push(json!({
                                "index": idx,
                                "target_id": spec.target_id,
                                "relation": spec.relation,
                                "error": format!("{e}"),
                            }));
                            continue;
                        }
                    };
                    let weight = match validate_weight(spec.weight) {
                        Ok(w) => w,
                        Err(e) => {
                            edge_errors.push(json!({
                                "index": idx,
                                "target_id": spec.target_id,
                                "relation": spec.relation,
                                "error": format!("{e}"),
                            }));
                            continue;
                        }
                    };
                    // Preserve the requested new-record -> target orientation through
                    // validation so rejection diagnostics use that ordered kind pair.
                    // `link` still canonicalizes accepted symmetric edges for persistence.
                    match self
                        .runtime
                        .link(token, new_id, target, relation, weight, None)
                        .await
                    {
                        Ok(edge) => match to_json(&edge) {
                            Ok(v) => edge_results.push(v),
                            Err(e) => edge_errors.push(json!({
                                "index": idx,
                                "error": format!("serialize: {e}"),
                            })),
                        },
                        Err(e) => {
                            edge_errors.push(json!({
                                "index": idx,
                                "target_id": spec.target_id,
                                "relation": spec.relation,
                                "error": format!("{e}"),
                            }));
                        }
                    }
                }
                let mut out = match response {
                    Value::Object(map) => map,
                    other => {
                        let mut m = serde_json::Map::new();
                        m.insert("entity".to_string(), other);
                        m
                    }
                };
                out.insert("edges".to_string(), Value::Array(edge_results));
                if !edge_errors.is_empty() {
                    out.insert("edge_errors".to_string(), Value::Array(edge_errors));
                }
                return Ok(Value::Object(out));
            }
        }

        Ok(response)
    }
}
