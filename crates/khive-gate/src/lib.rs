//! Validated authorization request, decision, obligation, audit, and gate interfaces.

mod actor;
mod audit;
mod context;
mod decision;
mod enrollment;
mod error;
mod gate;
mod mailbox;
mod obligation;
mod operation;
mod request;

pub use actor::{ActorRef, RUNTIME_STAMPED_ACTOR_KINDS};
pub use audit::{AuditDecision, AuditEvent};
pub use context::GateContext;
pub use decision::GateDecision;
pub use enrollment::CallerEnrollmentGate;
pub use error::{GateError, GateValidationError};
pub use gate::{AllowAllGate, Gate, GateRef};
pub use mailbox::{
    check_with_mailbox_policy, is_valid_mailbox_actor_label, mailbox_read_owner,
    MailboxPolicyError, MailboxReadGate,
};
pub use obligation::Obligation;
pub use operation::{
    classify_operation, OperationAccess, CLASSIFIED_OPERATIONS, OPERATION_CLASSIFIER_VERSION,
};
pub use request::GateRequest;

#[cfg(test)]
mod tests;
