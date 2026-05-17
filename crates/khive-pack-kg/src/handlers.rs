//! Verb handlers for the KG pack.
//!
//! Each handler: deserialize params from Value → validate → call runtime → serialize result.

use std::str::FromStr;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{EdgeListFilter, EntityPatch, KhiveRuntime, MergeStrategy, RuntimeError};
use khive_storage::types::{Direction, TraversalOptions, TraversalRequest};
use khive_storage::EdgeRelation;

use crate::vocab::{EntityKind, NoteKind};
use crate::KgPack;

// ---- Param structs (serde-only, no rmcp dependency) ----

#[derive(Deserialize)]
struct CreateParams {
    kind: String,
    namespace: Option<String>,
    name: Option<String>,
    entity_kind: Option<String>,
    description: Option<String>,
    content: Option<String>,
    note_kind: Option<String>,
    salience: Option<f64>,
    annotates: Option<Vec<String>>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct GetParams {
    namespace: Option<String>,
    id: String,
}

#[derive(Deserialize)]
struct ListParams {
    kind: String,
    namespace: Option<String>,
    limit: Option<u32>,
    entity_kind: Option<String>,
    source_id: Option<String>,
    target_id: Option<String>,
    relations: Option<Vec<String>>,
    min_weight: Option<f64>,
    max_weight: Option<f64>,
    note_kind: Option<String>,
}

#[derive(Deserialize)]
struct UpdateParams {
    namespace: Option<String>,
    id: String,
    name: Option<String>,
    description: Option<Value>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
    relation: Option<String>,
    weight: Option<f64>,
}

#[derive(Deserialize)]
struct DeleteParams {
    namespace: Option<String>,
    id: String,
    hard: Option<bool>,
}

#[derive(Deserialize)]
struct MergeParams {
    namespace: Option<String>,
    into_id: String,
    from_id: String,
    strategy: Option<String>,
}

#[derive(Deserialize)]
struct SearchParams {
    kind: String,
    namespace: Option<String>,
    query: String,
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct LinkParams {
    namespace: Option<String>,
    source_id: String,
    target_id: String,
    relation: String,
    weight: Option<f64>,
}

#[derive(Deserialize)]
struct NeighborsParams {
    namespace: Option<String>,
    node_id: String,
    direction: Option<String>,
    limit: Option<u32>,
    relations: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TraverseParams {
    namespace: Option<String>,
    roots: Vec<String>,
    max_depth: Option<usize>,
    direction: Option<String>,
    relations: Option<Vec<String>>,
    include_roots: Option<bool>,
}

#[derive(Deserialize)]
struct QueryParams {
    namespace: Option<String>,
    query: String,
}

// ---- Helpers ----

async fn resolve_uuid_async(
    s: &str,
    runtime: &KhiveRuntime,
    namespace: Option<&str>,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        match runtime.resolve_prefix(namespace, s).await {
            Ok(Some(uuid)) => return Ok(uuid),
            Ok(None) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "no record matches prefix: {s:?}"
                )))
            }
            Err(e) => return Err(e),
        }
    }
    Err(RuntimeError::InvalidInput(format!(
        "invalid UUID (expected full UUID or 8+ hex prefix): {s:?}"
    )))
}

fn parse_direction(s: Option<&str>) -> Direction {
    match s {
        Some("in") => Direction::In,
        Some("both") => Direction::Both,
        _ => Direction::Out,
    }
}

