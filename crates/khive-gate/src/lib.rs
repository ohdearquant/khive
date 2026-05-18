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
//!     Namespace::default_ns(),
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
    Audit { tag: String },
    RateLimit { window_secs: u64, max: u32 },
    Custom(serde_json::Value),
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
/// - `RegoGate` (planned, behind a feature flag) — regorus-backed Rego eval
/// - `LionGate<G>` (khive-cloud, BUSL) — wraps any `Gate` with lion-core
///   capability witnesses for verifiable enforcement.
pub trait Gate: Send + Sync + std::fmt::Debug {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError>;

    /// Short name of this backend — surfaced in audit events (ADR-032 planned)
    /// so downstream tooling can tell `RegoGate` results apart from
    /// `LionGate<RegoGate>` results without parsing the type.
    fn impl_name(&self) -> &'static str {
        "Gate"
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
            Namespace::default_ns(),
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
}
