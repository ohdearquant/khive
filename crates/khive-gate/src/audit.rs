use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ActorRef, GateDecision, Obligation};

/// How a top-level operation argument entered the resolved dispatch envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentOrigin {
    /// The operation supplied a concrete JSON value.
    Literal,
    /// The value was obtained entirely from a chain `$prev` reference.
    ResolvedReference,
    /// A container combined literal content with one or more `$prev` references.
    Mixed,
}

/// Non-reversible identity for an argument envelope.
///
/// Values are never persisted. The runtime secret-masks a canonical JSON projection before
/// hashing it and exposes only a bounded list of masked top-level keys for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditArgumentIdentity {
    /// BLAKE3 identity of the secret-masked canonical argument envelope.
    pub digest: String,
    /// Sorted, bounded, secret-masked top-level object keys.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Whether additional top-level keys were omitted from `keys`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keys_truncated: bool,
}

/// Structured audit record emitted once per gate consultation.
///
/// JSON field names are stable; events reach tracing and the configured event store. See
/// `crates/khive-gate/docs/api/audit-events.md`.
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
    /// Gate outcome — `"allow"`, `"deny"`, or `"gate_unavailable"`.
    pub decision: AuditDecision,
    /// Deny reason, present only when `decision == "deny"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_reason: Option<String>,
    /// Obligations on allow; always serialized and empty on deny or outage.
    #[serde(default)]
    pub obligations: Vec<Obligation>,
    /// Name of the gate implementation that produced this decision.
    pub gate_impl: String,
    /// Correlation token — `GateContext::session_id` when present, else `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Zero-based operation position within a request group, when supplied by the transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_index: Option<u32>,
    /// Per-top-level-argument literal/substitution provenance from the parsed request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub argument_origins: BTreeMap<String, ArgumentOrigin>,
    /// Identity of the resolved pre-gate argument envelope; values are never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_arguments: Option<AuditArgumentIdentity>,
    /// Identity of the canonical argument envelope consumed by the handler, when dispatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_arguments: Option<AuditArgumentIdentity>,
}

/// The outcome field of an [`AuditEvent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    Allow,
    Deny,
    GateUnavailable,
}

impl AuditEvent {
    /// Project one request/decision pair into a timestamped stable audit envelope.
    ///
    /// See `crates/khive-gate/docs/api/audit-events.md`.
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
            operation_index: None,
            argument_origins: BTreeMap::new(),
            resolved_arguments: None,
            effective_arguments: None,
        }
    }

    /// Project a gate infrastructure failure into the stable audit envelope.
    pub fn gate_unavailable(req: &crate::GateRequest, gate_impl: &str) -> Self {
        Self {
            timestamp: req.context.timestamp.unwrap_or_else(chrono::Utc::now),
            actor: req.actor.clone(),
            namespace: req.namespace.as_str().to_string(),
            verb: req.verb.clone(),
            decision: AuditDecision::GateUnavailable,
            deny_reason: None,
            obligations: Vec::new(),
            gate_impl: gate_impl.to_string(),
            session_id: req.context.session_id.clone(),
            operation_index: None,
            argument_origins: BTreeMap::new(),
            resolved_arguments: None,
            effective_arguments: None,
        }
    }
}
