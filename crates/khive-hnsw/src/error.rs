//! Error types for the HNSW crate.

use thiserror::Error;

/// Category of error — used to decide retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Transient — may succeed on retry.
    Transient,
    /// Permanent — retrying will not help.
    Permanent,
}

/// Errors that can occur during HNSW operations.
#[derive(Error, Debug)]
pub enum RetrievalError {
    /// Vector dimension mismatch.
    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch {
        /// Expected dimensionality.
        expected: usize,
        /// Actual dimensionality provided.
        actual: usize,
    },

    /// Memory budget exceeded on insert.
    #[error("memory budget exceeded: current={current_usage}, item={item_size}, limit={limit}")]
    BudgetExceeded {
        /// Current memory usage in bytes.
        current_usage: usize,
        /// Estimated cost of the new item in bytes.
        item_size: usize,
        /// Configured budget in bytes.
        limit: usize,
    },

    /// HNSW index operation error.
    #[error("hnsw error: {0}")]
    Hnsw(String),

    /// Vector contains non-finite values (NaN, Infinity, or -Infinity).
    #[error("non-finite vector: {reason}")]
    NonFiniteVector {
        /// Description of the issue.
        reason: String,
    },

    /// Configuration error.
    #[error("configuration error: {0}")]
    Configuration(String),

    /// Query timed out.
    #[error("query timed out after {elapsed_ms}ms")]
    QueryTimeout {
        /// How long the query ran before timing out.
        elapsed_ms: u64,
    },
}

impl RetrievalError {
    /// Create an HNSW error from any displayable value.
    pub fn hnsw(msg: impl std::fmt::Display) -> Self {
        Self::Hnsw(msg.to_string())
    }

    /// Create a `BudgetExceeded` error.
    pub fn budget_exceeded(current_usage: usize, item_size: usize, limit: usize) -> Self {
        Self::BudgetExceeded {
            current_usage,
            item_size,
            limit,
        }
    }

    /// Return the error kind (Transient vs Permanent).
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::QueryTimeout { .. } => ErrorKind::Transient,
            _ => ErrorKind::Permanent,
        }
    }

    /// Returns true if retrying the operation might succeed.
    pub fn is_retryable(&self) -> bool {
        self.kind() == ErrorKind::Transient
    }
}

/// Convenience `Result` alias for HNSW operations.
pub type Result<T> = std::result::Result<T, RetrievalError>;

/// Rejects vectors containing `NaN`, `Infinity`, or `-Infinity`.
#[inline]
pub fn validate_finite_vector(vector: &[f32]) -> Result<()> {
    for (i, &v) in vector.iter().enumerate() {
        if !v.is_finite() {
            return Err(RetrievalError::NonFiniteVector {
                reason: format!("element at index {i} is {v}"),
            });
        }
    }
    Ok(())
}
