use thiserror::Error;

// ---------- Validation error ----------

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
    #[error("deny reason must not be empty")]
    EmptyDenyReason,
    #[error("audit tag must not be empty")]
    EmptyAuditTag,
    #[error("rate limit window_secs must be > 0")]
    ZeroRateLimitWindow,
    #[error("rate limit max must be > 0")]
    ZeroRateLimitMax,
}

// ---------- Error ----------

/// Errors returned by [`Gate::check`].
#[derive(Error, Debug)]
pub enum GateError {
    #[error("policy error: {0}")]
    Policy(String),
    #[error("evaluation error: {0}")]
    Evaluation(String),
    #[error("internal gate error: {0}")]
    Internal(String),
    #[error("validation error: {0}")]
    Validation(#[from] GateValidationError),
}
