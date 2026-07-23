//! `create` verb handler.

use serde_json::{json, Value};

use khive_runtime::{EntityCreateSpec, NamespaceToken, RuntimeError, VerbRegistry};

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, immutable_event_error,
    normalize_entity_timestamps, parse_relation, reconcile_specific, remap_note_status,
    resolve_kind_spec, resolve_uuid_unfiltered, to_json, validate_entity_type, validate_weight,
    CreateParams, KindSpec,
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

impl KgPack {
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
                let attempted = entries.len();
                if attempted > 1000 {
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

                // Build EntityCreateSpec for every entry, resolving kind/entity_type at
                // the handler layer (same helpers used by the single-entity path).
                let mut specs: Vec<EntityCreateSpec> = Vec::with_capacity(attempted);
                for (idx, entry) in entries.into_iter().enumerate() {
                    // Resolve the item's own kind.
                    let item_kind_spec = resolve_kind_spec(&entry.kind, registry).map_err(|e| {
                        RuntimeError::InvalidInput(format!("items[{idx}].kind: {e}"))
                    })?;
                    let canonical = match &item_kind_spec {
                        KindSpec::Entity { specific } => {
                            let legacy = entry.entity_kind.as_deref();
                            super::common::reconcile_specific(
                                specific.clone(),
                                legacy,
                                |s| super::common::canonical_entity_kind(s, registry),
                                "entity_kind",
                            )
                            .map_err(|e| {
                                RuntimeError::InvalidInput(format!("items[{idx}]: {e}"))
                            })?
                            .ok_or_else(|| RuntimeError::InvalidInput(format!(
                                "items[{idx}]: kind=entity requires a specific kind — use kind=<concept|…> or kind=entity + entity_kind=<…>"
                            )))?
                        }
                        _ => {
                            return Err(RuntimeError::InvalidInput(format!(
                                "items[{idx}]: bulk create only supports entity kinds; got {:?}",
                                entry.kind
                            )));
                        }
                    };
                    let validated_type =
                        validate_entity_type(&canonical, entry.entity_type.as_deref(), registry)
                            .map_err(|e| {
                                RuntimeError::InvalidInput(format!("items[{idx}]: {e}"))
                            })?;
                    specs.push(EntityCreateSpec {
                        kind: canonical,
                        entity_type: validated_type,
                        name: entry.name,
                        description: entry.description,
                        properties: entry.properties,
                        tags: entry.tags.unwrap_or_default(),
                    });
                }

                if atomic {
                    let entities = self.runtime.create_many(token, specs).await?;
                    let created = entities.len();
                    let mut resp = serde_json::json!({
                        "attempted": attempted,
                        "created": created,
                        "skipped": 0,
                        "failed": 0,
                    });
                    if verbose {
                        resp["entities"] = serde_json::to_value(&entities)
                            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
                    }
                    return super::common::to_json(&resp);
                } else {
                    // Non-atomic: best-effort, per-item errors collected.
                    let mut results: Vec<serde_json::Value> = Vec::new();
                    let mut error_list: Vec<serde_json::Value> = Vec::new();
                    for (idx, spec) in specs.into_iter().enumerate() {
                        match self.runtime.create_many(token, vec![spec]).await {
                            Ok(mut v) => {
                                if verbose {
                                    if let Some(e) = v.pop() {
                                        if let Ok(jv) = serde_json::to_value(&e) {
                                            results.push(jv);
                                        }
                                    }
                                } else {
                                    results.push(serde_json::Value::Null);
                                }
                            }
                            Err(e) => {
                                error_list.push(
                                    serde_json::json!({"index": idx, "error": format!("{e}")}),
                                );
                            }
                        }
                    }
                    let mut resp = serde_json::json!({
                        "attempted": attempted,
                        "created": results.len(),
                        "skipped": 0,
                        "failed": error_list.len(),
                        "errors": error_list,
                    });
                    if verbose {
                        resp["entities"] = serde_json::Value::Array(
                            results.into_iter().filter(|v| !v.is_null()).collect(),
                        );
                    }
                    return super::common::to_json(&resp);
                }
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

        let (mut response, new_id, embedding_input_truncated) = match p.kind.as_str() {
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
                let embed_body_len = p
                    .description
                    .as_deref()
                    .filter(|description| !description.is_empty())
                    .map_or(name.len(), |description| {
                        name.len()
                            .saturating_add(1)
                            .saturating_add(description.len())
                    });
                let truncated = self
                    .runtime
                    .document_embedding_input_len_will_be_truncated(embed_body_len);
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
                (
                    normalize_entity_timestamps(to_json(&entity)?),
                    id,
                    truncated,
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
                let embed_text = p.embedding_content.as_deref().unwrap_or(&content);
                let truncated = self
                    .runtime
                    .document_embedding_input_will_be_truncated(embed_text);
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
                    truncated,
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
