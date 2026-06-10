//! Handler for `memory.feedback` — explicit recall-domain feedback.

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};

use crate::recall_feedback::on_explicit_feedback;
use crate::MemoryPack;

#[derive(Debug, Deserialize)]
struct FeedbackParams {
    target_id: String,
    signal: String,
}

impl MemoryPack {
    /// Handle `memory.feedback` with 3-tier profile resolution (ADR-035).
    ///
    /// Resolution order:
    /// 1. Explicit brain profile in pack config (`self.brain_profile`) → route via `brain.feedback`
    /// 2. Namespace-bound profile resolved via `brain.resolve` → route via `brain.feedback`
    /// 3. Global tuning prior → update `self.recall_state` directly (original behavior)
    pub(crate) async fn handle_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: FeedbackParams = serde_json::from_value(params).map_err(|e| {
            RuntimeError::InvalidInput(format!("memory.feedback: invalid params: {e}"))
        })?;

        let target_id = p.target_id.parse::<Uuid>().map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "memory.feedback: target_id {:?} is not a valid UUID",
                p.target_id
            ))
        })?;

        // Tier 1: explicit profile from config.
        if let Some(ref profile_id) = self.brain_profile {
            return route_to_brain(registry, token, &p.target_id, &p.signal, profile_id).await;
        }

        // Tier 2: namespace-bound profile via brain.resolve.
        // Use consumer_kind="recall" — the brain contract keys recall bindings/defaults
        // under "recall" (brain.resolve(consumer_kind="recall") returns balanced-recall-v1).
        let ns = token.namespace().as_str().to_string();
        if let Some(profile_id) = resolve_namespace_profile(registry, &ns, "recall").await {
            return route_to_brain(registry, token, &p.target_id, &p.signal, &profile_id).await;
        }

        // Tier 3: global tuning prior (original behavior).
        if let Ok(mut state) = self.recall_state.lock() {
            on_explicit_feedback(&mut state, target_id, &p.signal);
        }

        Ok(json!({ "ok": true, "target_id": p.target_id, "signal": p.signal }))
    }
}

/// Route feedback to `brain.feedback` for a known profile ID.
///
/// Returns the brain.feedback result on success, or an error if the brain pack
/// rejects the call (e.g. unknown profile). Callers that want graceful fallback
/// should handle the error themselves.
async fn route_to_brain(
    registry: &VerbRegistry,
    token: &NamespaceToken,
    target_id: &str,
    signal: &str,
    profile_id: &str,
) -> Result<Value, RuntimeError> {
    // Include `namespace` so the registry mints the correct NamespaceToken for
    // the brain pack — the registry strips it before forwarding to the handler
    // since brain.feedback does not declare `namespace` as a param.
    let brain_params = json!({
        "namespace": token.namespace().as_str(),
        "target_id": target_id,
        "signal": signal,
        "served_by_profile_id": profile_id,
    });
    registry.dispatch("brain.feedback", brain_params).await
}

