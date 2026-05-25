//! khive-gate — pluggable authorization gate for verb dispatch.
//!
//! The runtime consults a `Gate` impl before dispatching each verb. The default
//! `AllowAllGate` is permissive (suitable for personal/local deployments). For
//! production policy enforcement, plug a Rego-backed or capability-witness-backed
//! impl into `RuntimeConfig.gate`.
//!
//! # Quick start
//!
//! ```
//! use std::sync::Arc;
//! use khive_gate::{AllowAllGate, Gate, GateRef, GateRequest, ActorRef};
//! use khive_types::Namespace;
//! use serde_json::json;
//!
//! let gate: GateRef = Arc::new(AllowAllGate);
//! let req = GateRequest::new(
//!     ActorRef::anonymous(),
//!     Namespace::local(),
//!     "search",
//!     json!({"query": "LoRA"}),
//! );
//! assert!(gate.check(&req).unwrap().is_allow());
//! ```

use std::sync::Arc;

use chrono::{DateTime, Utc};
use khive_types::Namespace;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------- Actor ----------

/// Caller identity. `kind` distinguishes user vs agent vs lambda etc.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActorRef {
    pub kind: String,
    pub id: String,
}

impl ActorRef {
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
        }
    }

    /// The implicit caller for unauthenticated local usage.
    pub fn anonymous() -> Self {
        Self {
            kind: "anonymous".into(),
            id: "local".into(),
        }
    }
}

// ---------- Context ----------

/// Per-request context — session, timing, transport source.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GateContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

// ---------- Request ----------

/// What the gate sees on every verb invocation.
///
/// The JSON projection of this struct is the input shape policies receive
/// (e.g. Rego's `input.actor`, `input.verb`, `input.args`). The shape is a
/// public contract — changing field names is a breaking change.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateRequest {
    pub actor: ActorRef,
    pub namespace: Namespace,
    pub verb: String,
    pub args: serde_json::Value,
    #[serde(default)]
    pub context: GateContext,
}

impl GateRequest {
    pub fn new(
        actor: ActorRef,
        namespace: Namespace,
        verb: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self {
            actor,
            namespace,
            verb: verb.into(),
            args,
            context: GateContext::default(),
        }
    }

    pub fn with_context(mut self, context: GateContext) -> Self {
        self.context = context;
        self
    }
}

// ---------- Obligation ----------

/// Side-effects a policy may attach to an `Allow` decision.
///
/// v0 obligations are **advisory** — the dispatcher SHOULD log them but is
/// not required to enforce. Enforcement (real rate limiting, hard audit
/// writes) is a follow-up.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Obligation {
    Audit {
        tag: String,
    },
    RateLimit {
        window_secs: u64,
        max: u32,
    },
    /// Escape hatch for policy-specific obligations. `value` accepts ARBITRARY
    /// JSON (objects, arrays, scalars, null) — the struct-like variant shape
    /// is required because serde's internally-tagged enums cannot merge the
    /// `kind` discriminator into a non-object newtype payload.
    Custom {
        value: serde_json::Value,
    },
}

// ---------- Decision ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GateDecision {
    Allow {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        obligations: Vec<Obligation>,
    },
    Deny {
        reason: String,
    },
}

impl GateDecision {
    pub fn allow() -> Self {
        Self::Allow {
            obligations: Vec::new(),
        }
    }

    pub fn allow_with(obligations: Vec<Obligation>) -> Self {
        Self::Allow { obligations }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

// ---------- Error ----------

#[derive(Error, Debug)]
pub enum GateError {
    #[error("policy error: {0}")]
    Policy(String),
    #[error("evaluation error: {0}")]
    Evaluation(String),
    #[error("internal gate error: {0}")]
    Internal(String),
}

// ---------- Trait ----------

/// Authorization gate consulted before each verb dispatch.
///
/// Implementations live downstream:
/// - `AllowAllGate` (this crate) — permissive default
/// - `RegoGate` (Apache-2.0 sibling crate `khive-gate-rego`, ADR-032) —
///   regorus-backed Rego eval
/// - `LionGate<G>` (khive-cloud, BUSL) — wraps any `Gate` with lion-core
///   capability witnesses for verifiable enforcement.
pub trait Gate: Send + Sync + std::fmt::Debug {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError>;

    /// Short name of this backend — surfaced in audit events (ADR-033) so
    /// downstream tooling can tell `RegoGate` results apart from
    /// `LionGate<RegoGate>` results without parsing the type.
    fn impl_name(&self) -> &'static str {
        "Gate"
    }
}

// ---------- Audit event (ADR-033) ----------

/// Structured audit record emitted once per gate consultation (ADR-033).
///
/// The JSON projection of this struct is the **public contract** — field names
/// are stable. Adding fields is non-breaking; removing or renaming requires a
/// new ADR.
///
/// In v0.2 events are emitted via `tracing::info!` as structured JSON. The
/// `EventStore` write path is deferred to v0.3 when the `VerbRegistry` gains
/// a runtime handle (see ADR-033 §"Implementation Status").
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
    pub fn from_check(req: &GateRequest, decision: &GateDecision, gate_impl: &str) -> Self {
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

/// Shareable handle to a `Gate` impl.
pub type GateRef = Arc<dyn Gate>;

// ---------- Default impl ----------

/// Permissive gate — every request is allowed with no obligations.
///
/// This is the runtime default. Replace it in `RuntimeConfig.gate` for any
/// deployment that needs real authorization.
#[derive(Clone, Debug, Default)]
pub struct AllowAllGate;

impl Gate for AllowAllGate {
    fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::allow())
    }

