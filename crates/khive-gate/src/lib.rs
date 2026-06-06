//! khive-gate — pluggable authorization gate for verb dispatch.
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
pub use error::GateError;
pub use gate::{AllowAllGate, Gate, GateRef};
pub use obligation::Obligation;
pub use request::GateRequest;

#[cfg(test)]
mod tests;
