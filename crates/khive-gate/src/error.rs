use thiserror::Error;

/// Validation error for gate wire types.
///
/// Returned by `try_new` constructors and custom `Deserialize` impls when
/// invariants are violated (empty fields, zero rate-limit values).
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum GateValidationError {
    #[error("actor kind must not be empty")]
    EmptyActorKind,
    #[error("actor id must not be empty")]
    EmptyActorId,
    #[error("verb must not be empty")]
    EmptyVerb,
    #[error("invalid deny_writes_for configuration: {0}")]
    InvalidWriteDenyPatterns(String),
    #[error("deny reason must not be empty")]
    EmptyDenyReason,
    #[error("audit tag must not be empty")]
    EmptyAuditTag,
    #[error("rate limit window_secs must be > 0")]
    ZeroRateLimitWindow,
    #[error("rate limit max must be > 0")]
    ZeroRateLimitMax,
}

/// Errors returned by [`crate::Gate::check`].
#[derive(Error, Debug)]
pub enum GateError {
    #[error("policy error: {0}")]
    Policy(String),
    #[error("internal gate error: {0}")]
    Internal(String),
    #[error("validation error: {0}")]
    Validation(#[from] GateValidationError),
}

impl GateError {
    /// Stable, caller-safe classification of this error's failure category.
    ///
    /// This is what a `Gate::check` caller is permitted to forward to an
    /// untrusted requester. It never includes this error's `Display` text:
    /// a gate backend's error message can embed connection details,
    /// addresses, or credentials, and that text must stay in server-side
    /// logs only (log the full error separately with its `Display` impl).
    ///
    /// `Internal` is a transient backend-availability failure — safe to
    /// retry. `Policy` and `Validation` are non-transient: the gate backend
    /// is reachable but the request or its configured policy cannot be
    /// evaluated, and retrying the identical request will not change the
    /// outcome.
    pub fn wire_reason(&self) -> &'static str {
        match self {
            Self::Internal(_) => "gate backend unavailable",
            Self::Policy(_) => "gate policy evaluation failed",
            Self::Validation(_) => "gate request validation failed",
        }
    }
}

#[cfg(test)]
#[path = "error_wire_reason_tests.rs"]
mod wire_reason_tests;
