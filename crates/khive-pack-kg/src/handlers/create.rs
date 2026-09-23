//! `create` verb handler.

use serde_json::{json, Value};
use std::sync::Arc;

use khive_runtime::{
    run_atomic_unit, runtime_error_value, AtomicOpPlan, AtomicRunOutcome, DomainDisposition,
    EntityCreateSpec, KhiveRuntime, KindHook, NamespaceToken, NoteCreateSpec, RuntimeError,
    VerbRegistry,
};
use khive_storage::note::Note;
use khive_storage::Entity;

use super::common::{
    canonical_entity_kind, canonical_note_kind, describe_entity_type_normalization, deser,
    immutable_event_error, normalize_entity_timestamps, parse_relation, reconcile_specific,
    remap_note_status, resolve_kind_spec, resolve_uuid_unfiltered, to_json, validate_entity_type,
    validate_weight, CreateParams, KindSpec,
};
use crate::KgPack;

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

struct PreparedBulkEntity {
    spec: EntityCreateSpec,
    args: Value,
    hook: Option<Arc<dyn KindHook>>,
}

struct PreparedBulkNote {
    spec: NoteCreateSpec,
    args: Value,
    hook: Option<Arc<dyn KindHook>>,
}

/// A bulk `create(items=[...])` item after kind resolution, hook
/// normalization and shared-field validation, still awaiting the
/// runtime-owned admission checks (kind, secret gate, owned-identity
/// derivation) each substrate's own prepare-plan call applies.
enum PreparedBulkItem {
    Entity {
        prepared: PreparedBulkEntity,
        /// `entity_type` alias substitution, echoed in the response even
        /// without `verbose` (see `describe_entity_type_normalization`).
        normalized: Option<Value>,
    },
    Note(PreparedBulkNote),
}

/// One entity or note item, already committed, paired with what its own
/// substrate's response entry and post-commit hook call need.
enum BulkWrite {
    Entity {
        hook: Option<Arc<dyn KindHook>>,
        args: Value,
        entity: Entity,
        /// `entity_type` alias substitution, echoed in the response even
        /// without `verbose` (see `describe_entity_type_normalization`).
        normalized: Option<Value>,
    },
    Note {
        hook: Option<Arc<dyn KindHook>>,
        args: Value,
        note: Note,
    },
}

impl BulkWrite {
    async fn after_create(&self, runtime: &KhiveRuntime) {
        let (hook, args, id, kind) = match self {
            BulkWrite::Entity {
                hook, args, entity, ..
            } => (hook, args, entity.id, entity.kind.as_str()),
            BulkWrite::Note { hook, args, note } => (hook, args, note.id, note.kind.as_str()),
        };
        if let Some(hook) = hook {
            if let Err(error) = hook.after_create(runtime, id, args).await {
                tracing::warn!(
                    %kind,
                    %id,
                    %error,
                    "kind hook after_create failed (storage write already committed)"
                );
            }
        }
    }

    /// This item's `results[idx].result` value: an id/kind/created summary
    /// always, replaced by the full committed record when `verbose`.
    fn ok_result(&self, verbose: bool) -> Result<Value, RuntimeError> {
        let (id, kind, full) = match self {
            BulkWrite::Entity { entity, .. } => (
                entity.id,
                entity.kind.as_str(),
                verbose
                    .then(|| serde_json::to_value(entity))
                    .transpose()
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?,
            ),
            BulkWrite::Note { note, .. } => (
                note.id,
                note.kind.as_str(),
                verbose
                    .then(|| serde_json::to_value(note))
                    .transpose()
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?,
            ),
        };
        Ok(match full {
            Some(Value::Object(mut record)) => {
                record.insert("created".to_string(), json!(true));
                Value::Object(record)
            }
            _ => json!({"id": id, "kind": kind, "created": true}),
        })
    }
}

