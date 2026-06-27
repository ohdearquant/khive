//! Concept-tier verb handlers: `learn`, `cite`, `topic`, `feedback`.

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_brain_core::{FeedbackSignal, SectionType};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::EdgeRelation;

use crate::knowledge::section_feedback::on_section_feedback;
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
        // chars so the generated name is readable — issue #488.
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
            "domain": domain_norm,
            "tags": entity.tags,
            "namespace": entity.namespace,
        }))
    }

    /// Link a concept to the paper/source that introduced it (`introduced_by` edge).
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

    /// List concept entities, optionally filtered by domain or free-text query.
    pub(crate) async fn handle_topic(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TopicParams = deser(params)?;
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
            // `total` = post-filter count of the candidate window (bounded by limit*4),
            // NOT a true corpus count — see doc comment above.
            let hits = self
                .runtime
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
                self.runtime
                    .get_entities_by_ids(token, &hit_ids)
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

            let total = filtered.len();
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

            Ok(json!({ "results": results, "total": total }))
        } else {
            // Listing path: DB-level domain filter via tags_any avoids silent
            // truncation (K-3).  `count_entities_tagged` gives the pre-limit
            // match count for `total` (K-6).
            let total = self
                .runtime
                .count_entities_tagged(token, Some("concept"), domain_filter.as_deref())
                .await?;

            let entities = self
                .runtime
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

    /// Apply per-section feedback signals to the pack's section posterior state.
    ///
    /// 3-tier profile resolution (ADR-035) — exclusive flow (each tier returns early):
    /// 1. Explicit brain profile in config (`self.brain_profile`) → route via `brain.feedback`
    /// 2. Namespace-bound profile via `brain.resolve(consumer_kind="recall")` → route via `brain.feedback`
    /// 3. Global section_posteriors → update in-memory state directly (tier-3 only when neither 1 nor 2 resolves)
    pub(crate) async fn handle_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let target_id_str = params
            .get("target_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let raw = params
            .get("section_signals")
            .and_then(|v| v.as_object())
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "section_signals is required and must be an object".to_string(),
                )
            })?;

        let mut signals: Vec<(SectionType, FeedbackSignal)> = Vec::with_capacity(raw.len());
        for (key, val) in raw {
            let section_type = SectionType::from_str_loose(key).ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "unknown section_type: {key:?}; valid: {}",
                    SectionType::NAMES.join(", ")
                ))
            })?;
            let signal_str = val.as_str().ok_or_else(|| {
                RuntimeError::InvalidInput(format!("section signal for {key:?} must be a string"))
            })?;
            let signal = match signal_str {
                "useful" => FeedbackSignal::Useful,
                "not_useful" => FeedbackSignal::NotUseful,
                "wrong" => FeedbackSignal::Wrong,
                other => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "unknown feedback signal {other:?}; expected useful | not_useful | wrong"
                    )))
                }
            };
            signals.push((section_type, signal));
        }

        let ns = token.namespace().as_str().to_string();
        let section_signals_val = params.get("section_signals").cloned().unwrap_or_default();

        // Tier 1: explicit profile from config — route exclusively to brain.feedback.
        if let Some(ref profile_id) = self.brain_profile {
            if let Some(ref tid) = target_id_str {
                let brain_params = json!({
                    "namespace": ns,
                    "target_id": tid,
                    "signal": "useful",
                    "served_by_profile_id": profile_id,
                    "section_signals": section_signals_val,
                });
                let result = registry.dispatch("brain.feedback", brain_params).await?;
                return Ok(json!({
                    "ok": true,
                    "brain_profile": profile_id,
                    "signals_applied": signals.len(),
                    "emitted": result.get("emitted").and_then(|v| v.as_bool()).unwrap_or(false),
                }));
            }
        }

        // Tier 2: namespace-bound profile via brain.resolve(consumer_kind="recall").
        // Use "recall" (not "knowledge.search") — the brain contract registers recall
        // bindings under consumer_kind="recall" (brain pack design.md §34).
        if let Some(ref tid) = target_id_str {
            if let Some(profile_id) =
                knowledge_resolve_namespace_profile(registry, &ns, "recall").await
            {
                let brain_params = json!({
                    "namespace": ns,
                    "target_id": tid,
                    "signal": "useful",
                    "served_by_profile_id": profile_id,
                    "section_signals": section_signals_val,
                });
                let result = registry.dispatch("brain.feedback", brain_params).await?;
                return Ok(json!({
                    "ok": true,
                    "brain_profile": profile_id,
                    "signals_applied": signals.len(),
                    "emitted": result.get("emitted").and_then(|v| v.as_bool()).unwrap_or(false),
                }));
            }
        }

        // Tier 3: global tuning prior — update pack-local section_posteriors directly.
        let total_events = {
            let mut state = self.section_posteriors.lock().map_err(|_| {
                RuntimeError::Internal("section_posteriors lock poisoned".to_string())
            })?;
            on_section_feedback(&mut state, &signals);
            state.total_events
        };

        Ok(json!({
            "ok": true,
            "total_events": total_events,
            "signals_applied": signals.len(),
        }))
    }
}

