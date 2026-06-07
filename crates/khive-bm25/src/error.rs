//! Error types for the BM25 index.

use thiserror::Error;

/// Classification of errors by recoverability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Retrying will not help (e.g., budget exceeded, invalid config).
    Permanent,
    /// Retrying after a delay may succeed.
    Transient,
}

/// Errors produced by BM25 index operations.
#[derive(Debug, Error)]
pub enum RetrievalError {
    /// The memory budget was exceeded when indexing a new document.
    #[error(
        "memory budget exceeded: current={current_usage}, item_size={item_size}, limit={limit}"
    )]
    BudgetExceeded {
        current_usage: usize,
        item_size: usize,
        limit: usize,
    },

    /// Invalid BM25 configuration parameters.
    #[error("configuration error: {0}")]
    Configuration(String),

    /// Internal document ID space exhausted (u32::MAX documents indexed).
    #[error("internal document ID space exhausted (u32::MAX reached)")]
    IdSpaceExhausted,
}

impl RetrievalError {
    /// Construct a `BudgetExceeded` error.
    pub fn budget_exceeded(current_usage: usize, item_size: usize, limit: usize) -> Self {
        Self::BudgetExceeded {
            current_usage,
            item_size,
            limit,
        }
    }

    /// Return the [`ErrorKind`] for this error.
    pub fn kind(&self) -> ErrorKind {
        ErrorKind::Permanent
    }

    /// Whether retrying this operation might succeed.
    pub fn is_retryable(&self) -> bool {
        self.kind() == ErrorKind::Transient
    }
}

/// Convenience `Result` alias for BM25 operations.
pub type Result<T> = std::result::Result<T, RetrievalError>;
