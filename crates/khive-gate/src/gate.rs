use std::sync::Arc;

use crate::{GateDecision, GateError, GateRequest};

// ---------- Trait ----------

/// Authorization gate consulted before each verb dispatch.
///
/// Implementations live downstream:
/// - `AllowAllGate` (this crate) — permissive default
/// - `RegoGate` (Apache-2.0 sibling crate `khive-gate-rego`) — regorus-backed Rego eval
/// - `LionGate<G>` (khive-cloud, BUSL) — wraps any `Gate` with lion-core
///   capability witnesses for verifiable enforcement.
pub trait Gate: Send + Sync + std::fmt::Debug {
    /// Evaluates the authorization policy for `req` and returns a decision.
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError>;

    /// Short name of this backend — surfaced in audit events so downstream
    /// tooling can tell `RegoGate` results apart from `LionGate<RegoGate>`
    /// results without parsing the type.
    ///
    /// Defaults to `std::any::type_name::<Self>()`.
    fn impl_name(&self) -> &'static str {
        std::any::type_name::<Self>()
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
