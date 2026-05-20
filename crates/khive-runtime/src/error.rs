//! Runtime error types.

use std::fmt;

use thiserror::Error;

pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// A single missing pack dependency (ADR-037).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingPackDependency {
    pub from: String,
    pub requires: String,
}

impl fmt::Display for MissingPackDependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pack '{}' requires '{}', but '{}' is not in the loaded pack set",
            self.from, self.requires, self.requires
        )
    }
}

impl std::error::Error for MissingPackDependency {}

/// Multiple missing pack dependencies collected into one error (ADR-037).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingPackDependencies {
    pub missing: Vec<MissingPackDependency>,
}

impl fmt::Display for MissingPackDependencies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.missing.iter().map(ToString::to_string).collect();
        write!(f, "{}", parts.join("; "))
    }
}

impl std::error::Error for MissingPackDependencies {}

/// Circular pack dependency detected during topological sort (ADR-037).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircularPackDependency {
    pub cycle: Vec<String>,
}

impl fmt::Display for CircularPackDependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "circular dependency detected among packs: {}",
            self.cycle.join(" -> ")
        )
    }
}

impl std::error::Error for CircularPackDependency {}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("storage: {0}")]
    Storage(#[from] khive_storage::StorageError),

    #[error("sqlite: {0}")]
    Sqlite(#[from] khive_db::SqliteError),

    #[error("query: {0}")]
    Query(#[from] khive_query::QueryError),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("unconfigured: {0} is not set")]
    Unconfigured(String),

    #[error("embedding: {0}")]
    Embedding(#[from] lattice_embed::EmbedError),

    #[error("ambiguous: {0}")]
    Ambiguous(String),

    #[error("internal: {0}")]
    Internal(String),

    #[error("missing pack dependency: {0}")]
    MissingPackDependency(MissingPackDependency),

    #[error("missing pack dependencies: {0}")]
    MissingPackDependencies(MissingPackDependencies),

    #[error("{0}")]
    CircularPackDependency(CircularPackDependency),

    /// Gate denied this verb invocation (ADR-035).
    ///
    /// Returned by `VerbRegistry::dispatch` when the configured `Gate` returns
    /// `GateDecision::Deny`. The pack is never invoked. The `reason` field
    /// carries the deny message produced by the gate implementation.
    #[error("permission denied for verb {verb:?}: {reason}")]
    PermissionDenied { verb: String, reason: String },

    /// A structured [`khive_types::KhiveError`] converted into the runtime
    /// layer. The full structured error is preserved so callers can inspect
    /// `kind`, `code`, `details`, and `retry_hint` without information loss.
    #[error("{0}")]
    Khive(khive_types::KhiveError),
}

impl From<khive_types::KhiveError> for RuntimeError {
    fn from(e: khive_types::KhiveError) -> Self {
        Self::Khive(e)
    }
}