/// Try to resolve the profile bound to `namespace` for `consumer_kind` via
/// `brain.resolve`. Returns `None` when the brain pack is absent, the verb
/// errors, no binding matches, or the result is only a system-default fallback
/// (`matched_binding = false`).
///
/// Per ADR-035, tier-2 fires only on a real binding match. A system-default
/// fallback (e.g. `balanced-recall-v1` active with no explicit binding) must
/// fall through to tier-3 (pack-local global prior).
async fn resolve_namespace_profile(
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use khive_pack_kg::KgPack;
    use khive_runtime::{Namespace, RuntimeConfig, VerbRegistryBuilder};

    fn build_memory_rt(brain_profile: Option<String>) -> khive_runtime::KhiveRuntime {
        let tmp = tempfile::Builder::new()
            .prefix("khive-mem-feedback-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp dir");
        let db_path = tmp.path().join("khive.db");
        std::mem::forget(tmp);

        khive_runtime::KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "memory".to_string()],
            brain_profile,
            ..RuntimeConfig::default()
        })
        .expect("runtime")
    }

    /// Tier-3: when no brain pack is loaded and no profile is configured, feedback
    /// updates the global prior without error.
    #[tokio::test]
    async fn feedback_falls_through_to_global_prior() {
        let rt = build_memory_rt(None);
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");

        let note_id = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "test feedback note",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(crate::MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.feedback",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "target_id": note_id.id.to_string(),
                    "signal": "useful",
                }),
            )
            .await;

        assert!(result.is_ok(), "feedback must not error: {:?}", result);
        let v = result.unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["signal"], "useful");
    }

    /// Tier-3: not_useful signal flows through global prior path correctly.
    #[tokio::test]
    async fn feedback_global_prior_not_useful() {
        let rt = build_memory_rt(None);
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");

        let note_id = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "not useful note",
                Some(0.5),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(crate::MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let r = registry
            .dispatch(
                "memory.feedback",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "target_id": note_id.id.to_string(),
                    "signal": "not_useful",
                }),
            )
            .await
            .expect("feedback ok");

        assert_eq!(r["ok"], true);
        assert_eq!(r["signal"], "not_useful");
    }

    /// Tier-1: explicit brain_profile config routes to brain.feedback (which errors
    /// if brain pack not loaded — that is the expected contract).
    #[tokio::test]
    async fn feedback_explicit_profile_routes_to_brain() {
        let rt = build_memory_rt(Some("balanced-recall-v1".to_string()));
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");

        let note_id = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "profile routed note",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");

        // No brain pack loaded → brain.feedback not found → error propagates.
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(crate::MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");

        let result = registry
            .dispatch(
                "memory.feedback",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "target_id": note_id.id.to_string(),
                    "signal": "useful",
                }),
            )
            .await;

        // brain.feedback is not registered → should error (verb not found).
        assert!(
            result.is_err(),
            "explicit profile with no brain pack must error, got {:?}",
            result
        );
    }

    // ── Three-tier integration tests (kg + memory + brain all loaded) ──────────
    //
    // These tests verify that feedback resolution respects the full tier order.
    // Each test builds a registry with ALL THREE packs registered and inspects
    // `brain.profile` (total_events) to confirm which profile received credit.

    fn build_full_rt(brain_profile: Option<String>) -> khive_runtime::KhiveRuntime {
        let tmp = tempfile::Builder::new()
            .prefix("khive-mem-3tier-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp dir");
        let db_path = tmp.path().join("khive.db");
        std::mem::forget(tmp);

        khive_runtime::KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "memory".to_string(), "brain".to_string()],
            brain_profile,
            ..RuntimeConfig::default()
        })
        .expect("runtime")
    }

    /// Tier-1 wins over tier-2: when an explicit profile is configured AND a
    /// namespace binding exists for consumer_kind="recall", the explicit profile
    /// receives feedback — not the bound profile.
    #[tokio::test]
    async fn feedback_tier1_explicit_wins_over_bound_profile() {
        use khive_pack_brain::BrainPack;

        let rt = build_full_rt(Some("balanced-recall-v1".to_string()));
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");

        let note_id = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "tier-1 wins note",
                Some(0.8),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(crate::MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        // Create and activate a secondary profile to act as the "bound" tier-2 profile.
        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "alt-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create alt profile");

        registry
            .dispatch(
                "brain.activate",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "alt-recall-v1",
                }),
            )
            .await
            .expect("activate alt profile");

        // Bind alt-recall-v1 for consumer_kind="recall" in the local namespace.
        registry
            .dispatch(
                "brain.bind",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "alt-recall-v1",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind alt profile");

        // Send feedback: tier-1 (explicit brain_profile = "balanced-recall-v1") must win.
        // brain.feedback returns {"emitted": true, ...} when routed through the brain pack.
        let r = registry
            .dispatch(
                "memory.feedback",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "target_id": note_id.id.to_string(),
                    "signal": "useful",
                }),
            )
            .await
            .expect("feedback ok");
        assert_eq!(
            r["emitted"], true,
            "tier-1 feedback must route to brain pack: {r:?}"
        );

        // balanced-recall-v1 must have total_events == 1 (received the credit).
        let default_prof = registry
            .dispatch(
                "brain.profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "balanced-recall-v1",
                }),
            )
            .await
            .expect("brain.profile");
        assert_eq!(
            default_prof["total_events"].as_u64().unwrap_or(0),
            1,
            "tier-1: balanced-recall-v1 must receive the feedback event"
        );

        // alt-recall-v1 must have total_events == 0 (was NOT credited despite binding).
        let alt_prof = registry
            .dispatch(
                "brain.profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "alt-recall-v1",
                }),
            )
            .await
            .expect("brain.profile alt");
        assert_eq!(
            alt_prof["total_events"].as_u64().unwrap_or(0),
            0,
            "tier-1 wins: alt-recall-v1 must NOT receive any events when tier-1 is active"
        );
    }

    /// Tier-2 namespace binding: when no explicit profile is configured but a
    /// namespace binding for consumer_kind="recall" exists, that bound profile
    /// receives feedback.
    ///
    /// This test FAILS before the consumer_kind fix ("memory.recall" → "recall")
    /// and PASSES after it.
    #[tokio::test]
    async fn feedback_tier2_namespace_bound_profile_credited() {
        use khive_pack_brain::BrainPack;

        // No explicit brain_profile — tier-2 must kick in.
        let rt = build_full_rt(None);
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");

        let note_id = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "tier-2 binding note",
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");

        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(crate::MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        // Create a secondary profile and activate it.
        registry
            .dispatch(
                "brain.create_profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "name": "ns-bound-recall",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("create ns-bound profile");

        registry
            .dispatch(
                "brain.activate",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "ns-bound-recall",
                }),
            )
            .await
            .expect("activate ns-bound profile");

        // Bind ns-bound-recall to the "local" namespace for consumer_kind="recall".
        // resolve_namespace_profile calls brain.resolve(namespace="local", consumer_kind="recall"),
        // which must match this binding when the consumer_kind fix is in place.
        registry
            .dispatch(
                "brain.bind",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "ns-bound-recall",
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("bind ns-bound profile");

        // Verify brain.resolve agrees with the expected binding before calling feedback.
        let resolve_result = registry
            .dispatch(
                "brain.resolve",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("brain.resolve");
        assert_eq!(
            resolve_result["resolved_profile_id"],
            serde_json::json!("ns-bound-recall"),
            "brain.resolve must return the namespace-bound profile for consumer_kind=recall"
        );

        // Send feedback — tier-2 must route to ns-bound-recall.
        // brain.feedback returns {"emitted": true, ...} when routed through the brain pack.
        let r = registry
            .dispatch(
                "memory.feedback",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "target_id": note_id.id.to_string(),
                    "signal": "useful",
                }),
            )
            .await
            .expect("feedback ok");
        assert_eq!(
            r["emitted"], true,
            "tier-2 feedback must route to brain pack: {r:?}"
        );

        // ns-bound-recall must have total_events == 1.
        let bound_prof = registry
            .dispatch(
                "brain.profile",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "profile_id": "ns-bound-recall",
                }),
            )
            .await
            .expect("brain.profile ns-bound");
        assert_eq!(
            bound_prof["total_events"].as_u64().unwrap_or(0),
            1,
            "tier-2: namespace-bound profile ns-bound-recall must receive the feedback event"
        );
    }

    /// Tier-3 global fallback: when no explicit profile is configured and no explicit
    /// namespace binding exists, feedback falls through to the pack-local global
    /// tuning prior — even when the brain pack is loaded and balanced-recall-v1 is active.
    ///
    /// Before the matched_binding fix, resolve_namespace_profile treated the system-default
    /// fallback (balanced-recall-v1 active, no binding rows) as a tier-2 hit. This test
    /// verifies that the deactivation workaround is no longer needed: tier-3 fires in
    /// the normal case (brain loaded, no explicit binding).
    #[tokio::test]
    async fn feedback_tier3_global_fallback_with_brain_loaded() {
        use khive_pack_brain::BrainPack;

        // No explicit brain_profile configured.
        let rt = build_full_rt(None);
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns.clone()).expect("token");

        let note_id = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                "tier-3 global fallback note",
                Some(0.6),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");

        // Load brain pack but do NOT create any explicit bindings.
        // brain.resolve returns balanced-recall-v1 as system-default (matched_binding=false).
        // With the fix, resolve_namespace_profile returns None for matched_binding=false,
        // so tier-2 is skipped and tier-3 fires WITHOUT needing to deactivate the default profile.
        let brain = BrainPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(crate::MemoryPack::new(rt.clone()));
        builder.register(brain);
        let registry = builder.build().expect("registry");

        // Confirm brain.resolve returns a system-default (matched_binding=false) — no explicit binding.
        let resolve_result = registry
            .dispatch(
                "brain.resolve",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "consumer_kind": "recall",
                }),
            )
            .await
            .expect("brain.resolve should succeed");
        assert_eq!(
            resolve_result["matched_binding"], false,
            "no explicit binding exists: matched_binding must be false (system default)"
        );

        // Tier-3: feedback must succeed and echo the signal back (ok=true, signal echoed).
        // balanced-recall-v1 is still Active, but tier-2 is skipped due to matched_binding=false.
        let r = registry
            .dispatch(
                "memory.feedback",
                serde_json::json!({
                    "namespace": ns.as_str(),
                    "target_id": note_id.id.to_string(),
                    "signal": "not_useful",
                }),
            )
            .await
            .expect("tier-3 feedback must not error");

        assert_eq!(
            r["ok"], true,
            "tier-3 global path must return ok=true: {r:?}"
        );
        assert_eq!(
            r["signal"], "not_useful",
            "tier-3 path must echo the signal: {r:?}"
        );
        // No brain_profile key in the response (tier-3 does not route to brain).
        assert!(
            r.get("emitted").is_none(),
            "tier-3 path must not produce an emitted key: {r:?}"
        );
    }
}
