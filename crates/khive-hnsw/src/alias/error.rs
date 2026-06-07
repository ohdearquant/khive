//! Error types for alias operations (creation, swap, drain, validation).

use std::fmt;
use std::time::Duration;

/// Errors from alias manager operations.
#[derive(Debug)]
pub enum AliasError {
    /// The requested alias name does not exist.
    AliasNotFound(String),

    /// The requested collection name does not exist.
    CollectionNotFound(String),

    /// A collection with this name already exists.
    CollectionAlreadyExists(String),

    /// An alias with this name already exists.
    AliasAlreadyExists(String),

    /// The pre-swap validation failed.
    ValidationFailed {
        /// Human-readable reason.
        reason: String,
        /// Recall score achieved (if applicable).
        recall: Option<f32>,
        /// Minimum recall required (if applicable).
        min_recall: Option<f32>,
    },

    /// Drain timed out waiting for active readers to finish.
    DrainTimeout {
        /// How long we waited.
        elapsed: Duration,
        /// Configured timeout.
        timeout: Duration,
        /// Number of readers still active.
        active_readers: u64,
    },

    /// An HNSW operation failed during migration.
    IndexError(String),
}

impl fmt::Display for AliasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AliasNotFound(name) => write!(f, "alias not found: {name}"),
            Self::CollectionNotFound(name) => write!(f, "collection not found: {name}"),
            Self::CollectionAlreadyExists(name) => {
                write!(f, "collection already exists: {name}")
            }
            Self::AliasAlreadyExists(name) => write!(f, "alias already exists: {name}"),
            Self::ValidationFailed {
                reason,
                recall,
                min_recall,
            } => {
                write!(f, "validation failed: {reason}")?;
                if let (Some(r), Some(min)) = (recall, min_recall) {
                    write!(f, " (recall={r:.4}, min={min:.4})")?;
                }
                Ok(())
            }
            Self::DrainTimeout {
                elapsed,
                timeout,
                active_readers,
            } => {
                write!(
                    f,
                    "drain timeout after {:.1}s (limit {:.1}s, {active_readers} readers remaining)",
                    elapsed.as_secs_f64(),
                    timeout.as_secs_f64()
                )
            }
            Self::IndexError(msg) => write!(f, "index error: {msg}"),
        }
    }
}

impl std::error::Error for AliasError {}

impl From<AliasError> for crate::error::RetrievalError {
    fn from(e: AliasError) -> Self {
        match &e {
            AliasError::DrainTimeout { .. } => {
                // Drain timeout is transient -- readers will eventually finish
                crate::error::RetrievalError::QueryTimeout {
                    elapsed_ms: match &e {
                        AliasError::DrainTimeout { elapsed, .. } => elapsed.as_millis() as u64,
                        _ => 0,
                    },
                }
            }
            _ => crate::error::RetrievalError::Hnsw(e.to_string()),
        }
    }
}
