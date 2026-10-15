//! Error types for retrieval operations.
//!
//! Errors are classified as transient (retryable: network, external services) or
//! permanent (non-retryable: validation, config, data integrity). See RETRIEVAL-06.

use thiserror::Error;

/// Error classification for retry behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Transient error that may succeed on retry (network, contention).
    Transient,
    /// Permanent error that won't be fixed by retry (validation, config).
    Permanent,
}

/// Errors that can occur during retrieval operations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum RetrievalError {
    /// Vector index operation failed.
    #[error("hnsw error: {0}")]
    Hnsw(String),

    /// BM25 index operation failed.
    #[error("bm25 error: {0}")]
    Bm25(String),

    /// Fusion operation failed.
    #[error("fusion error: {0}")]
    Fusion(String),

    /// Graph traversal failed.
    #[error("graph traversal error: {0}")]
    GraphTraversal(String),

    /// Invalid query parameters.
    #[error("invalid query: {0}")]
    InvalidQuery(String),

    /// Dimension mismatch.
    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch {
        /// Expected dimensions.
        expected: usize,
        /// Actual dimensions.
        actual: usize,
    },

    /// Configuration error.
    #[error("configuration error: {0}")]
    Configuration(String),

    /// Embedding store error.
    #[error("embedding store: {0}")]
    EmbeddingStore(String),

    /// Link store error (for graph operations).
    #[error("link store: {0}")]
    LinkStore(String),

    /// Index not initialized.
    #[error("index not initialized: {0}")]
    IndexNotInitialized(String),

    /// Index rebuild required.
    #[error("index rebuild required: {reason}")]
    RebuildRequired {
        /// Why rebuild is needed.
        reason: String,
    },

    /// Query timed out before completing.
    ///
    /// The search operation exceeded the configured timeout duration.
    /// This is a transient error: the query may succeed with a longer timeout
    /// or fewer results requested.
    #[error("query timed out after {elapsed_ms}ms")]
    QueryTimeout {
        /// Elapsed time in milliseconds before timeout.
        elapsed_ms: u64,
    },

    /// Query was cancelled via cancellation token.
    ///
    /// The search operation was cancelled before completing.
    /// This is a transient error: the query may succeed if not cancelled.
    #[error("query cancelled")]
    QueryCancelled,

    /// Memory budget exceeded.
    ///
    /// The insert operation would cause the index to exceed its configured
    /// memory budget. This is a permanent error: the same insert will always
    /// fail unless the budget is raised or existing data is removed.
    #[error("memory budget exceeded: current {current_usage} + item {item_size} > limit {limit}")]
    BudgetExceeded {
        /// Current estimated memory usage in bytes.
        current_usage: usize,
        /// Estimated size of the item being inserted in bytes.
        item_size: usize,
        /// Configured memory budget in bytes.
        limit: usize,
    },

    /// Reranking operation failed (permanent).
    #[error("rerank error: {0}")]
    Rerank(String),
    // TODO(port-rerank): khive-inference not ported yet; re-enable when available.
    // #[cfg(feature = "native-rerank")]
    // #[error("inference error: {0}")]
    // Inference(#[from] khive_inference::InferenceError),
}

