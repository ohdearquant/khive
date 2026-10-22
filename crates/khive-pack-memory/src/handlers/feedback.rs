//! Handler for `memory.feedback` — explicit recall-domain feedback.

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{NamespaceToken, RequestIdentity, RuntimeError, VerbRegistry};

use crate::recall_feedback::on_explicit_feedback;
use crate::MemoryPack;

#[derive(Debug, Deserialize)]
struct FeedbackParams {
    target_id: String,
    signal: String,
}

impl MemoryPack {
    /// Route validated feedback through explicit profile, bound profile, then global state.
    /// See `crates/khive-pack-memory/docs/api/memory-lifecycle.md`.
    pub(crate) async fn handle_feedback(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: FeedbackParams = serde_json::from_value(params).map_err(|e| {
            RuntimeError::InvalidInput(format!("memory.feedback: invalid params: {e}"))
        })?;
        validate_feedback_signal(&p.signal)?;

        let target_id = p.target_id.parse::<Uuid>().map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "memory.feedback: target_id must be a full UUID because a short-prefix \
                 resolution can miss or be ambiguous, while feedback must identify one exact \
                 recalled record; got {:?}",
                p.target_id
            ))
        })?;
        let canonical_target_id = target_id.as_hyphenated().to_string();

        // Tiers 1-2: explicit config profile, then namespace-bound profile via
        // brain.resolve(consumer_kind="recall") — shared with memory.recall's
        // serve-time stamp so the two resolution paths cannot drift apart.
        if let Some(profile_id) =
            super::common::resolve_serving_profile(&self.brain_profile, token, registry).await
        {
            return route_to_brain(
                registry,
                token,
                &canonical_target_id,
                &p.signal,
                &profile_id,
            )
            .await;
        }

        // Tier 3: global tuning prior (original behavior).
        if let Ok(mut state) = self.recall_state.lock() {
            on_explicit_feedback(&mut state, target_id, &p.signal);
        }

        Ok(json!({
            "ok": true,
            "target_id": canonical_target_id,
            "signal": p.signal,
        }))
    }
}

/// Reject unknown feedback before the low-level tier-three no-op can report success.
fn validate_feedback_signal(signal: &str) -> Result<(), RuntimeError> {
    let semantic = khive_brain_core::FeedbackEventKind::from_signal_str(signal).is_some();
    let legacy = serde_json::from_value::<khive_brain_core::FeedbackSignal>(json!(signal)).is_ok();
    if semantic || legacy {
        Ok(())
    } else {
        Err(RuntimeError::InvalidInput(format!(
            "memory.feedback: invalid signal {:?}; valid: useful | not_useful | wrong | \
             explicit_positive | explicit_negative | implicit_positive | implicit_negative | correction",
            mask_for_redaction_surface(RedactionSurface::GateProbe, signal)
        )))
    }
}

/// Route feedback to a known profile, returning any brain-pack error unchanged.
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
    registry
        .dispatch_with_identity(
            "brain.feedback",
            brain_params,
            Some(RequestIdentity::from_token(token)),
        )
        .await
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "feedback_tests.rs"]
mod tests;