fn parse_relation(s: &str) -> Result<EdgeRelation, RuntimeError> {
    s.parse::<EdgeRelation>().map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "unknown relation {s:?}; valid: contains | part_of | instance_of | extends | \
             variant_of | introduced_by | supersedes | depends_on | enables | implements | \
             competes_with | composed_with | annotates"
        ))
    })
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RuntimeError> {
    serde_json::to_value(v).map_err(|e| RuntimeError::Internal(format!("serialize: {e}")))
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

// ---- Handler implementations ----

impl KgPack {
    pub(crate) async fn handle_create(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: CreateParams = deser(params)?;
        match p.kind.as_str() {
            "entity" => {
                let name = p.name.ok_or_else(|| {
                    RuntimeError::InvalidInput("kind=entity requires 'name'".into())
                })?;
                let raw_kind = p.entity_kind.ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "kind=entity requires 'entity_kind' (concept | document | dataset | project | person | org)".into(),
                    )
                })?;
                let validated = EntityKind::from_str(&raw_kind)
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
                let tags = p.tags.unwrap_or_default();
                let entity = self
                    .runtime
                    .create_entity(
                        p.namespace.as_deref(),
                        validated.name(),
                        &name,
                        p.description.as_deref(),
                        p.properties,
                        tags,
                    )
                    .await?;
                to_json(&entity)
            }
            "note" => {
                let content = p.content.ok_or_else(|| {
                    RuntimeError::InvalidInput("kind=note requires 'content'".into())
                })?;
                let kind_str = match p.note_kind.as_deref() {
                    None | Some("") => "observation".to_string(),
                    Some(s) => {
                        let validated = NoteKind::from_str(s)
                            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
                        validated.name().to_string()
                    }
                };
                let salience = p.salience.unwrap_or(0.5);
                let mut annotates = Vec::new();
                for s in p.annotates.unwrap_or_default() {
                    annotates
                        .push(resolve_uuid_async(&s, &self.runtime, p.namespace.as_deref()).await?);
                }
                let note = self
                    .runtime
                    .create_note(
                        p.namespace.as_deref(),
                        &kind_str,
                        p.name.as_deref(),
                        &content,
                        salience,
                        p.properties,
                        annotates,
                    )
                    .await?;
                to_json(&note)
            }
            other => Err(RuntimeError::InvalidInput(format!(
                "unknown kind {other:?}; valid: entity | note"
            ))),
        }
    }

    pub(crate) async fn handle_get(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        if let Some(entity) = self.runtime.get_entity(ns, id).await? {
            return to_json(&serde_json::json!({"kind": "entity", "data": entity}));
        }

        if let Some(note) = self
            .runtime
            .notes(ns)?
            .get_note(id)
            .await
            .map_err(RuntimeError::Storage)?
        {
            if note.namespace == self.runtime.ns(ns) {
                return to_json(&serde_json::json!({"kind": "note", "data": note}));
            }
        }

        if let Some(edge) = self.runtime.get_edge(ns, id).await? {
            return to_json(&serde_json::json!({"kind": "edge", "data": edge}));
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_list(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: ListParams = deser(params)?;
        match p.kind.as_str() {
            "entity" => {
                let kind_filter = match p.entity_kind.as_deref() {
                    Some(k) => {
                        let v = EntityKind::from_str(k)
                            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
                        Some(v.name().to_string())
                    }
                    None => None,
                };
                let limit = p.limit.unwrap_or(50).min(500);
                let entities = self
                    .runtime
                    .list_entities(p.namespace.as_deref(), kind_filter.as_deref(), limit)
                    .await?;
                to_json(&entities)
            }
            "edge" => {
                let source_id = match p.source_id.as_deref() {
                    Some(s) => {
                        Some(resolve_uuid_async(s, &self.runtime, p.namespace.as_deref()).await?)
                    }
                    None => None,
                };
                let target_id = match p.target_id.as_deref() {
                    Some(s) => {
                        Some(resolve_uuid_async(s, &self.runtime, p.namespace.as_deref()).await?)
                    }
                    None => None,
                };
                let relations: Vec<EdgeRelation> = p
                    .relations
                    .unwrap_or_default()
                    .iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let filter = EdgeListFilter {
                    source_id,
                    target_id,
                    relations,
                    min_weight: p.min_weight,
                    max_weight: p.max_weight,
                };
                let limit = p.limit.unwrap_or(100);
                let edges = self
                    .runtime
                    .list_edges(p.namespace.as_deref(), filter, limit)
                    .await?;
                to_json(&edges)
            }
            "note" => {
                let kind_filter = match p.note_kind.as_deref() {
                    None | Some("") => None,
                    Some(s) => {
                        let v = NoteKind::from_str(s)
                            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
                        Some(v.name().to_string())
                    }
                };
                let limit = p.limit.unwrap_or(20).min(200);
                let notes = self
                    .runtime
                    .list_notes(p.namespace.as_deref(), kind_filter.as_deref(), limit)
                    .await?;
                to_json(&notes)
            }
            other => Err(RuntimeError::InvalidInput(format!(
                "unknown kind {other:?}; valid: entity | edge | note"
            ))),
        }
    }

    pub(crate) async fn handle_update(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: UpdateParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        if self.runtime.get_entity(ns, id).await?.is_some() {
            let description = match p.description {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(s)) => Some(Some(s)),
                Some(other) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "description must be null or a string, got: {other}"
                    )))
                }
            };
            let patch = EntityPatch {
                name: p.name,
                description,
                properties: p.properties,
                tags: p.tags,
            };
            let entity = self.runtime.update_entity(ns, id, patch).await?;
            return to_json(&entity);
        }

        if self.runtime.get_edge(ns, id).await?.is_some() {
            let relation = p.relation.as_deref().map(parse_relation).transpose()?;
            let edge = self.runtime.update_edge(ns, id, relation, p.weight).await?;
            return to_json(&edge);
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_delete(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: DeleteParams = deser(params)?;
        let id = resolve_uuid_async(&p.id, &self.runtime, p.namespace.as_deref()).await?;
        let ns = p.namespace.as_deref();

        if self.runtime.get_entity(ns, id).await?.is_some() {
            let deleted = self
                .runtime
                .delete_entity(ns, id, p.hard.unwrap_or(false))
                .await?;
            return to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }));
        }

        if self.runtime.get_edge(ns, id).await?.is_some() {
            let deleted = self.runtime.delete_edge(ns, id).await?;
            return to_json(&serde_json::json!({ "deleted": deleted, "id": p.id }));
        }

        let deleted_note = self
            .runtime
            .delete_note(ns, id, p.hard.unwrap_or(false))
            .await?;
        if deleted_note {
            return to_json(&serde_json::json!({ "deleted": true, "id": p.id }));
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    pub(crate) async fn handle_merge(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: MergeParams = deser(params)?;
        let into_id = resolve_uuid_async(&p.into_id, &self.runtime, p.namespace.as_deref()).await?;
        let from_id = resolve_uuid_async(&p.from_id, &self.runtime, p.namespace.as_deref()).await?;
        let strategy = match p.strategy.as_deref().unwrap_or("prefer_into") {
            "prefer_into" => MergeStrategy::PreferInto,
            "prefer_from" => MergeStrategy::PreferFrom,
            "union" => MergeStrategy::Union,
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown strategy {other:?}; use prefer_into | prefer_from | union"
                )))
            }
        };
        let summary = self
            .runtime
            .merge_entity(p.namespace.as_deref(), into_id, from_id, strategy)
            .await?;
        to_json(&summary)
    }

    pub(crate) async fn handle_search(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: SearchParams = deser(params)?;
        let limit = p.limit.unwrap_or(10).min(100);
        match p.kind.as_str() {
            "entity" => {
                let query_vector = if self.runtime.config().embedding_model.is_some() {
                    Some(self.runtime.embed(&p.query).await?)
                } else {
                    None
                };
                let hits = self
                    .runtime
                    .hybrid_search(p.namespace.as_deref(), &p.query, query_vector, limit)
                    .await?;
                let result: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "entity_id": h.entity_id.to_string(),
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                to_json(&result)
            }
            "note" => {
                let query_vector = if self.runtime.config().embedding_model.is_some() {
                    Some(self.runtime.embed(&p.query).await?)
                } else {
                    None
                };
                let hits = self
                    .runtime
                    .search_notes(p.namespace.as_deref(), &p.query, query_vector, limit)
                    .await?;
                let result: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "note_id": h.note_id.to_string(),
                            "score": h.score.to_f64(),
                        })
                    })
                    .collect();
                to_json(&result)
            }
            other => Err(RuntimeError::InvalidInput(format!(
                "unknown kind {other:?}; valid: entity | note"
            ))),
        }
    }

    pub(crate) async fn handle_link(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: LinkParams = deser(params)?;
        let source =
            resolve_uuid_async(&p.source_id, &self.runtime, p.namespace.as_deref()).await?;
        let target =
            resolve_uuid_async(&p.target_id, &self.runtime, p.namespace.as_deref()).await?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);
        let relation = parse_relation(&p.relation)?;
        let edge = self
            .runtime
            .link(p.namespace.as_deref(), source, target, relation, weight)
            .await?;
        to_json(&edge)
    }

    pub(crate) async fn handle_neighbors(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: NeighborsParams = deser(params)?;
        let node_id = resolve_uuid_async(&p.node_id, &self.runtime, p.namespace.as_deref()).await?;
        let direction = parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let hits = self
            .runtime
            .neighbors(
                p.namespace.as_deref(),
                node_id,
                direction,
                p.limit,
                relations,
            )
            .await?;
        to_json(&hits)
    }

    pub(crate) async fn handle_traverse(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: TraverseParams = deser(params)?;
        let mut roots = Vec::with_capacity(p.roots.len());
        for s in &p.roots {
            roots.push(resolve_uuid_async(s, &self.runtime, p.namespace.as_deref()).await?);
        }
        let direction = parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let options = TraversalOptions {
            max_depth: p.max_depth.unwrap_or(3),
            direction,
            relations,
            min_weight: None,
            limit: None,
        };
        let request = TraversalRequest {
            roots,
            options,
            include_roots: p.include_roots.unwrap_or(true),
        };
        let paths = self
            .runtime
            .traverse(p.namespace.as_deref(), request)
            .await?;
        to_json(&paths)
    }

    pub(crate) async fn handle_query(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: QueryParams = deser(params)?;
        let rows = self.runtime.query(p.namespace.as_deref(), &p.query).await?;
        to_json(&rows)
    }
}