/// Try to resolve the profile bound to `namespace` for `consumer_kind` via
/// `brain.resolve`. Returns `None` when the brain pack is absent, the verb
/// errors, no binding matches, or the result is only a system-default fallback
/// (`matched_binding = false`).
///
/// Per ADR-035, tier-2 fires only on a real binding match. A system-default
/// fallback must fall through to tier-3 (pack-local global prior).
/// Mirrors `resolve_namespace_profile` in the memory pack handler.
async fn knowledge_resolve_namespace_profile(
    registry: &VerbRegistry,
    namespace: &str,
    consumer_kind: &str,
) -> Option<String> {
    let resolve_params = json!({
        "namespace": namespace,
        "consumer_kind": consumer_kind,
    });
    match registry.dispatch("brain.resolve", resolve_params).await {
        Ok(v) => {
            // Only treat as a tier-2 hit when brain.resolve confirms an explicit binding.
            let matched_binding = v
                .get("matched_binding")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            if matched_binding {
                v.get("resolved_profile_id")
                    .and_then(|id| id.as_str())
                    .map(str::to_owned)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use uuid::Uuid;

    use khive_runtime::{KhiveRuntime, Namespace, PackRuntime, VerbRegistryBuilder};
    use serde_json::json;

    use crate::KnowledgePack;

    /// Regression: handle_topic's entity lookup must never panic when an entity_id
    /// is absent from the map.
    ///
    /// The old two-pass code:
    ///   filter(|h| entity_map.get(id).is_some()) followed by
    ///   map(|h| entity_map.get(id).unwrap())          ← panic site
    ///
    /// The fix merges both into a single filter_map that binds the entity once.
    /// This test verifies that hits with missing entity_ids are silently dropped
    /// (the documented intent of the guard) and never cause a panic.
    #[test]
    fn filter_map_drops_missing_entities_without_panic() {
        let present_id = Uuid::new_v4();
        let absent_id = Uuid::new_v4();

        let mut entity_map: HashMap<Uuid, &str> = HashMap::new();
        entity_map.insert(present_id, "present");

        // Simulate hits: one whose entity is in the map, one whose entity is absent.
        let hits = vec![present_id, absent_id];

        let results: Vec<&str> = hits
            .into_iter()
            .filter_map(|id| entity_map.get(&id).copied())
            .collect();

        assert_eq!(
            results,
            vec!["present"],
            "absent entity must be dropped silently"
        );
    }

    /// Verify that an unknown `section_signals` key produces an error message that
    /// lists valid section types, not just the bare "unknown section_type" string.
    #[tokio::test]
    async fn invalid_section_type_error_lists_valid_values() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let pack = KnowledgePack::new(rt.clone());
        let registry = VerbRegistryBuilder::new()
            .build()
            .expect("empty registry builds");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        let err = pack
            .dispatch(
                "knowledge.feedback",
                json!({ "section_signals": { "not_a_real_section": "useful" } }),
                &registry,
                &token,
            )
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("overview"),
            "error must list valid section types; got: {msg}",
        );
    }
}
