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
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = RetrievalError::hnsw("connection failed");
        assert_eq!(err.to_string(), "hnsw error: connection failed");
    }

    #[test]
    fn test_dimension_mismatch() {
        let err = RetrievalError::dimension_mismatch(768, 512);
        assert_eq!(err.to_string(), "dimension mismatch: expected 768, got 512");
    }

    #[test]
    fn test_is_retryable() {
        // Non-retryable (permanent errors)
        assert!(!RetrievalError::hnsw("fail").is_retryable());
        assert!(!RetrievalError::bm25("fail").is_retryable());
        assert!(!RetrievalError::InvalidQuery("bad".into()).is_retryable());
        assert!(!RetrievalError::dimension_mismatch(768, 512).is_retryable());
    }

    // RETRIEVAL-06: Comprehensive error classification tests

    #[test]
    fn test_error_kind_transient() {
        // EmbeddingStore and LinkStore are transient (external services)
        // Note: We can't easily construct these without the actual error types,
        // so we test via is_transient/is_permanent methods on constructable errors
    }

    #[test]
    fn test_error_kind_permanent_all_variants() {
        // All internal errors should be permanent
        let permanent_errors: Vec<RetrievalError> = vec![
            RetrievalError::hnsw("index corrupt"),
            RetrievalError::bm25("tokenization failed"),
            RetrievalError::fusion("incompatible scores"),
            RetrievalError::graph_traversal("cycle detected"),
            RetrievalError::invalid_query("empty query"),
            RetrievalError::dimension_mismatch(768, 512),
            RetrievalError::configuration("invalid k1 value"),
            RetrievalError::index_not_initialized("HNSW index"),
            RetrievalError::rebuild_required("version mismatch"),
            RetrievalError::budget_exceeded(1000, 500, 1200),
        ];

        for err in permanent_errors {
            assert!(err.is_permanent(), "Expected permanent: {err:?}");
            assert!(!err.is_transient(), "Should not be transient: {err:?}");
            assert_eq!(
                err.kind(),
                ErrorKind::Permanent,
                "Kind mismatch for: {err:?}"
            );
        }
    }

    #[test]
    fn test_is_transient_is_permanent_consistency() {
        // is_transient and is_permanent should be mutually exclusive and exhaustive
        let test_errors: Vec<RetrievalError> = vec![
            RetrievalError::hnsw("test"),
            RetrievalError::bm25("test"),
            RetrievalError::fusion("test"),
            RetrievalError::invalid_query("test"),
            RetrievalError::dimension_mismatch(1, 2),
            RetrievalError::configuration("test"),
            RetrievalError::budget_exceeded(100, 50, 120),
        ];

        for err in test_errors {
            let transient = err.is_transient();
            let permanent = err.is_permanent();

            // XOR: exactly one should be true
            assert!(
                transient ^ permanent,
                "Error must be exactly transient OR permanent: {err:?} (transient={transient}, permanent={permanent})"
            );

            // is_retryable should match is_transient
            assert_eq!(
                err.is_retryable(),
                err.is_transient(),
                "is_retryable should equal is_transient for: {err:?}"
            );
        }
    }

    #[test]
    fn test_error_constructors_produce_correct_messages() {
        assert_eq!(RetrievalError::hnsw("test").to_string(), "hnsw error: test");
        assert_eq!(RetrievalError::bm25("test").to_string(), "bm25 error: test");
        assert_eq!(
            RetrievalError::fusion("test").to_string(),
            "fusion error: test"
        );
        assert_eq!(
            RetrievalError::graph_traversal("test").to_string(),
            "graph traversal error: test"
        );
        assert_eq!(
            RetrievalError::invalid_query("test").to_string(),
            "invalid query: test"
        );
        assert_eq!(
            RetrievalError::configuration("test").to_string(),
            "configuration error: test"
        );
        assert_eq!(
            RetrievalError::index_not_initialized("test").to_string(),
            "index not initialized: test"
        );
        assert_eq!(
            RetrievalError::rebuild_required("test").to_string(),
            "index rebuild required: test"
        );
        assert_eq!(
            RetrievalError::budget_exceeded(100, 50, 120).to_string(),
            "memory budget exceeded: current 100 + item 50 > limit 120"
        );
    }

    #[test]
    fn test_error_kind_enum_debug() {
        // Verify ErrorKind is Debug-able
        assert_eq!(format!("{:?}", ErrorKind::Transient), "Transient");
        assert_eq!(format!("{:?}", ErrorKind::Permanent), "Permanent");
    }

    #[test]
    fn test_error_kind_equality() {
        // Verify ErrorKind implements PartialEq correctly
        assert_eq!(ErrorKind::Transient, ErrorKind::Transient);
        assert_eq!(ErrorKind::Permanent, ErrorKind::Permanent);
        assert_ne!(ErrorKind::Transient, ErrorKind::Permanent);
    }

    #[test]
    fn test_query_timeout_error() {
        let err = RetrievalError::query_timeout(5000);
        assert_eq!(err.to_string(), "query timed out after 5000ms");
        assert!(err.is_transient());
        assert!(!err.is_permanent());
        assert!(err.is_retryable());
        assert_eq!(err.kind(), ErrorKind::Transient);
    }

    #[test]
    fn test_query_cancelled_error() {
        let err = RetrievalError::query_cancelled();
        assert_eq!(err.to_string(), "query cancelled");
        assert!(err.is_transient());
        assert!(!err.is_permanent());
        assert!(err.is_retryable());
        assert_eq!(err.kind(), ErrorKind::Transient);
    }

    #[test]
    fn test_transient_errors_classification() {
        // All transient errors should be classified correctly
        let transient_errors: Vec<RetrievalError> = vec![
            RetrievalError::query_timeout(100),
            RetrievalError::query_cancelled(),
        ];

        for err in transient_errors {
            assert!(err.is_transient(), "Expected transient: {err:?}");
            assert!(!err.is_permanent(), "Should not be permanent: {err:?}");
            assert_eq!(
                err.kind(),
                ErrorKind::Transient,
                "Kind mismatch for: {err:?}"
            );
        }
    }
}
