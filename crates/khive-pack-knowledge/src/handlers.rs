//! Concept-tier verb handlers: `learn`, `cite`, `topic`, `feedback`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_brain_core::{resolve_consumer_profile, ConsumerKind};
use khive_runtime::{KhiveRuntime, NamespaceToken, RequestIdentity, RuntimeError, VerbRegistry};
use khive_storage::EdgeRelation;

use crate::KnowledgePack;

// ── helpers ──────────────────────────────────────────────────────────────────

use khive_runtime::deser_params as deser;

fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

pub(crate) async fn resolve_uuid(
    s: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    runtime.resolve_uuid_or_prefix(token, s).await
}

// ── param structs ─────────────────────────────────────────────────────────────

// ue-errors C1 (cross-pack): deny_unknown_fields so typo kwargs are rejected
// at deserialization rather than silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LearnParams {
    /// Concept name; auto-derived from `content` first ~60 chars when absent.
    #[serde(default)]
    name: Option<String>,
    /// Free-text description; also accepted as `content` for UX consistency.
    #[serde(default, alias = "content")]
    description: Option<String>,
    /// Research domain (e.g. "attention", "inference").
    #[serde(default)]
    domain: Option<String>,
    /// Additional tags.
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CiteParams {
    concept_id: String,
    source_id: String,
    #[serde(default)]
    weight: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TopicParams {
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

// ── handler implementations ───────────────────────────────────────────────────

impl KnowledgePack {
    /// Register a concept entity with optional domain and tags.
    pub(crate) async fn handle_learn(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: LearnParams = deser(params)?;

        // Resolve name: explicit `name` wins; otherwise auto-generate from `content`
        // (the `description` field).  Truncate at the last word boundary before 60
        // chars so the generated name is readable.
        let name = match p.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(n) => n.to_string(),
            None => {
                let src = p.description.as_deref().unwrap_or("").trim().to_string();
                if src.is_empty() {
                    return Err(RuntimeError::InvalidInput(
                        "name must not be empty (provide 'name' or 'content')".to_string(),
                    ));
                }
                // Truncate at last whitespace boundary <= 60 chars (char-boundary safe).
                if src.chars().count() <= 60 {
                    src.clone()
                } else {
                    let byte_limit = src
                        .char_indices()
                        .nth(60)
                        .map(|(i, _)| i)
                        .unwrap_or(src.len());
                    let boundary = src[..byte_limit]
                        .rfind(char::is_whitespace)
                        .unwrap_or(byte_limit);
                    src[..boundary].trim_end().to_string()
                }
            }
        };

        // Normalise the domain once (trim + lowercase) and use the same value
        // for properties.domain, the promoted tag, and the response — domain matching
        // is case-insensitive, so the three surfaces must agree.
        let domain_norm: Option<String> = p
            .domain
            .as_ref()
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty());

        let properties = domain_norm.as_ref().map(|d| json!({ "domain": d }));

        let mut tags = p.tags.unwrap_or_default();
        if let Some(d) = &domain_norm {
            if !tags.contains(d) {
                tags.push(d.clone());
            }
        }

        // Concept entities are linkable graph rows and must live on the core
        // (main) backend even when the pack is assigned a secondary backend
        // (ADR-073; same seam as the session pack's ADR-083 §4 fix).
        let (entity, embedding_report) = self
            .runtime
            .core()
            .create_entity_with_embedding_report(
                token,
                "concept",
                None,
                &name,
                p.description.as_deref(),
                properties,
                tags.clone(),
            )
            .await?;

        let mut response = json!({
            "id": short_id(entity.id),
            "full_id": entity.id.as_hyphenated().to_string(),
            "kind": "concept",
            "name": entity.name,
            "description": entity.description,
            "domain": domain_norm,
            "tags": entity.tags,
            "namespace": entity.namespace,
        });
        if embedding_report.any_truncated() {
            response["warnings"] =
                json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]);
        }
        Ok(response)
    }

    /// Link a concept to the paper/source that introduced it (`introduced_by` edge).
    pub(crate) async fn handle_cite(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: CiteParams = deser(params)?;
        // Concepts and their sources live on the core graph, so both prefix
        // resolution and the edge write must target the core backend (ADR-073).
        let core = self.runtime.core();
        let concept_id = resolve_uuid(&p.concept_id, &core, token).await?;
        let source_id = resolve_uuid(&p.source_id, &core, token).await?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);

        let edge = core
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
            "weight_requested": p.weight,
            "weight": weight,
        }))
    }

    /// List concept entities, optionally filtered by domain or free-text query.
    pub(crate) async fn handle_topic(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TopicParams = deser(params)?;
        // Concept entities live on the core graph (see handle_learn); every
        // read in this handler must go through the core backend or a
        // secondary-assigned pack would list/search an empty graph (ADR-073).
        let core = self.runtime.core();
        let limit = p.limit.unwrap_or(20).min(100);
        // Normalise domain filter to lowercase for case-insensitive matching.
        let domain_filter = p
            .domain
            .as_deref()
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty());

        if let Some(ref query) = p.query {
            // Search path: hybrid FTS+vector search, then optional domain post-filter.
            // We fetch limit*4 candidates to give the domain filter enough to work with.
            // Report this post-hydration/domain-filter window separately from the
            // full corpus count returned by the unqueried listing branch.
            let hits = core
                .hybrid_search(
                    token,
                    query,
                    None,
                    limit * 4,
                    Some("concept"),
                    None,
                    &[],
                    None,
                )
                .await?;

            // Always fetch entity records for the search hits so we can emit a
            // unified item shape (K-2) and apply the domain filter reliably.
            let hit_ids: Vec<Uuid> = hits.iter().map(|h| h.entity_id).collect();
            let entity_map: std::collections::HashMap<Uuid, _> = if !hit_ids.is_empty() {
                core.get_entities_by_ids(token, &hit_ids)
                    .await?
                    .into_iter()
                    .map(|e| (e.id, e))
                    .collect()
            } else {
                std::collections::HashMap::new()
            };

            // Filter by domain (case-insensitive tag match) and bind each hit to its
            // entity in one pass.  Hits whose entity record is missing are dropped.
            // The entity reference is carried directly into the map step so we never
            // need to re-fetch from the map (which would require an `.unwrap()`).
            let filtered: Vec<_> = hits
                .into_iter()
                .filter_map(|h| {
                    let entity = entity_map.get(&h.entity_id)?;
                    if let Some(ref d) = domain_filter {
                        if !entity.tags.iter().any(|t| t.eq_ignore_ascii_case(d)) {
                            return None;
                        }
                    }
                    Some((h, entity))
                })
                .collect();

            let candidate_window_count = filtered.len();
            let results: Vec<Value> = filtered
                .into_iter()
                .take(limit as usize)
                .map(|(h, entity)| {
                    let mut item = json!({
                        "id": short_id(entity.id),
                        "full_id": entity.id.as_hyphenated().to_string(),
                        "name": entity.name,
                        "description": entity.description,
                        "tags": entity.tags,
                        "score": h.score.to_f64(),
                    });
                    if let Some(snippet) = h.snippet {
                        item["snippet"] = serde_json::Value::String(snippet);
                    }
                    item
                })
                .collect();

            Ok(json!({ "results": results, "candidate_window_count": candidate_window_count }))
        } else {
            // Listing path: DB-level domain filter via tags_any avoids silent
            // truncation (K-3).  `count_entities_tagged` gives the pre-limit
            // match count for `total` (K-6).
            let total = core
                .count_entities_tagged(token, Some("concept"), domain_filter.as_deref())
                .await?;

            let entities = core
                .list_entities_tagged(token, Some("concept"), domain_filter.as_deref(), limit, 0)
                .await?;

            let results: Vec<Value> = entities
                .into_iter()
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

            Ok(json!({ "results": results, "total": total }))
        }
    }
}

