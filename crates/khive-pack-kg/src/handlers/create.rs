//! `create` verb handler.

use serde_json::{json, Value};

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, immutable_event_error,
    normalize_entity_timestamps, parse_relation, reconcile_specific, remap_note_status,
    resolve_kind_spec, resolve_uuid_async, to_json, validate_entity_type, validate_weight,
    CreateParams, KindSpec,
};
use crate::KgPack;

impl KgPack {
    pub(crate) async fn handle_create(
        &self,
        token: &NamespaceToken,
        mut params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("create requires 'kind'".into()))?
            .to_string();

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
                let canonical = sub_kind.clone().expect("entity_kind canonicalized above");
                let name = p.name.ok_or_else(|| {
                    RuntimeError::InvalidInput("kind=entity requires 'name'".into())
                })?;
                if name.trim().is_empty() {
                    return Err(RuntimeError::InvalidInput("name must not be empty".into()));
                }
                let tags = p.tags.unwrap_or_default();
                let validated_type = validate_entity_type(&canonical, p.entity_type.as_deref())?;
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
                    annotates.push(resolve_uuid_async(&s, &self.runtime, token).await?);
                }
                let note = self
                    .runtime
                    .create_note(
                        token,
                        &canonical,
                        p.name.as_deref(),
                        &content,
                        p.salience,
                        p.properties,
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
                    let target =
                        match resolve_uuid_async(&spec.target_id, &self.runtime, token).await {
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