impl KgPack {
    async fn prepare_create_fields(
        &self,
        kind: &str,
        mut fields: CreateParams,
        args: &mut Value,
        hook: Option<&Arc<dyn KindHook>>,
        registry: &VerbRegistry,
    ) -> Result<(CreateParams, Option<Value>), RuntimeError> {
        // Callers establish the shared field types and canonical kind first.
        // Owners may then normalize values before semantic validation; both
        // singleton and bulk creation pass through this same boundary.
        if let Some(hook) = hook {
            hook.prepare_create(&self.runtime, args).await?;
            fields = deser(args.clone())?;
        }
        super::common::require_object_param(fields.properties.as_ref(), "properties")?;
        if fields.kind != "note"
            && (fields.key.is_some() || fields.embed.is_some() || fields.fence.is_some())
        {
            return Err(RuntimeError::InvalidInput(
                "key, embed and fence apply only to notes".into(),
            ));
        }
        let normalized = if fields.kind == "entity" {
            if fields.embedding_content.is_some() {
                return Err(RuntimeError::InvalidInput(
                    "embedding_content is only valid for kind=note".into(),
                ));
            }
            let name = fields
                .name
                .as_deref()
                .ok_or_else(|| RuntimeError::InvalidInput("kind=entity requires 'name'".into()))?;
            if name.trim().is_empty() {
                return Err(RuntimeError::InvalidInput("name must not be empty".into()));
            }
            let entity_type = validate_entity_type(kind, fields.entity_type.as_deref(), registry)?;
            let normalized = describe_entity_type_normalization(
                fields.entity_type.as_deref(),
                entity_type.as_deref(),
            );
            fields.entity_type = entity_type;
            normalized
        } else {
            None
        };
        Ok((fields, normalized))
    }

    async fn prepare_bulk_entity(
        &self,
        kind: String,
        entry: super::params::BulkCreateEntry,
        token: &NamespaceToken,
        registry: &VerbRegistry,
    ) -> Result<(PreparedBulkEntity, Option<Value>), RuntimeError> {
        let hook = registry.find_kind_hook(&kind);
        // Bulk entries already crossed their typed deserialization boundary.
        // Only hooks need a JSON argument object; the no-hook path stays typed.
        let mut args = if hook.is_some() {
            let mut args = json!({
                "kind": "entity",
                "entity_kind": kind,
                "namespace": token.namespace().as_str(),
            });
            if let Some(value) = &entry.name {
                args["name"] = json!(value);
            }
            if let Some(value) = &entry.entity_type {
                args["entity_type"] = json!(value);
            }
            if let Some(value) = &entry.description {
                args["description"] = json!(value);
            }
            if let Some(value) = &entry.properties {
                args["properties"] = value.clone();
            }
            if let Some(value) = &entry.tags {
                args["tags"] = json!(value);
            }
            args
        } else {
            Value::Null
        };
        let fields = CreateParams {
            kind: "entity".into(),
            entity_type: entry.entity_type,
            name: entry.name,
            description: entry.description,
            properties: entry.properties,
            tags: entry.tags,
            content: None,
            salience: None,
            annotates: None,
            skip_dedup_check: None,
            edges: None,
            embedding_content: None,
            key: None,
            embed: None,
            fence: None,
        };
        let (fields, normalized) = self
            .prepare_create_fields(&kind, fields, &mut args, hook.as_ref(), registry)
            .await?;
        if fields.kind != "entity" {
            return Err(RuntimeError::InvalidInput(
                "bulk create requires entity fields after kind-hook preparation".into(),
            ));
        }
        let name = fields
            .name
            .expect("entity fields validated during preparation");
        Ok((
            PreparedBulkEntity {
                spec: EntityCreateSpec {
                    kind,
                    entity_type: fields.entity_type,
                    name,
                    description: fields.description,
                    properties: fields.properties,
                    tags: fields.tags.unwrap_or_default(),
                },
                args,
                hook,
            },
            normalized,
        ))
    }