impl KnowledgePack {
    /// Resolve `type_weights` for `compose` via the same 3-tier ADR-035 ladder
    /// used by `record_feedback`, completing read/write symmetry for section posteriors.
    ///
    /// Tier 1: explicit `brain_profile` config → `brain.profile` → extract `weight` per section.
    /// Tier 2: namespace-bound profile via `brain.resolve(consumer_kind="knowledge_compose")` → same.
    /// Tier 3: pack-local, namespace-keyed `section_posteriors` mutex → `deterministic_weights()`.
    /// Fallback: `SectionPosteriorState::default()`.
    pub(crate) async fn resolve_compose_type_weights(
        &self,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> HashMap<String, f32> {
        let ns = token.namespace().as_str().to_string();

        // Tier 1: explicit profile from config.
        if let Some(ref profile_id) = self.brain_profile {
            if let Some(weights) = load_profile_type_weights(registry, profile_id, token).await {
                return weights;
            }
        }

        // Tier 2: namespace-bound profile via brain.resolve(consumer_kind="knowledge_compose").
        if let Some(profile_id) =
            resolve_consumer_profile(registry, token, ConsumerKind::KnowledgeCompose).await
        {
            if let Some(weights) = load_profile_type_weights(registry, &profile_id, token).await {
                return weights;
            }
        }

        // Tier 3: exact-namespace pack-local section_posteriors. An arm with
        // no feedback gets fresh defaults rather than inheriting another
        // namespace's tuning.
        if let Ok(states) = self.section_posteriors.lock() {
            if let Some(state) = states.get(&ns) {
                return state
                    .deterministic_weights()
                    .into_iter()
                    .map(|(st, w)| (st.as_str().to_string(), w as f32))
                    .collect();
            }
        }

        // Fallback: fresh default (lock poisoned — should not occur in normal operation).
        khive_brain_core::SectionPosteriorState::default()
            .deterministic_weights()
            .into_iter()
            .map(|(st, w)| (st.as_str().to_string(), w as f32))
            .collect()
    }
}

/// Fetch deterministic section weights for `profile_id` via `brain.profile`.
///
/// `brain.profile` computes `derive_deterministic_weights` and embeds the per-section
/// `weight` field in `section_posteriors` — extract it directly rather than
/// reconstructing a `SectionPosteriorState` for read-only scoring.
///
/// Returns `None` when the brain pack is absent, the profile is not found,
/// or `section_posteriors` is missing from the response.
async fn load_profile_type_weights(
    registry: &VerbRegistry,
    profile_id: &str,
    token: &NamespaceToken,
) -> Option<HashMap<String, f32>> {
    let namespace = token.namespace().as_str();
    let result = registry
        .dispatch_with_identity(
            "brain.profile",
            json!({ "profile_id": profile_id, "namespace": namespace }),
            Some(RequestIdentity::from_token(token)),
        )
        .await
        .ok()?;
    let sections = result.get("section_posteriors")?.as_object()?;
    let weights: HashMap<String, f32> = sections
        .iter()
        .filter_map(|(name, val)| {
            let w = val.get("weight")?.as_f64()? as f32;
            Some((name.clone(), w))
        })
        .collect();
    if weights.is_empty() {
        None
    } else {
        Some(weights)
    }
}

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;
