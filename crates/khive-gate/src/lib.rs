//! Validated authorization request, decision, obligation, audit, and gate interfaces.

mod actor;
mod audit;
mod context;
mod decision;
mod error;
mod gate;
mod obligation;
mod request;

pub use actor::ActorRef;
pub use audit::{AuditDecision, AuditEvent};
pub use context::GateContext;
pub use decision::GateDecision;
pub use error::{GateError, GateValidationError};
pub use gate::{AllowAllGate, Gate, GateRef};
pub use obligation::Obligation;
pub use request::GateRequest;

#[cfg(test)]
mod tests;
