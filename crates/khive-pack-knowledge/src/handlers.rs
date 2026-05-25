//! Verb handlers for the knowledge pack.
//!
//! Three verbs are implemented:
//!
//! - `learn` — register a concept entity with domain/tags (sugar over
//!   `create(kind="concept")`).
//! - `cite` — link a concept to a paper/source via `introduced_by`
//!   (sugar over `link(relation="introduced_by")`).
//! - `topic` — list concepts filtered by domain or tag (sugar over
//!   `search(kind="concept")` with a properties filter).

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::EdgeRelation;

use crate::KnowledgePack;

// ── helpers ──────────────────────────────────────────────────────────────────

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

pub(crate) async fn resolve_uuid(
    s: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = s.parse::<Uuid>() {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return match runtime.resolve_prefix(token, s).await? {
            Some(uuid) => Ok(uuid),
            None => Err(RuntimeError::InvalidInput(format!(
                "no record matches prefix: {s:?}"
            ))),
        };
    }
    Err(RuntimeError::InvalidInput(format!(
        "invalid UUID (expected full UUID or 8+ hex prefix): {s:?}"
    )))
}

// ── param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LearnParams {
    /// Name of the concept.
    name: String,
    /// Optional free-text description.
    #[serde(default)]
    description: Option<String>,
    /// Research domain (e.g. "attention", "inference").
    #[serde(default)]
    domain: Option<String>,
    /// Additional tags.
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct CiteParams {
    /// UUID or 8-char prefix of the concept being introduced.
    concept_id: String,
    /// UUID or 8-char prefix of the paper/source entity.
    source_id: String,
    /// Edge weight (default 1.0 — definitional).
    #[serde(default)]
    weight: Option<f64>,
}

#[derive(Deserialize)]
struct TopicParams {
    /// Domain to filter by (matches `properties.domain`).
    #[serde(default)]
    domain: Option<String>,
    /// Free-text search query applied in addition to any filter.
    #[serde(default)]
    query: Option<String>,
    /// Result limit (default 20, max 100).
    #[serde(default)]
    limit: Option<u32>,
}

// ── handler implementations ───────────────────────────────────────────────────

impl KnowledgePack {
    /// Register a concept entity with optional domain and tags.
    ///
    /// Returns the created entity with short `id` and full UUID.
    pub(crate) async fn handle_learn(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: LearnParams = deser(params)?;
        let name = p.name.trim().to_string();
        if name.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "name must not be empty".to_string(),
            ));
        }

        // Build properties: include domain if provided.
        let properties = match &p.domain {
            Some(domain) if !domain.trim().is_empty() => Some(json!({ "domain": domain.trim() })),
            _ => None,
        };

        let mut tags = p.tags.unwrap_or_default();
        // Promote domain to a tag for FTS discoverability.
        if let Some(d) = &p.domain {
            let d = d.trim().to_string();
            if !d.is_empty() && !tags.contains(&d) {
                tags.push(d);
            }
        }

        let entity = self
            .runtime
            .create_entity(
                token,
                "concept",
                None,
                &name,
                p.description.as_deref(),
                properties,
                tags.clone(),
            )
            .await?;

        Ok(json!({
            "id": short_id(entity.id),
            "full_id": entity.id.as_hyphenated().to_string(),
            "kind": "concept",
            "name": entity.name,
            "description": entity.description,
            "domain": p.domain,
            "tags": entity.tags,
            "namespace": entity.namespace,
        }))
    }

    /// Link a concept to the paper/source that introduced it.
    ///
    /// Direction: concept →[introduced_by]→ source (ADR-002 §introduced_by).
    pub(crate) async fn handle_cite(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: CiteParams = deser(params)?;
        let concept_id = resolve_uuid(&p.concept_id, &self.runtime, token).await?;
        let source_id = resolve_uuid(&p.source_id, &self.runtime, token).await?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);

        let edge = self
            .runtime
            .link(
                token,
                concept_id,
                source_id,
                EdgeRelation::IntroducedBy,
                weight,
                None,
            )
            .await?;

        Ok(json!({
            "id": short_id(edge.id.0),
            "full_id": edge.id.0.as_hyphenated().to_string(),
            "relation": "introduced_by",
            "concept_id": concept_id.as_hyphenated().to_string(),
            "source_id": source_id.as_hyphenated().to_string(),
            "weight": weight,
        }))
    }

    /// List concept entities, optionally filtered by domain.
    ///
    /// When `query` is provided, a hybrid search is run and results are
    /// domain-filtered post-retrieval.  Without `query`, all concepts in the
    /// namespace are listed (up to `limit`).
    pub(crate) async fn handle_topic(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TopicParams = deser(params)?;
        let limit = p.limit.unwrap_or(20).min(100);
        let domain_filter = p
            .domain
            .as_deref()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());

        if let Some(ref query) = p.query {
            // Hybrid search path.
            let hits = self
                .runtime
                .hybrid_search(token, query, None, limit * 4, Some("concept"), None)
                .await?;

            // Collect hit IDs for optional domain post-filter.
            let hit_ids: Vec<Uuid> = hits.iter().map(|h| h.entity_id).collect();

            // When a domain filter is active, fetch entity records to check tags.
            let entity_tags: std::collections::HashMap<Uuid, Vec<String>> =
                if domain_filter.is_some() && !hit_ids.is_empty() {
                    let entities = self
                        .runtime
                        .list_entities(token, Some("concept"), None, hit_ids.len() as u32, 0)
                        .await?;
                    entities
                        .into_iter()
                        .filter(|e| hit_ids.contains(&e.id))
                        .map(|e| (e.id, e.tags))
                        .collect()
                } else {
                    std::collections::HashMap::new()
                };

            let results: Vec<Value> = hits
                .into_iter()
                .filter(|h| {
                    if let Some(ref d) = domain_filter {
                        entity_tags
                            .get(&h.entity_id)
                            .map(|tags| tags.iter().any(|t| t.eq_ignore_ascii_case(d)))
                            .unwrap_or(false)
                    } else {
                        true
                    }
                })
                .take(limit as usize)
                .map(|h| {
                    json!({
                        "id": h.entity_id.as_hyphenated().to_string(),
                        "title": h.title,
                        "snippet": h.snippet,
                        "score": h.score.to_f64(),
                    })
                })
                .collect();

            Ok(json!({ "items": results, "total": results.len() }))
        } else {
            // Listing path.
            let entities = self
                .runtime
                .list_entities(token, Some("concept"), None, limit, 0)
                .await?;

            let results: Vec<Value> = entities
                .into_iter()
                .filter(|e| {
                    if let Some(ref d) = domain_filter {
                        e.tags.iter().any(|t| t.eq_ignore_ascii_case(d))
                    } else {
                        true
                    }
                })
                .map(|e| {
                    json!({
                        "id": short_id(e.id),
                        "full_id": e.id.as_hyphenated().to_string(),
                        "name": e.name,
                        "description": e.description,
                        "tags": e.tags,
                    })
                })
                .collect();

            Ok(json!({ "items": results, "total": results.len() }))
        }
    }
}
