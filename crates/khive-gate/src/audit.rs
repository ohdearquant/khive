use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ActorRef, GateDecision, Obligation};

// ---------- Audit event ----------

/// Structured audit record emitted once per gate consultation.
///
/// The JSON projection of this struct is the **public contract** — field names
/// are stable. Adding fields is non-breaking; removing or renaming requires a
/// new ADR.
///
/// Events are emitted via `tracing::info!` as structured JSON. When the
/// dispatch registry is configured with an `EventStore`, the same envelope is
/// also persisted as an audit event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Wall-clock timestamp of the gate check (UTC, RFC3339 in JSON).
    pub timestamp: DateTime<Utc>,
    /// Caller identity as given to the gate.
    pub actor: ActorRef,
    /// Namespace in which the verb was invoked.
    pub namespace: String,
    /// Verb being dispatched.
    pub verb: String,
    /// Gate outcome — `"allow"` or `"deny"`.
    pub decision: AuditDecision,
    /// Deny reason, present only when `decision == "deny"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_reason: Option<String>,
    /// Obligations attached by the policy on Allow (empty array on Deny).
    /// Always serialized — `obligations: []` is the wire shape when there
    /// are none, so non-Rust consumers do not need to special-case absence
    /// vs. emptiness.
    #[serde(default)]
    pub obligations: Vec<Obligation>,
    /// Name of the gate implementation that produced this decision.
    pub gate_impl: String,
    /// Correlation token — `GateContext::session_id` when present, else `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// The outcome field of an [`AuditEvent`], serialised as `"allow"` / `"deny"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    Allow,
    Deny,
}

impl AuditEvent {
    /// Build an `AuditEvent` from the gate inputs and output.
    pub fn from_check(req: &crate::GateRequest, decision: &GateDecision, gate_impl: &str) -> Self {
        let (audit_decision, deny_reason, obligations) = match decision {
            GateDecision::Allow { obligations } => {
                (AuditDecision::Allow, None, obligations.clone())
            }
            GateDecision::Deny { reason } => {
                (AuditDecision::Deny, Some(reason.clone()), Vec::new())
            }
        };
        Self {
            timestamp: req.context.timestamp.unwrap_or_else(chrono::Utc::now),
            actor: req.actor.clone(),
            namespace: req.namespace.as_str().to_string(),
            verb: req.verb.clone(),
            decision: audit_decision,
            deny_reason,
            obligations,
            gate_impl: gate_impl.to_string(),
            session_id: req.context.session_id.clone(),
        }
    }
}