impl RetrievalError {
    /// Get the error classification (transient or permanent).
    ///
    /// This classification determines retry behavior:
    /// - `Transient`: May succeed on retry (network, external services)
    /// - `Permanent`: Won't be fixed by retry (validation, config, data)
    ///
    /// # Error Classification Table
    ///
    /// | Error Type | Classification | Reason |
    /// |------------|---------------|--------|
    /// | EmbeddingStore | Transient | External service, may recover |
    /// | LinkStore | Transient | External service, may recover |
    /// | Hnsw | Permanent | Index algorithm error |
    /// | Bm25 | Permanent | Index algorithm error |
    /// | Fusion | Permanent | Score combination error |
    /// | GraphTraversal | Permanent | Graph algorithm error |
    /// | InvalidQuery | Permanent | User input validation |
    /// | DimensionMismatch | Permanent | Data incompatibility |
    /// | Configuration | Permanent | Setup/config issue |
    /// | IndexNotInitialized | Permanent | Missing prerequisite |
    /// | RebuildRequired | Permanent | Data integrity issue |
    /// | QueryTimeout | Transient | May succeed with longer timeout |
    /// | QueryCancelled | Transient | May succeed if not cancelled |
    /// | BudgetExceeded | Permanent | Capacity limit, won't auto-resolve |
    pub fn kind(&self) -> ErrorKind {
        match self {
            // Transient: external services that may recover, timeouts, cancellations
            RetrievalError::EmbeddingStore(_)
            | RetrievalError::LinkStore(_)
            | RetrievalError::QueryTimeout { .. }
            | RetrievalError::QueryCancelled => ErrorKind::Transient,

            // Permanent: logic, validation, and configuration errors
            RetrievalError::Hnsw(_)
            | RetrievalError::Bm25(_)
            | RetrievalError::Fusion(_)
            | RetrievalError::GraphTraversal(_)
            | RetrievalError::InvalidQuery(_)
            | RetrievalError::DimensionMismatch { .. }
            | RetrievalError::Configuration(_)
            | RetrievalError::IndexNotInitialized(_)
            | RetrievalError::RebuildRequired { .. }
            | RetrievalError::BudgetExceeded { .. }
            | RetrievalError::Rerank(_) => ErrorKind::Permanent,
            // TODO(port-rerank): khive-inference not ported yet
            // #[cfg(feature = "native-rerank")]
            // RetrievalError::Inference(_) => ErrorKind::Permanent,
        }
    }

    /// Check if this error is transient (external/network/contention — may succeed on retry).
    #[inline]
    pub fn is_transient(&self) -> bool {
        self.kind() == ErrorKind::Transient
    }

    /// Check if this error is permanent (won't be fixed by retry).
    ///
    /// Permanent errors should be surfaced to the user immediately
    /// without retry attempts.
    #[inline]
    pub fn is_permanent(&self) -> bool {
        self.kind() == ErrorKind::Permanent
    }

    /// Check if this error is retryable (alias for `is_transient`).
    ///
    /// Provided for backward compatibility and semantic clarity.
    #[inline]
    pub fn is_retryable(&self) -> bool {
        self.is_transient()
    }

    /// Create a rerank error (permanent).
    pub fn rerank(msg: impl Into<String>) -> Self {
        Self::Rerank(msg.into())
    }

    /// Create an HNSW error (permanent).
    pub fn hnsw(msg: impl Into<String>) -> Self {
        Self::Hnsw(msg.into())
    }

    /// Create a BM25 error (permanent).
    pub fn bm25(msg: impl Into<String>) -> Self {
        Self::Bm25(msg.into())
    }

    /// Create a fusion error (permanent).
    pub fn fusion(msg: impl Into<String>) -> Self {
        Self::Fusion(msg.into())
    }

    /// Create a graph traversal error (permanent).
    pub fn graph_traversal(msg: impl Into<String>) -> Self {
        Self::GraphTraversal(msg.into())
    }

    /// Create an invalid query error (permanent).
    pub fn invalid_query(msg: impl Into<String>) -> Self {
        Self::InvalidQuery(msg.into())
    }

    /// Create a dimension mismatch error (permanent).
    pub fn dimension_mismatch(expected: usize, actual: usize) -> Self {
        Self::DimensionMismatch { expected, actual }
    }

    /// Create a configuration error (permanent).
    pub fn configuration(msg: impl Into<String>) -> Self {
        Self::Configuration(msg.into())
    }

    /// Create an index not initialized error (permanent).
    pub fn index_not_initialized(msg: impl Into<String>) -> Self {
        Self::IndexNotInitialized(msg.into())
    }

    /// Create a rebuild required error (permanent).
    pub fn rebuild_required(reason: impl Into<String>) -> Self {
        Self::RebuildRequired {
            reason: reason.into(),
        }
    }

    /// Create a query timeout error (transient).
    pub fn query_timeout(elapsed_ms: u64) -> Self {
        Self::QueryTimeout { elapsed_ms }
    }

    /// Create a query cancelled error (transient).
    pub fn query_cancelled() -> Self {
        Self::QueryCancelled
    }

    /// Create a budget exceeded error (permanent).
    pub fn budget_exceeded(current_usage: usize, item_size: usize, limit: usize) -> Self {
        Self::BudgetExceeded {
            current_usage,
            item_size,
            limit,
        }
    }
}

/// Result type alias for retrieval operations.
pub type Result<T> = std::result::Result<T, RetrievalError>;

#[cfg(test)]
#[path = "error/tests.rs"]
mod tests;
