use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ActorRef, GateDecision, Obligation};

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
    /// Original request parser position, not completion order. Unknown outside
    /// a composed-request scope and in historical envelopes.
    #[serde(default)]
    pub op_index: Option<u32>,
    /// Reference provenance; absent together with `op_index` when unknown.
    #[serde(default)]
    pub ref_resolution: Option<khive_types::RefResolution>,
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
    /// Attach provenance established by a request runner. A direct gate
    /// consultation without that scope explicitly remains unattributed.
    pub fn with_operation_attribution(
        mut self,
        operation: Option<khive_types::OperationAttribution>,
    ) -> Self {
        self.op_index = operation.map(|operation| operation.op_index);
        self.ref_resolution = operation.map(|operation| operation.ref_resolution);
        self
    }

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
            op_index: None,
            ref_resolution: None,
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
            op_index: None,
            ref_resolution: None,
        }
    }
}

#[cfg(test)]
mod operation_tests {
    use super::*;
    use khive_types::{OperationAttribution, RefResolution};

    #[test]
    fn audit_operation_fields_are_closed_and_legacy_absence_stays_unknown() {
        let request = crate::GateRequest::new(
            crate::ActorRef::anonymous(),
            khive_types::Namespace::local(),
            "get",
            serde_json::json!({}),
        );
        let direct = AuditEvent::from_check(
            &request,
            &GateDecision::Allow {
                obligations: vec![],
            },
            "test",
        );
        assert_eq!((direct.op_index, direct.ref_resolution), (None, None));
        let mut old = serde_json::to_value(&direct).unwrap();
        old.as_object_mut().unwrap().remove("op_index");
        old.as_object_mut().unwrap().remove("ref_resolution");
        let legacy: AuditEvent = serde_json::from_value(old).unwrap();
        assert_eq!((legacy.op_index, legacy.ref_resolution), (None, None));

        let attributed = direct.with_operation_attribution(Some(OperationAttribution {
            op_index: 3,
            ref_resolution: RefResolution::Resolved,
        }));
        let mut value = serde_json::to_value(attributed).unwrap();
        assert_eq!(value["op_index"], 3);
        assert_eq!(value["ref_resolution"], "resolved");
        value["ref_resolution"] = serde_json::json!("unknown");
        assert!(serde_json::from_value::<AuditEvent>(value).is_err());
    }
}
