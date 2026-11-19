//! Kernel error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("actor not registered: {0}")]
    UnknownActor(String),

    #[error("namespace not accessible: {0}")]
    NamespaceDenied(String),

    #[error("capability insufficient: need {needed}, have {have}")]
    InsufficientRights { needed: String, have: String },

    #[error("capability revoked: {0}")]
    CapabilityRevoked(u128),

    #[error("capability seal is invalid: {0}")]
    InvalidSeal(u128),

    #[error("capability identifier space exhausted")]
    IdSpaceExhausted,

    #[error("delegation failed: {0}")]
    DelegationFailed(String),

    #[error("lion-core error: {0}")]
    Core(String),
}