    async fn prepare_bulk_note(
        &self,
        kind: String,
        entry: super::params::BulkCreateEntry,
        token: &NamespaceToken,
        registry: &VerbRegistry,
    ) -> Result<PreparedBulkNote, RuntimeError> {
        let hook = registry.find_kind_hook(&kind);
        // Bulk entries already crossed their typed deserialization boundary.
        // Only hooks need a JSON argument object; the no-hook path stays typed.
        let mut args = if hook.is_some() {
            let mut args = json!({
                "kind": "note",
                "note_kind": kind,
                "namespace": token.namespace().as_str(),
            });
            if let Some(value) = &entry.content {
                args["content"] = json!(value);
            }
            if let Some(value) = &entry.name {
                args["name"] = json!(value);
            }
            if let Some(value) = entry.salience {
                args["salience"] = json!(value);
            }
            if let Some(value) = &entry.properties {
                args["properties"] = value.clone();
            }
            if let Some(value) = &entry.tags {
                args["tags"] = json!(value);
            }
            args
        } else {
            Value::Null
        };
        let fields = CreateParams {
            kind: "note".into(),
            entity_type: None,
            name: entry.name,
            description: None,
            properties: entry.properties,
            tags: entry.tags,
            content: entry.content,
            salience: entry.salience,
            annotates: None,
            skip_dedup_check: None,
            edges: None,
            embedding_content: None,
            key: None,
            embed: None,
            fence: None,
        };
        let (fields, _normalized) = self
            .prepare_create_fields(&kind, fields, &mut args, hook.as_ref(), registry)
            .await?;
        if fields.kind != "note" {
            return Err(RuntimeError::InvalidInput(
                "bulk create requires note fields after kind-hook preparation".into(),
            ));
        }
        let content = fields
            .content
            .ok_or_else(|| RuntimeError::InvalidInput("note item requires content".into()))?;
        let properties = super::common::merge_note_tags(fields.properties, fields.tags)?;
        Ok(PreparedBulkNote {
            spec: NoteCreateSpec {
                kind,
                name: fields.name,
                content,
                salience: fields.salience,
                properties,
            },
            args,
            hook,
        })
    }

