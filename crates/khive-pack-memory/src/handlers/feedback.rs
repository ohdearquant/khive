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
        let ns = token.namespace().as_str().to_string();
        if let Some(profile_id) = resolve_namespace_profile(registry, &ns, "memory.recall").await {
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
/// `brain.resolve`. Returns `None` when the brain pack is absent or no
/// binding exists — callers fall through to the next tier.
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
        Ok(v) => v
            .get("resolved_profile_id")
            .and_then(|id| id.as_str())
            .map(str::to_owned),
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
}
