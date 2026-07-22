//! `create` verb handler.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{EntityCreateSpec, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::Note;

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, immutable_event_error,
    normalize_entity_timestamps, parse_relation, reconcile_specific, remap_note_status,
    resolve_kind_spec, resolve_uuid_unfiltered, to_json, validate_entity_type, validate_weight,
    CreateParams, KindSpec,
};
use crate::KgPack;

enum BulkCreateSpec {
    Entity(EntityCreateSpec),
    Note(BulkNoteCreateSpec),
}

struct BulkNoteCreateSpec {
    kind: String,
    name: Option<String>,
    content: String,
    salience: Option<f64>,
    properties: Option<Value>,
    annotates: Vec<Uuid>,
}

impl KgPack {
    async fn prepare_bulk_create_spec(
        &self,
        token: &NamespaceToken,
        idx: usize,
        entry: super::params::BulkCreateEntry,
        registry: &VerbRegistry,
    ) -> Result<BulkCreateSpec, RuntimeError> {
        let kind = resolve_kind_spec(&entry.kind, registry)
            .map_err(|e| RuntimeError::InvalidInput(format!("items[{idx}].kind: {e}")))?;
        match kind {
            KindSpec::Entity { specific } => {
                if entry.note_kind.is_some()
                    || entry.content.is_some()
                    || entry.salience.is_some()
                    || entry.annotates.is_some()
                {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: note fields are not valid for an entity item"
                    )));
                }
                let canonical = reconcile_specific(
                    specific,
                    entry.entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?
                .ok_or_else(|| RuntimeError::InvalidInput(format!(
                    "items[{idx}]: kind=entity requires a specific kind — use kind=<concept|…> or kind=entity + entity_kind=<…>"
                )))?;
                let entity_type =
                    validate_entity_type(&canonical, entry.entity_type.as_deref(), registry)
                        .map_err(|e| RuntimeError::InvalidInput(format!("items[{idx}]: {e}")))?;
                let name = entry.name.ok_or_else(|| {
                    RuntimeError::InvalidInput(format!("items[{idx}]: entity item requires 'name'"))
                })?;
                Ok(BulkCreateSpec::Entity(EntityCreateSpec {
                    kind: canonical,
                    entity_type,
                    name,
                    description: entry.description,
                    properties: entry.properties,
                    tags: entry.tags.unwrap_or_default(),
                }))
            }
            KindSpec::Note { specific } => {
                if entry.entity_kind.is_some()
                    || entry.entity_type.is_some()
                    || entry.description.is_some()
                {
                    return Err(RuntimeError::InvalidInput(format!(
                        "items[{idx}]: entity fields are not valid for a note item"
                    )));
                }
                let canonical = reconcile_specific(
                    specific,
                    entry.note_kind.as_deref(),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?
                .unwrap_or_else(|| "observation".to_string());
                let content = entry.content.ok_or_else(|| {
                    RuntimeError::InvalidInput(format!(
                        "items[{idx}]: note item requires 'content'"
                    ))
                })?;
                let mut annotates = Vec::new();
                for target in entry.annotates.unwrap_or_default() {
                    annotates.push(
                        resolve_uuid_unfiltered(&target, &self.runtime, token)
                            .await
                            .map_err(|e| {
                                RuntimeError::InvalidInput(format!("items[{idx}].annotates: {e}"))
                            })?,
                    );
                }
                Ok(BulkCreateSpec::Note(BulkNoteCreateSpec {
                    kind: canonical,
                    name: entry.name,
                    content,
                    salience: entry.salience,
                    properties: super::common::merge_note_tags(entry.properties, entry.tags)
                        .map_err(|e| RuntimeError::InvalidInput(format!("items[{idx}]: {e}")))?,
                    annotates,
                }))
            }
            KindSpec::Edge | KindSpec::Event | KindSpec::Proposal => {
                Err(RuntimeError::InvalidInput(format!(
                    "items[{idx}]: bulk create supports only entity and note kinds; got {:?}",
                    entry.kind
                )))
            }
        }
    }

    async fn create_bulk_note(
        &self,
        token: &NamespaceToken,
        spec: BulkNoteCreateSpec,
    ) -> Result<Note, RuntimeError> {
        self.runtime
            .create_note(
                token,
                &spec.kind,
                spec.name.as_deref(),
                &spec.content,
                spec.salience,
                spec.properties,
                spec.annotates,
            )
            .await
    }

    async fn rollback_bulk_notes(
        &self,
        token: &NamespaceToken,
        notes: &[Note],
        cause: RuntimeError,
    ) -> RuntimeError {
        let mut failures = Vec::new();
        for note in notes.iter().rev() {
            if let Err(e) = self
                .runtime
                .delete_note_row_first_for_compensation(token, note.id)
                .await
            {
                failures.push(format!("{}: {e}", note.id));
            }
        }
        if failures.is_empty() {
            cause
        } else {
            RuntimeError::Internal(format!(
                "bulk create failed: {cause}; note rollback failed: {}",
                failures.join("; ")
            ))
        }
    }

    async fn handle_bulk_create(
        &self,
        token: &NamespaceToken,
        entries: Vec<super::params::BulkCreateEntry>,
        atomic: bool,
        verbose: bool,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let attempted = entries.len();
        let mut specs = Vec::with_capacity(attempted);
        for (idx, entry) in entries.into_iter().enumerate() {
            specs.push(
                self.prepare_bulk_create_spec(token, idx, entry, registry)
                    .await?,
            );
        }

        if atomic {
            let mut entity_specs = Vec::new();
            let mut note_specs = Vec::new();
            for spec in specs {
                match spec {
                    BulkCreateSpec::Entity(spec) => entity_specs.push(spec),
                    BulkCreateSpec::Note(spec) => note_specs.push(spec),
                }
            }
            let mut notes = Vec::with_capacity(note_specs.len());
            for spec in note_specs {
                match self.create_bulk_note(token, spec).await {
                    Ok(note) => notes.push(note),
                    Err(e) => return Err(self.rollback_bulk_notes(token, &notes, e).await),
                }
            }
            let entities = match self.runtime.create_many(token, entity_specs).await {
                Ok(entities) => entities,
                Err(e) => return Err(self.rollback_bulk_notes(token, &notes, e).await),
            };
            let mut response = json!({
                "attempted": attempted,
                "created": entities.len() + notes.len(),
                "skipped": 0,
                "failed": 0,
            });
            if verbose {
                response["entities"] = to_json(&entities)?;
                response["notes"] = to_json(&notes)?;
            }
            return Ok(response);
        }

        let mut created = 0;
        let mut errors = Vec::new();
        let mut entities = Vec::new();
        let mut notes = Vec::new();
        for (idx, spec) in specs.into_iter().enumerate() {
            match spec {
                BulkCreateSpec::Entity(spec) => {
                    match self.runtime.create_many(token, vec![spec]).await {
                        Ok(mut value) => {
                            created += 1;
                            if verbose {
                                entities.append(&mut value);
                            }
                        }
                        Err(e) => errors.push(json!({"index": idx, "error": e.to_string()})),
                    }
                }
                BulkCreateSpec::Note(spec) => match self.create_bulk_note(token, spec).await {
                    Ok(note) => {
                        created += 1;
                        if verbose {
                            notes.push(note);
                        }
                    }
                    Err(e) => errors.push(json!({"index": idx, "error": e.to_string()})),
                },
            }
        }
        let mut response = json!({
            "attempted": attempted,
            "created": created,
            "skipped": 0,
            "failed": errors.len(),
            "errors": errors,
        });
        if verbose {
            response["entities"] = to_json(&entities)?;
            response["notes"] = to_json(&notes)?;
        }
        Ok(response)
    }

    pub(crate) async fn handle_create(
        &self,
        token: &NamespaceToken,
        mut params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        // `kind` is required for the single-record path but NOT for the bulk
        // `items` path (each item carries its own kind). Defer the requirement
        // until after the bulk early-exit so `create(items=[...])` works without
        // a redundant top-level `kind`.
        let raw_kind_opt = params
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_string);

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
        // Early exit: if `items` is present, handle bulk entity creation and
        // return before the single-record path executes.
        //
        // Med-1: if `items` is present but malformed, return an error immediately.
        // The previous `.ok()` silently dropped parse failures and fell through to
        // the singleton path, creating a surprising "TopLevelCreated" entity when
        // a bulk item contained an unknown field.
        {
            let maybe_items = if params.get("items").is_some() {
                let raw = params["items"].clone();
                match serde_json::from_value::<Vec<super::params::BulkCreateEntry>>(raw) {
                    Ok(entries) => Some(entries),
                    Err(e) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "create: malformed `items` — could not parse bulk entries: {e}"
                        )));
                    }
                }
            } else {
                None
            };
            if let Some(entries) = maybe_items {
                if params.get("embedding_content").is_some() {
                    return Err(RuntimeError::InvalidInput(
                        "embedding_content is only valid for a singleton kind=note create, not bulk `items`".into(),
                    ));
                }
                if entries.len() > 1000 {
                    return Err(RuntimeError::InvalidInput(
                        "bulk create limited to 1000 entries per request".into(),
                    ));
                }
                let atomic = params
                    .get("atomic")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let verbose = params
                    .get("verbose")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                return self
                    .handle_bulk_create(token, entries, atomic, verbose, registry)
                    .await;
            }
        }
        // ── End bulk path ──────────────────────────────────────────────────────

        let raw_kind = raw_kind_opt
            .ok_or_else(|| RuntimeError::InvalidInput("create requires 'kind'".into()))?;
        let spec = resolve_kind_spec(&raw_kind, registry)?;

        let (sub_kind, hook) = match &spec {
            KindSpec::Entity { specific } => {
                let legacy = params
                    .get("entity_kind")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let canonical = reconcile_specific(
                    specific.clone(),
                    legacy.as_deref(),
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
                let legacy = params
                    .get("note_kind")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|s| !s.is_empty());
                let canonical = reconcile_specific(
                    specific.clone(),
                    legacy.as_deref(),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?
                .unwrap_or_else(|| "observation".to_string());
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

        if let Some(ref h) = hook {
            h.prepare_create(&self.runtime, &mut params).await?;
        }

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

        let (mut response, new_id) = match p.kind.as_str() {
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
                let entity = self
                    .runtime
                    .create_entity(
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
                (normalize_entity_timestamps(to_json(&entity)?), id)
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
                let note = self
                    .runtime
                    .create_note_with_embedding_content(
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
                )
            }
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown kind {other:?}; valid: entity | note"
                )))
            }
        };

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
                    let (source, target) = if relation.is_symmetric() && target < new_id {
                        (target, new_id)
                    } else {
                        (new_id, target)
                    };
                    match self
                        .runtime
                        .link(token, source, target, relation, weight, None)
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