    /// Resolve one bulk `items[idx]` entry from its raw JSON value: parse it
    /// against the substrate-discriminated [`super::params::BulkCreateEntry`]
    /// shape, resolve which substrate its `kind` names, reject a field that
    /// does not apply to that substrate, and run the same kind-owned
    /// preparation (owner hooks included) a singleton `create` applies.
    /// Parsing per item, rather than deserializing the whole `items` vector
    /// up front, is what lets a malformed item become its own indexed
    /// failure under `atomic:false` instead of failing the entire call.
    async fn prepare_bulk_item(
        &self,
        idx: usize,
        raw: Value,
        token: &NamespaceToken,
        registry: &VerbRegistry,
    ) -> Result<PreparedBulkItem, RuntimeError> {
        let entry: super::params::BulkCreateEntry = serde_json::from_value(raw)
            .map_err(|e| RuntimeError::InvalidInput(format!("items[{idx}]: {e}")))?;
        let item_kind_spec = resolve_kind_spec(&entry.kind, registry)
            .map_err(|e| RuntimeError::InvalidInput(format!("items[{idx}].kind: {e}")))?;
        match item_kind_spec {
            KindSpec::Entity { specific } => {
                if entry.content.is_some() || entry.note_kind.is_some() || entry.salience.is_some()
                {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: content, note_kind and salience apply only to note items"
                    )));
                }
                let canonical = reconcile_specific(
                    specific,
                    entry.entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )
                .map_err(|e| RuntimeError::InvalidInput(format!("items[{idx}]: {e}")))?
                .ok_or_else(|| RuntimeError::InvalidInput(format!(
                    "items[{idx}]: kind=entity requires a specific kind — use kind=<concept|…> or kind=entity + entity_kind=<…>"
                )))?;
                let (prepared, normalized) = self
                    .prepare_bulk_entity(canonical, entry, token, registry)
                    .await
                    .map_err(|error| {
                        RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                    })?;
                Ok(PreparedBulkItem::Entity {
                    prepared,
                    normalized,
                })
            }
            KindSpec::Note { specific } => {
                if entry.entity_kind.is_some()
                    || entry.entity_type.is_some()
                    || entry.description.is_some()
                {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: entity_kind, entity_type and description apply only to entity items"
                    )));
                }
                let canonical = reconcile_specific(
                    specific,
                    entry.note_kind.as_deref(),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )
                .map_err(|e| RuntimeError::InvalidInput(format!("items[{idx}]: {e}")))?
                .unwrap_or_else(|| "observation".to_string());
                if canonical == "scheduled_event" {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: kind=scheduled_event is not creatable via bulk create; \
                         use `schedule.remind` or `schedule.schedule` instead"
                    )));
                }
                let prepared = self
                    .prepare_bulk_note(canonical, entry, token, registry)
                    .await
                    .map_err(|error| {
                        RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                    })?;
                Ok(PreparedBulkItem::Note(prepared))
            }
            _ => Err(RuntimeError::InvalidInput(format!(
                "items[{idx}]: bulk create only supports entity or note kinds; got {:?}",
                entry.kind
            ))),
        }
    }

    /// `atomic: true` (including omitted): every item is validated and
    /// admitted, runtime-owned checks included (kind existence, secret
    /// gate, owned-identity derivation), before any domain write, then
    /// every item's plan joins ONE `run_atomic_unit` call so the batch
    /// commits together or not at all (ADR-115 Amendment 4: the finalizer
    /// composes multiple prepared candidates into the caller's own atomic
    /// unit without committing each item separately inside it). A failure
    /// at any stage returns before `run_atomic_unit` is ever called, so no
    /// item's row, FTS document, or edge is written; a confirmed rollback
    /// likewise leaves no successful item receipt: the whole call becomes
    /// this method's single `Err`, never a partial `results` array.
    async fn commit_bulk_atomic(
        &self,
        token: &NamespaceToken,
        attempted: usize,
        verbose: bool,
        prepared: Vec<Result<PreparedBulkItem, RuntimeError>>,
    ) -> Result<Value, RuntimeError> {
        let mut plans: Vec<AtomicOpPlan> = Vec::with_capacity(attempted);
        let mut writes: Vec<BulkWrite> = Vec::with_capacity(attempted);
        for item in prepared {
            match item? {
                PreparedBulkItem::Entity {
                    prepared,
                    normalized,
                } => {
                    let PreparedBulkEntity { spec, args, hook } = prepared;
                    let idx = writes.len();
                    let (entity, plan) = self
                        .runtime
                        .prepare_bulk_entity_plan(token, spec)
                        .await
                        .map_err(|error| {
                        RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                    })?;
                    plans.push(plan);
                    writes.push(BulkWrite::Entity {
                        hook,
                        args,
                        entity,
                        normalized,
                    });
                }
                PreparedBulkItem::Note(prepared) => {
                    let PreparedBulkNote { spec, args, hook } = prepared;
                    let idx = writes.len();
                    let (note, plan) = self
                        .runtime
                        .prepare_bulk_note_plan(token, spec)
                        .await
                        .map_err(|error| {
                            RuntimeError::InvalidInput(format!("items[{idx}]: {error}"))
                        })?;
                    plans.push(plan);
                    writes.push(BulkWrite::Note { hook, args, note });
                }
            }
        }

        if plans.is_empty() {
            return to_json(&json!({
                "attempted": 0,
                "created": 0,
                "skipped": 0,
                "failed": 0,
                "results": Value::Array(vec![]),
            }));
        }

        match run_atomic_unit(self.runtime.sql().as_ref(), plans).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                for write in &writes {
                    write.after_create(&self.runtime).await;
                }
                let mut results = Vec::with_capacity(writes.len());
                let mut entity_type_normalized: Vec<Value> = Vec::new();
                let mut entities: Vec<Value> = Vec::new();
                for (idx, write) in writes.into_iter().enumerate() {
                    results.push(json!({
                        "index": idx,
                        "ok": true,
                        "result": write.ok_result(verbose)?,
                    }));
                    if let BulkWrite::Entity {
                        entity, normalized, ..
                    } = write
                    {
                        if let Some(mut applied) = normalized {
                            if let Some(obj) = applied.as_object_mut() {
                                obj.insert("index".to_string(), json!(idx));
                            }
                            entity_type_normalized.push(applied);
                        }
                        if verbose {
                            entities.push(
                                serde_json::to_value(&entity)
                                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?,
                            );
                        }
                    }
                }
                let created = results.len();
                let mut resp = json!({
                    "attempted": attempted,
                    "created": created,
                    "skipped": 0,
                    "failed": 0,
                    "results": results,
                });
                if !entity_type_normalized.is_empty() {
                    resp["entity_type_normalized"] = Value::Array(entity_type_normalized);
                }
                if verbose && !entities.is_empty() {
                    resp["entities"] = Value::Array(entities);
                }
                to_json(&resp)
            }
            Ok(AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            }) => Err(RuntimeError::Internal(format!(
                "create: atomic bulk batch rolled back at item index {failed_op_index}: {failure:?}"
            ))),
            Err(e) => Err(RuntimeError::Internal(format!(
                "create: atomic bulk batch failed: {}",
                e.0
            ))),
        }
    }

    /// One `atomic: false` item's own commit: admission (kind, secret gate,
    /// owned-identity derivation) followed by a single-plan `run_atomic_unit`
    /// call scoped to this item alone, so a sibling item's failure can never
    /// roll this one back. The returned [`DomainDisposition`] distinguishes
    /// a confirmed no-write (validation, admission, or a clean transaction
    /// rollback) from a genuinely ambiguous seam failure, matching
    /// [`runtime_error_value`]'s own disposition contract.
    async fn commit_bulk_item(
        &self,
        token: &NamespaceToken,
        item: Result<PreparedBulkItem, RuntimeError>,
    ) -> Result<BulkWrite, (RuntimeError, DomainDisposition)> {
        let item = item.map_err(|error| (error, DomainDisposition::NotCommitted))?;
        let (plan, write) = match item {
            PreparedBulkItem::Entity {
                prepared,
                normalized,
            } => {
                let PreparedBulkEntity { spec, args, hook } = prepared;
                let (entity, plan) = self
                    .runtime
                    .prepare_bulk_entity_plan(token, spec)
                    .await
                    .map_err(|error| (error, DomainDisposition::NotCommitted))?;
                (
                    plan,
                    BulkWrite::Entity {
                        hook,
                        args,
                        entity,
                        normalized,
                    },
                )
            }
            PreparedBulkItem::Note(prepared) => {
                let PreparedBulkNote { spec, args, hook } = prepared;
                let (note, plan) = self
                    .runtime
                    .prepare_bulk_note_plan(token, spec)
                    .await
                    .map_err(|error| (error, DomainDisposition::NotCommitted))?;
                (plan, BulkWrite::Note { hook, args, note })
            }
        };
        match run_atomic_unit(self.runtime.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                write.after_create(&self.runtime).await;
                Ok(write)
            }
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err((
                RuntimeError::Internal(format!("create: item write rolled back: {failure:?}")),
                DomainDisposition::NotCommitted,
            )),
            // The transaction seam itself failed rather than any op's guard
            // or statement. Whether the write landed is unestablished here,
            // not merely unwritten, so this is the "unknown" arm, not
            // "not_committed" (ADR's "genuinely ambiguous storage outcome").
            Err(e) => Err((
                RuntimeError::Internal(format!("create: item write seam failure: {}", e.0)),
                DomainDisposition::Unknown,
            )),
        }
    }

    /// `atomic: false`: each item that passed `prepare_bulk_item` is
    /// admitted and committed independently, in index order. A failure at
    /// any stage is that item's own indexed `results` entry and never
    /// blocks a valid sibling item.
    async fn commit_bulk_best_effort(
        &self,
        token: &NamespaceToken,
        attempted: usize,
        verbose: bool,
        prepared: Vec<Result<PreparedBulkItem, RuntimeError>>,
    ) -> Result<Value, RuntimeError> {
        let mut results: Vec<Value> = Vec::with_capacity(attempted);
        let mut error_list: Vec<Value> = Vec::new();
        let mut entity_type_normalized: Vec<Value> = Vec::new();
        let mut entities: Vec<Value> = Vec::new();
        let mut created = 0usize;

        for (idx, item) in prepared.into_iter().enumerate() {
            match self.commit_bulk_item(token, item).await {
                Ok(write) => {
                    created += 1;
                    results.push(json!({
                        "index": idx,
                        "ok": true,
                        "result": write.ok_result(verbose)?,
                    }));
                    if let BulkWrite::Entity {
                        entity, normalized, ..
                    } = &write
                    {
                        if let Some(applied) = normalized {
                            let mut applied = applied.clone();
                            if let Some(obj) = applied.as_object_mut() {
                                obj.insert("index".to_string(), json!(idx));
                            }
                            entity_type_normalized.push(applied);
                        }
                        if verbose {
                            entities.push(
                                serde_json::to_value(entity)
                                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?,
                            );
                        }
                    }
                }
                Err((error, disposition)) => {
                    // `errors[].error` keeps its existing string form; the
                    // structured projection is carried by `results` only.
                    let message = error.to_string();
                    let projected = runtime_error_value(error, disposition);
                    let domain_disposition = projected["domain_disposition"].clone();
                    error_list.push(json!({"index": idx, "error": message}));
                    results.push(json!({
                        "index": idx,
                        "ok": false,
                        "domain_disposition": domain_disposition,
                        "error": projected,
                    }));
                }
            }
        }

        let mut resp = json!({
            "attempted": attempted,
            "created": created,
            "skipped": 0,
            "failed": error_list.len(),
            "errors": error_list,
            "results": results,
        });
        if !entity_type_normalized.is_empty() {
            resp["entity_type_normalized"] = Value::Array(entity_type_normalized);
        }
        if verbose && !entities.is_empty() {
            resp["entities"] = Value::Array(entities);
        }
        to_json(&resp)
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
            "key",
            "embed",
            "fence",
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
        // Early exit: if `items` is present, handle bulk entity/note creation
        // and return before the single-record path executes. Every item is
        // parsed from its own raw JSON value (never as part of one
        // whole-vector deserialization) so a malformed item under
        // `atomic: false` is that item's own indexed failure rather than a
        // failure of the entire call; see `prepare_bulk_item`.
        if params.get("items").is_some() {
            if ["key", "embed", "fence"]
                .iter()
                .any(|field| params.get(*field).is_some())
            {
                return Err(RuntimeError::InvalidInput(
                    "key, embed and fence apply only to singleton notes".into(),
                ));
            }
            if params.get("embedding_content").is_some() {
                return Err(RuntimeError::InvalidInput(
                    "embedding_content is only valid for a singleton kind=note create, not bulk `items`".into(),
                ));
            }
            let raw_items = match &params["items"] {
                Value::Array(items) => items.clone(),
                other => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "create: `items` must be an array; got {other}"
                    )));
                }
            };
            let attempted = raw_items.len();
            if attempted > 1000 {
                return Err(RuntimeError::InvalidInput(
                    "bulk create limited to 1000 entries per request".into(),
                ));
            }
            let atomic = match params.get("atomic") {
                None => true,
                Some(Value::Bool(b)) => *b,
                Some(other) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "create: `atomic` must be a boolean; got {other}"
                    )));
                }
            };
            let verbose = match params.get("verbose") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(other) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "create: `verbose` must be a boolean; got {other}"
                    )));
                }
            };

            let mut prepared: Vec<Result<PreparedBulkItem, RuntimeError>> =
                Vec::with_capacity(attempted);
            for (idx, raw) in raw_items.into_iter().enumerate() {
                prepared.push(self.prepare_bulk_item(idx, raw, token, registry).await);
            }

            return if atomic {
                self.commit_bulk_atomic(token, attempted, verbose, prepared)
                    .await
            } else {
                self.commit_bulk_best_effort(token, attempted, verbose, prepared)
                    .await
            };
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
                match spec {
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
        let fields: CreateParams = deser(params.clone())?;
        let (p, entity_type_normalized) = self
            .prepare_create_fields(
                sub_kind
                    .as_deref()
                    .expect("create kind canonicalized above"),
                fields,
                &mut params,
                hook.as_ref(),
                registry,
            )
            .await?;
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

        let (mut response, new_id, embedding_input_truncated) = match p.kind.as_str() {
            "entity" => {
                let canonical = sub_kind.clone().expect("entity_kind canonicalized above");
                let name = p.name.expect("entity fields validated during preparation");
                let tags = p.tags.unwrap_or_default();
                let (entity, embedding_report) = self
                    .runtime
                    .create_entity_with_embedding_report(
                        token,
                        &canonical,
                        p.entity_type.as_deref(),
                        &name,
                        p.description.as_deref(),
                        p.properties,
                        tags,
                    )
                    .await?;
                let id = entity.id;
                let mut entity_json = normalize_entity_timestamps(to_json(&entity)?);
                if let Some(applied) = entity_type_normalized {
                    if let Some(obj) = entity_json.as_object_mut() {
                        obj.insert("entity_type_normalized".to_string(), applied);
                    }
                }
                (entity_json, id, embedding_report.any_truncated())
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
                let result = if canonical == "head"
                    || p.key.is_some()
                    || p.embed.is_some()
                    || p.fence.is_some()
                {
                    self.runtime
                        .create_note_with_options(
                            token,
                            &canonical,
                            p.name.as_deref(),
                            &content,
                            p.embedding_content.as_deref(),
                            p.salience,
                            None,
                            properties,
                            annotates,
                            None,
                            khive_runtime::note_write::NoteWriteOptions {
                                key: p.key.clone(),
                                embed: p.embed,
                                fence: p.fence,
                                expected_version: None,
                            },
                        )
                        .await
                } else {
                    self.runtime
                        .create_note_with_embedding_content_and_report(
                            token,
                            &canonical,
                            p.name.as_deref(),
                            &content,
                            p.embedding_content.as_deref(),
                            p.salience,
                            properties,
                            annotates,
                        )
                        .await
                };
                let (note, embedding_report) = result.map_err(|error| match error {
                    RuntimeError::Khive(error)
                        if error.details().and_then(|details| details.get("reason"))
                            == Some("key_conflict") =>
                    {
                        let key = p.key.as_deref().unwrap_or("");
                        if registry.allows_note_key_disclosure(token, &canonical, key) {
                            RuntimeError::Khive(error)
                        } else {
                            RuntimeError::Khive(error.with_details(
                                khive_types::Details::new_owned([
                                    ("reason", "key_conflict".into()),
                                    ("key", key.into()),
                                ]),
                            ))
                        }
                    }
                    other => other,
                })?;
                let id = note.id;
                (
                    remap_note_status(normalize_entity_timestamps(to_json(&note)?)),
                    id,
                    embedding_report.any_truncated(),
                )
            }
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown kind {other:?}; valid: entity | note"
                )))
            }
        };

        add_embedding_truncation_warning(&mut response, embedding_input_truncated);

        if let Some(ref h) = hook {
            if let Err(e) = h.after_create(&self.runtime, new_id, &params).await {
                tracing::warn!(
                    kind = %sub_kind.as_deref().unwrap_or(""),
                    id = %new_id,
                    error = %e,
                    "kind hook after_create failed (storage write already committed)"
                );
            }
        }

        // The caller acts on this field by deciding whether to link instead of
        // create, so it has to be able to tell "nothing close exists" from "the
        // comparison did not happen". Emitting the array only when it is
        // non-empty made those two the same observation, and a failed search
        // then read as a clean bill of health. So the array is always present
        // on an entity create that did not opt out, and a comparison that could
        // not run names itself in the companion field instead of arriving as an
        // empty list. Both fields appear together or not at all.
        //
        // `dedup_name` and `dedup_kind` are both Some here whenever this branch
        // is taken: the entity arm above refuses a create without a name and
        // relies on `sub_kind` already being canonicalized.
        if let (Some(ref name), Some(ref kind)) = (&dedup_name, &dedup_kind) {
            const DEDUP_LIMIT: u32 = 3;
            const DEDUP_SCORE_THRESHOLD: f64 = 0.1;
            let (similar, unavailable_reason): (Vec<Value>, Option<String>) = match self
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
                Ok(hits) => (
                    hits.into_iter()
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
                        .collect(),
                    None,
                ),
                Err(e) => {
                    tracing::warn!(
                        id = %new_id,
                        error = %e,
                        "dedup similarity search failed (entity already created)"
                    );
                    (Vec::new(), Some(format!("similarity search failed: {e}")))
                }
            };
            if let Some(obj) = response.as_object_mut() {
                obj.insert("similar_existing".to_string(), json!(similar));
                obj.insert(
                    "similar_existing_unavailable_reason".to_string(),
                    json!(unavailable_reason),
                );
            }
        }

        if let Some(edge_specs) = p.edges {
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
