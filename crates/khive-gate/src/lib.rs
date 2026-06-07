//! Pluggable authorization gate for verb dispatch.
//!
//! The runtime consults a [`Gate`] impl before dispatching each verb. The default
//! [`AllowAllGate`] is permissive. For production enforcement, plug a Rego-backed
//! or capability-witness-backed impl into `RuntimeConfig.gate`.
//! Wire types validate invariants at construction and deserialization boundaries.

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
