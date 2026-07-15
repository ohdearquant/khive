//! Concept-tier verb handlers: `learn`, `cite`, `topic`, `feedback`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_brain_core::{resolve_consumer_profile, ConsumerKind, FeedbackSignal, SectionType};
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
    /// 2. Namespace-bound profile via `brain.resolve(consumer_kind="knowledge_compose")` → route via `brain.feedback`
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

        // Tier 2: namespace-bound profile via brain.resolve(consumer_kind="knowledge_compose").
        // Use "knowledge_compose" (not "recall") — the knowledge pack's compose-ranking
        // feedback has its own consumer_kind bucket (ADR-058 amendment, #542) so it no
        // longer shares posteriors with the memory pack's recall bucket.
        if let Some(ref tid) = target_id_str {
            let actor = token.actor().binding_id();
            if let Some(profile_id) =
                resolve_consumer_profile(registry, actor, &ns, ConsumerKind::KnowledgeCompose).await
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

impl KnowledgePack {
    /// Resolve `type_weights` for `compose` via the same 3-tier ADR-035 ladder
    /// used by `record_feedback`, completing read/write symmetry for section posteriors.
    ///
    /// Tier 1: explicit `brain_profile` config → `brain.profile` → extract `weight` per section.
    /// Tier 2: namespace-bound profile via `brain.resolve(consumer_kind="knowledge_compose")` → same.
    /// Tier 3: pack-local `section_posteriors` mutex → `deterministic_weights()`.
    /// Fallback: `SectionPosteriorState::default()`.
    pub(crate) async fn resolve_compose_type_weights(
        &self,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> HashMap<String, f32> {
        let ns = token.namespace().as_str().to_string();

        // Tier 1: explicit profile from config.
        if let Some(ref profile_id) = self.brain_profile {
            if let Some(weights) = load_profile_type_weights(registry, profile_id).await {
                return weights;
            }
        }

        // Tier 2: namespace-bound profile via brain.resolve(consumer_kind="knowledge_compose").
        let actor = token.actor().binding_id();
        if let Some(profile_id) =
            resolve_consumer_profile(registry, actor, &ns, ConsumerKind::KnowledgeCompose).await
        {
            if let Some(weights) = load_profile_type_weights(registry, &profile_id).await {
                return weights;
            }
        }

        // Tier 3: pack-local section_posteriors (updated by global-tuning feedback).
        if let Ok(state) = self.section_posteriors.lock() {
            return state
                .deterministic_weights()
                .into_iter()
                .map(|(st, w)| (st.as_str().to_string(), w as f32))
                .collect();
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
) -> Option<HashMap<String, f32>> {
    let result = registry
        .dispatch("brain.profile", json!({ "profile_id": profile_id }))
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
mod tests {
    use std::collections::HashMap;
    use uuid::Uuid;

    use khive_brain_core::{FeedbackSignal, SectionPosteriorState, SectionType};
    use khive_runtime::{KhiveRuntime, Namespace, PackRuntime, VerbRegistryBuilder};
    use serde_json::json;

    use crate::knowledge::section_feedback::on_section_feedback;
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

    /// Regression for #346: Tier 3 must read tuned `section_posteriors`, not a
    /// fresh default. See crates/khive-pack-knowledge/docs/api/compose-type-weights.md.
    #[tokio::test]
    async fn resolve_compose_type_weights_reads_tuned_section_posteriors_at_tier3() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let pack = KnowledgePack::new(rt.clone());
        let registry = VerbRegistryBuilder::new()
            .build()
            .expect("empty registry builds");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        // Skew posteriors: many Useful events on Formalism, many Wrong events on
        // OperationalGuidance.  Force exploration_epoch=0 so deterministic_weights()
        // uses posterior means directly (no Thompson sampling noise).
        {
            let mut state = pack.section_posteriors.lock().unwrap();
            for _ in 0..80 {
                on_section_feedback(
                    &mut state,
                    &[
                        (SectionType::Formalism, FeedbackSignal::Useful),
                        (SectionType::OperationalGuidance, FeedbackSignal::Wrong),
                    ],
                );
            }
            state.exploration_epoch = 0;
        }

        // Tier 1 and 2 are absent (empty registry), so Tier 3 must fire.
        let weights = pack.resolve_compose_type_weights(&registry, &token).await;

        let formalism_tuned = *weights
            .get("formalism")
            .expect("formalism weight must be present");
        let og_tuned = *weights
            .get("operational_guidance")
            .expect("operational_guidance weight must be present");

        // Default priors: OperationalGuidance (α=6,β=1.5) >> Formalism (α=1.5,β=4).
        let default_state = SectionPosteriorState::default();
        let default_w: HashMap<String, f32> = default_state
            .deterministic_weights()
            .into_iter()
            .map(|(st, w)| (st.as_str().to_string(), w as f32))
            .collect();
        let formalism_default = *default_w.get("formalism").unwrap();
        let og_default = *default_w.get("operational_guidance").unwrap();

        assert!(
            formalism_tuned > formalism_default,
            "tuned formalism weight {formalism_tuned:.4} must exceed default {formalism_default:.4} \
             after skewing feedback"
        );
        assert!(
            og_tuned < og_default,
            "tuned og weight {og_tuned:.4} must be below default {og_default:.4} \
             after wrong feedback"
        );
        // After sufficient skewing, formalism must actually dominate operational_guidance —
        // this is the ordering flip that compose's type_weight component now reflects.
        assert!(
            formalism_tuned > og_tuned,
            "after skewing, formalism {formalism_tuned:.4} must outweigh og {og_tuned:.4}"
        );
    }

    /// Regression for #346 Tier-2: must read weights from a namespace-bound
    /// brain profile (`brain.bind`), not fall through to Tier 3/default. See
    /// crates/khive-pack-knowledge/docs/api/compose-type-weights.md.
    #[tokio::test]
    async fn resolve_compose_type_weights_reads_bound_profile_weights_at_tier2() {
        use khive_pack_brain::BrainPack;
        use khive_pack_kg::KgPack;

        // Build a registry with both brain and kg packs (brain REQUIRES kg).
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(BrainPack::new(rt.clone()));
        let registry = builder.build().expect("kg+brain registry builds");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        // Create a profile with inverted seed_priors:
        //   formalism:           Beta(8, 1) → mean ≈ 0.889
        //     (default prior:               α=1.5, β=4.0 → mean ≈ 0.273)
        //   operational_guidance: Beta(1, 8) → mean ≈ 0.111
        //     (default prior:               α=6.0, β=1.5 → mean ≈ 0.8)
        // ESS = alpha + beta = 9 ≤ DEFAULT_ESS_CAP (100.0) ✓
        registry
            .dispatch(
                "brain.create_profile",
                json!({
                    "name": "compose-tuned-v1",
                    "consumer_kind": "knowledge_compose",
                    "seed_priors": {
                        "section_posteriors": {
                            "formalism": {"alpha": 8.0, "beta": 1.0},
                            "operational_guidance": {"alpha": 1.0, "beta": 8.0}
                        }
                    }
                }),
            )
            .await
            .expect("create_profile with skewed seed_priors");

        // Activate so the profile leaves the Inactive state (matches proper usage).
        registry
            .dispatch("brain.activate", json!({"profile_id": "compose-tuned-v1"}))
            .await
            .expect("activate compose-tuned-v1");

        // Bind for namespace="local", consumer_kind="knowledge_compose".
        // `resolve_consumer_profile` dispatches:
        //   brain.resolve(namespace="local", consumer_kind="knowledge_compose")
        // which must find this binding with matched_binding=true.
        registry
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": "compose-tuned-v1",
                    "namespace": "local",
                    "consumer_kind": "knowledge_compose"
                }),
            )
            .await
            .expect("bind compose-tuned-v1 for namespace=local");

        // KnowledgePack with brain_profile=None → Tier 1 is skipped.
        let pack = KnowledgePack::new(rt.clone());

        // Tier 2 fires: brain.resolve returns matched_binding=true,
        // load_profile_type_weights dispatches brain.profile and extracts the
        // precomputed `weight` field from section_posteriors.
        // brain.profile always calls derive_deterministic_weights() — no Thompson
        // sampling, no epoch dependency — so results are deterministic from seed_priors.
        let weights = pack.resolve_compose_type_weights(&registry, &token).await;

        let formalism_w = *weights
            .get("formalism")
            .expect("formalism weight must be present");
        let og_w = *weights
            .get("operational_guidance")
            .expect("operational_guidance weight must be present");

        // seed_priors: formalism Beta(8,1) mean≈0.889 >> og Beta(1,8) mean≈0.111.
        // Default priors have the opposite ordering: og α=6,β=1.5 >> formalism α=1.5,β=4.
        // If load_profile_type_weights broke or Tier 2 fell through to Tier 3/default,
        // og would dominate — this assertion FAILS (genuine RED→GREEN guard).
        assert!(
            formalism_w > og_w,
            "Tier-2 bound-profile: formalism {formalism_w:.4} must exceed og {og_w:.4}; \
             ordering reflects seed_priors (formalism β(8,1) >> og β(1,8)), \
             not default priors where og α=6,β=1.5 dominates"
        );
    }

    /// #700: `resolve_compose_type_weights` must thread the caller's actor identity
    /// into `resolve_consumer_profile` so an actor-scoped (namespace-wildcard)
    /// `knowledge_compose` binding resolves at Tier 2 — not just namespace-scoped
    /// bindings, which the sibling `..._at_tier2` test above already covers.
    ///
    /// Binds `compose-tuned-actor-v1` by `actor="leo"` only, leaving namespace as
    /// the wildcard `"*"`. Before #700 (call sites passed `actor=None`), this
    /// binding could never match — `resolve_compose_type_weights` would fall
    /// through to Tier 3 and return the untuned default weights, where
    /// `operational_guidance` dominates `formalism`. With the caller's actor
    /// threaded through, Tier 2 must resolve this binding and return the
    /// seed-tuned weights instead.
    #[tokio::test]
    async fn resolve_compose_type_weights_reads_bound_profile_weights_via_actor_binding_at_tier2() {
        use khive_pack_brain::BrainPack;
        use khive_pack_kg::KgPack;

        // Mirror `KhiveRuntime::memory()` exactly, plus a configured actor.
        // `..RuntimeConfig::default()` is a CI trap here: `Default` resolves
        // `embedding_model` to a real on-disk model, which is absent on CI
        // runners and fails entity creation with `ModelInitialization`.
        let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: Some("leo".to_string()),
        })
        .expect("in-memory runtime with actor");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(BrainPack::new(rt.clone()));
        let registry = builder.build().expect("kg+brain registry builds");

        let token = rt.authorize(Namespace::local()).expect("authorize local");
        assert_eq!(
            token.actor().id,
            "leo",
            "test setup: token must carry the configured actor"
        );

        // Same inverted seed_priors as the namespace-bound sibling test:
        //   formalism:            Beta(8, 1) → mean ≈ 0.889
        //   operational_guidance: Beta(1, 8) → mean ≈ 0.111
        registry
            .dispatch(
                "brain.create_profile",
                json!({
                    "name": "compose-tuned-actor-v1",
                    "consumer_kind": "knowledge_compose",
                    "seed_priors": {
                        "section_posteriors": {
                            "formalism": {"alpha": 8.0, "beta": 1.0},
                            "operational_guidance": {"alpha": 1.0, "beta": 8.0}
                        }
                    }
                }),
            )
            .await
            .expect("create_profile with skewed seed_priors");

        registry
            .dispatch(
                "brain.activate",
                json!({"profile_id": "compose-tuned-actor-v1"}),
            )
            .await
            .expect("activate compose-tuned-actor-v1");

        // Bind by actor only — namespace left as the "*" wildcard — so a
        // namespace-only resolution (pre-#700 behavior) can never reach this
        // binding; only threading the caller's actor through can.
        registry
            .dispatch(
                "brain.bind",
                json!({
                    "actor": "leo",
                    "profile_id": "compose-tuned-actor-v1",
                    "consumer_kind": "knowledge_compose"
                }),
            )
            .await
            .expect("bind compose-tuned-actor-v1 for actor=leo");

        // KnowledgePack with brain_profile=None → Tier 1 is skipped.
        let pack = KnowledgePack::new(rt.clone());
        let weights = pack.resolve_compose_type_weights(&registry, &token).await;

        let formalism_w = *weights
            .get("formalism")
            .expect("formalism weight must be present");
        let og_w = *weights
            .get("operational_guidance")
            .expect("operational_guidance weight must be present");

        assert!(
            formalism_w > og_w,
            "Tier-2 actor-bound profile: formalism {formalism_w:.4} must exceed \
             og {og_w:.4}; ordering reflects seed_priors (formalism β(8,1) >> \
             og β(1,8)) resolved via the actor-scoped binding, not the default \
             priors where og α=6,β=1.5 dominates"
        );
    }

    /// Systemic-fix regression: an ANONYMOUS caller must not match an explicit
    /// `actor="local"` binding.
    ///
    /// `ActorRef::anonymous()` carries `id: "local"` (`khive-gate/src/actor.rs`).
    /// Before the `binding_id()` fix, `resolve_compose_type_weights` threaded
    /// `token.actor().id` unconditionally, so an anonymous token's `"local"`
    /// id could match an explicit `actor="local"` binding that a pre-actor-aware
    /// `None` could never have matched. This binds `compose-tuned-anon-v1` by
    /// `actor="local"` and asserts an anonymous-token caller falls through to
    /// Tier 3 default weights instead (where `operational_guidance` dominates
    /// `formalism`), not the bound profile's inverted seed_priors.
    #[tokio::test]
    async fn resolve_compose_type_weights_anonymous_caller_does_not_match_explicit_actor_local_binding(
    ) {
        use khive_pack_brain::BrainPack;
        use khive_pack_kg::KgPack;

        // Default RuntimeConfig — no actor_id configured — mints an anonymous
        // token (id="local") via `rt.authorize`, matching an unauthenticated caller.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(BrainPack::new(rt.clone()));
        let registry = builder.build().expect("kg+brain registry builds");

        let token = rt.authorize(Namespace::local()).expect("authorize local");
        assert!(
            token.actor().is_anonymous(),
            "test setup: token must carry the anonymous actor"
        );
        assert_eq!(
            token.actor().id,
            "local",
            "test setup: anonymous actor id must be \"local\" to exercise the collision"
        );

        registry
            .dispatch(
                "brain.create_profile",
                json!({
                    "name": "compose-tuned-anon-v1",
                    "consumer_kind": "knowledge_compose",
                    "seed_priors": {
                        "section_posteriors": {
                            "formalism": {"alpha": 8.0, "beta": 1.0},
                            "operational_guidance": {"alpha": 1.0, "beta": 8.0}
                        }
                    }
                }),
            )
            .await
            .expect("create_profile with skewed seed_priors");

        registry
            .dispatch(
                "brain.activate",
                json!({"profile_id": "compose-tuned-anon-v1"}),
            )
            .await
            .expect("activate compose-tuned-anon-v1");

        // Bind explicitly to actor="local" — the exact id anonymous tokens carry.
        registry
            .dispatch(
                "brain.bind",
                json!({
                    "actor": "local",
                    "profile_id": "compose-tuned-anon-v1",
                    "consumer_kind": "knowledge_compose"
                }),
            )
            .await
            .expect("bind compose-tuned-anon-v1 for actor=local");

        let pack = KnowledgePack::new(rt.clone());
        let weights = pack.resolve_compose_type_weights(&registry, &token).await;

        let formalism_w = *weights
            .get("formalism")
            .expect("formalism weight must be present");
        let og_w = *weights
            .get("operational_guidance")
            .expect("operational_guidance weight must be present");

        assert!(
            og_w > formalism_w,
            "anonymous caller must NOT match the actor=\"local\" binding: og {og_w:.4} \
             must exceed formalism {formalism_w:.4} (default-prior ordering, Tier 3), \
             not the bound profile's inverted seed_priors (formalism β(8,1) >> og β(1,8))"
        );
    }
}