    fn impl_name(&self) -> &'static str {
        "AllowAllGate"
    }
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_request() -> GateRequest {
        GateRequest::new(
            ActorRef::anonymous(),
            Namespace::local(),
            "search",
            json!({"query": "LoRA"}),
        )
    }

    #[test]
    fn allow_all_gate_allows() {
        let gate = AllowAllGate;
        let decision = gate.check(&sample_request()).unwrap();
        assert!(decision.is_allow());
    }

    #[test]
    fn allow_all_gate_through_dyn() {
        let gate: GateRef = Arc::new(AllowAllGate);
        let decision = gate.check(&sample_request()).unwrap();
        assert!(decision.is_allow());
    }

    #[test]
    fn actor_ref_anonymous() {
        let a = ActorRef::anonymous();
        assert_eq!(a.kind, "anonymous");
        assert_eq!(a.id, "local");
    }

    #[test]
    fn decision_helpers() {
        assert!(GateDecision::allow().is_allow());
        assert!(!GateDecision::deny("nope").is_allow());
    }

    #[test]
    fn request_serializes_to_stable_shape() {
        let req = sample_request();
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["actor"]["kind"], "anonymous");
        assert_eq!(v["actor"]["id"], "local");
        assert_eq!(v["namespace"], "local");
        assert_eq!(v["verb"], "search");
        assert_eq!(v["args"]["query"], "LoRA");
    }

    #[test]
    fn decision_roundtrips_through_json() {
        let allow = GateDecision::allow_with(vec![Obligation::Audit {
            tag: "search.attempt".into(),
        }]);
        let s = serde_json::to_string(&allow).unwrap();
        let back: GateDecision = serde_json::from_str(&s).unwrap();
        match back {
            GateDecision::Allow { obligations } => {
                assert_eq!(obligations.len(), 1);
                match &obligations[0] {
                    Obligation::Audit { tag } => assert_eq!(tag, "search.attempt"),
                    _ => panic!("expected Audit"),
                }
            }
            _ => panic!("expected Allow"),
        }

        let deny = GateDecision::deny("forbidden");
        let s = serde_json::to_string(&deny).unwrap();
        let back: GateDecision = serde_json::from_str(&s).unwrap();
        match back {
            GateDecision::Deny { reason } => assert_eq!(reason, "forbidden"),
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn obligation_rate_limit_serializes_with_kind_tag() {
        let o = Obligation::RateLimit {
            window_secs: 60,
            max: 100,
        };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["kind"], "rate_limit");
        assert_eq!(v["window_secs"], 60);
        assert_eq!(v["max"], 100);
    }

    // `Obligation::Custom` must carry arbitrary JSON per ADR-029. The
    // struct-like variant shape is mandatory here because an internally-tagged
    // newtype variant cannot merge the `kind` discriminator into a non-object
    // payload — a previous newtype shape failed for scalar/array values at
    // runtime instead of compile time, exactly the foot-gun this guards.
    fn assert_custom_round_trips(value: serde_json::Value) {
        let original = Obligation::Custom {
            value: value.clone(),
        };
        let json = serde_json::to_value(&original).expect("serialize");
        assert_eq!(json["kind"], "custom");
        assert_eq!(json["value"], value);
        let back: Obligation = serde_json::from_value(json).expect("deserialize");
        match back {
            Obligation::Custom { value: got } => assert_eq!(got, value),
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn obligation_custom_round_trips_object() {
        assert_custom_round_trips(serde_json::json!({"audit_tag": "billing", "weight": 1.5}));
    }

    #[test]
    fn obligation_custom_round_trips_string() {
        assert_custom_round_trips(serde_json::json!("just a string"));
    }

    #[test]
    fn obligation_custom_round_trips_number() {
        assert_custom_round_trips(serde_json::json!(42));
    }

    #[test]
    fn obligation_custom_round_trips_array() {
        assert_custom_round_trips(serde_json::json!(["a", "b", 3]));
    }

    #[test]
    fn obligation_custom_round_trips_null() {
        assert_custom_round_trips(serde_json::Value::Null);
    }

    #[test]
    fn obligation_custom_round_trips_bool() {
        assert_custom_round_trips(serde_json::json!(true));
    }

    // ---- AuditEvent (ADR-033) ----

    fn sample_req_with_session() -> GateRequest {
        GateRequest::new(
            ActorRef::new("user", "ocean"),
            Namespace::local(),
            "create",
            json!({"kind": "concept"}),
        )
        .with_context(GateContext {
            session_id: Some("sess-abc".into()),
            timestamp: None,
            source: Some("mcp".into()),
        })
    }

    #[test]
    fn audit_event_roundtrips_through_serde_stable_shape() {
        let req = sample_req_with_session();
        let decision = GateDecision::allow_with(vec![Obligation::Audit {
            tag: "create.attempt".into(),
        }]);
        let ev = AuditEvent::from_check(&req, &decision, "AllowAllGate");

        let json = serde_json::to_value(&ev).unwrap();

        // All required fields present with correct values.
        assert_eq!(json["actor"]["kind"], "user");
        assert_eq!(json["actor"]["id"], "ocean");
        assert_eq!(json["namespace"], "local");
        assert_eq!(json["verb"], "create");
        assert_eq!(json["decision"], "allow");
        assert_eq!(json["gate_impl"], "AllowAllGate");
        assert_eq!(json["session_id"], "sess-abc");
        // deny_reason absent on Allow.
        assert!(json.get("deny_reason").is_none() || json["deny_reason"].is_null());
        // obligations populated.
        assert_eq!(json["obligations"][0]["kind"], "audit");
        assert_eq!(json["obligations"][0]["tag"], "create.attempt");
        // timestamp present and non-null.
        assert!(json["timestamp"].is_string());

        // Full round-trip.
        let back: AuditEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back.verb, "create");
        assert_eq!(back.decision, AuditDecision::Allow);
        assert!(back.deny_reason.is_none());
        assert_eq!(back.obligations.len(), 1);
    }

    #[test]
    fn audit_event_deny_path_carries_reason() {
        let req = sample_request(); // anonymous, no session
        let decision = GateDecision::deny("forbidden: no write for anonymous");
        let ev = AuditEvent::from_check(&req, &decision, "RegoGate");

        let json = serde_json::to_value(&ev).unwrap();

        assert_eq!(json["decision"], "deny");
        assert_eq!(json["deny_reason"], "forbidden: no write for anonymous");
        assert_eq!(json["gate_impl"], "RegoGate");
        // obligations is always present on the wire, empty on Deny.
        assert_eq!(
            json["obligations"],
            serde_json::Value::Array(Vec::new()),
            "obligations must be an empty array on Deny, not omitted"
        );
        // session_id absent when not in context.
        assert!(json.get("session_id").is_none() || json["session_id"].is_null());
    }

    #[test]
    fn audit_event_allow_no_obligations() {
        let req = sample_request();
        let decision = GateDecision::allow();
        let ev = AuditEvent::from_check(&req, &decision, "AllowAllGate");
        assert_eq!(ev.decision, AuditDecision::Allow);
        assert!(ev.deny_reason.is_none());
        assert!(ev.obligations.is_empty());
        // obligations is always present on the wire as an empty array — the
        // public JSON contract does not depend on Rust's `#[serde(default)]`
        // behavior at the consumer side.
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            json["obligations"],
            serde_json::Value::Array(Vec::new()),
            "obligations must serialize as an empty array, not be omitted"
        );
    }

    #[test]
    fn audit_decision_serialises_as_snake_case() {
        let allow = serde_json::to_value(AuditDecision::Allow).unwrap();
        assert_eq!(allow, "allow");
        let deny = serde_json::to_value(AuditDecision::Deny).unwrap();
        assert_eq!(deny, "deny");
    }
}
