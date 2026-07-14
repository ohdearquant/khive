use std::sync::Arc;

use crate::{GateDecision, GateError, GateRequest};

/// Authorization gate consulted before each verb dispatch.
///
/// Implementations return policy denials as decisions and infrastructure failures as errors. See
/// `crates/khive-gate/docs/api/gate-evaluation.md`.
pub trait Gate: Send + Sync + std::fmt::Debug {
    /// Evaluate `req`, returning an allow/deny decision or a backend [`GateError`].
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError>;

    /// Return the audit backend name; defaults to `std::any::type_name::<Self>()`.
    fn impl_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// Shareable handle to a `Gate` impl.
pub type GateRef = Arc<dyn Gate>;

/// Permissive gate — every request is allowed with no obligations.
///
/// This runtime default is for trusted local use. See
/// `crates/khive-gate/docs/api/gate-evaluation.md`.
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
