//! khive-gate — pluggable authorization gate for verb dispatch.
//!
//! The runtime consults a [`Gate`] impl before dispatching each verb. The default
//! [`AllowAllGate`] is permissive (suitable for personal/local deployments). For
//! production policy enforcement, plug a Rego-backed or capability-witness-backed
//! impl into `RuntimeConfig.gate`.
//!
//! # Validation
//!
//! Public wire types ([`ActorRef`], [`GateRequest`], [`Obligation`]) validate
//! invariants at construction and deserialization boundaries. Empty actor
//! kind/id, empty verb, empty deny reason, and zero rate-limit values are
//! rejected with [`GateValidationError`]. See the ADR-018 design contract.
//!
//! See `docs/gate.md` for wire shapes, quick-start example, and design context.

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
