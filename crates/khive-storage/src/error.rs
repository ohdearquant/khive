//! Storage error types shared across all backend implementations.

use std::borrow::Cow;
use std::error::Error as StdError;

use thiserror::Error;

use crate::capability::StorageCapability;

/// Unified error type for all storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("{capability:?} resource not found: {resource} ({key})")]
    NotFound {
        capability: StorageCapability,
        resource: &'static str,
        key: String,
    },

    #[error("{capability:?} resource already exists: {resource} ({key})")]
    AlreadyExists {
        capability: StorageCapability,
        resource: &'static str,
        key: String,
    },

    #[error("conflict in {capability:?} during {operation}: {message}")]
    Conflict {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("invalid input for {capability:?} during {operation}: {message}")]
    InvalidInput {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("unsupported operation for {capability:?}: {operation} ({message})")]
    Unsupported {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("pool failure during {operation}: {message}")]
    Pool {
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("timeout during {operation}")]
    Timeout { operation: Cow<'static, str> },

    #[error("sql transaction failure during {operation}: {message}")]
    Transaction {
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("serialization failure in {capability:?}: {message}")]
    Serialization {
        capability: StorageCapability,
        message: String,
    },

    #[error("index maintenance failure in {capability:?}: {message}")]
    IndexMaintenance {
        capability: StorageCapability,
        message: String,
    },

    #[error("backend driver error in {capability:?} during {operation}: {source}")]
    Driver {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },

    /// The bounded write-queue channel (ADR-067 Component A) did not free
    /// capacity within the caller-supplied deadline. Returned only when a
    /// caller wraps `WriterTaskHandle::send`'s `channel.send().await` in a
    /// `tokio::time::timeout`; there is no immediate-error `try_send` path.
    #[error("write queue full: timed out after {timeout_ms}ms waiting for writer task capacity")]
    WriteQueueFull { timeout_ms: u64 },

    /// An internal write-queue plumbing failure not attributable to a
    /// specific storage capability: the writer task's channel closed (the
    /// task panicked or was dropped) or its oneshot reply was dropped before
    /// sending a result.
    #[error("internal storage error: {0}")]
    Internal(String),

    /// `KHIVE_WRITE_QUEUE=1` is set but the calling thread has no Tokio
    /// runtime context, so the writer task cannot be spawned (ADR-067
    /// Component A). Returned instead of panicking.
    /// See `crates/khive-storage/docs/api/error-taxonomy.md#writertasknoruntime`.
    #[error(
        "KHIVE_WRITE_QUEUE=1 but no Tokio runtime context is available to spawn the writer task"
    )]
    WriterTaskNoRuntime,

    /// A filesystem-backed capability (e.g. `BlobStore`) refused a write
    /// because `volume`'s available space, after accounting for the pending
    /// write, would drop below the configured free-space floor (khive#292).
    #[error(
        "refusing write on {capability:?} at {volume}: {available_bytes} bytes available, \
         below the {floor_bytes}-byte floor"
    )]
    CapacityFloor {
        capability: StorageCapability,
        volume: String,
        available_bytes: u64,
        floor_bytes: u64,
    },
}

impl StorageError {
    /// Construct a `Driver` error wrapping a backend-specific error source.
    pub fn driver(
        capability: StorageCapability,
        operation: impl Into<Cow<'static, str>>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::Driver {
            capability,
            operation: operation.into(),
            source: Box::new(source),
        }
    }

    /// Return the storage capability surface that produced this error, if any.
    pub fn capability(&self) -> Option<StorageCapability> {
        match self {
            Self::NotFound { capability, .. }
            | Self::AlreadyExists { capability, .. }
            | Self::Conflict { capability, .. }
            | Self::InvalidInput { capability, .. }
            | Self::Unsupported { capability, .. }
            | Self::Serialization { capability, .. }
            | Self::IndexMaintenance { capability, .. }
            | Self::Driver { capability, .. }
            | Self::CapacityFloor { capability, .. } => Some(*capability),
            Self::Pool { .. }
            | Self::Timeout { .. }
            | Self::Transaction { .. }
            | Self::WriteQueueFull { .. }
            | Self::Internal(..)
            | Self::WriterTaskNoRuntime => None,
        }
    }

    /// Whether this error is transient and the operation may succeed on retry.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Pool { .. }
                | Self::Timeout { .. }
                | Self::Transaction { .. }
                | Self::WriteQueueFull { .. }
        )
    }

    /// Whether this error is an FTS5 query-parser rejection of the MATCH
    /// expression itself, as opposed to a connection/pool/driver-level
    /// failure of the text-search backend.
    ///
    /// True only for `Driver` errors from the `Text` capability at the
    /// `fts_search` operation whose message names one of SQLite's FTS5
    /// parser failure modes (syntax error, stack overflow, unsupported
    /// column/phrase/NEAR query); all other errors return `false`.
    ///
    /// Callers that fail-open the FTS leg of a hybrid search (degrading to
    /// vector-only results on a bad query string) MUST gate on this
    /// predicate rather than on `StorageError` broadly — treating every
    /// `Err` as degradable turns a real backend outage into a silently-empty
    /// "successful" search (issue #389).
    /// See `crates/khive-storage/docs/api/error-taxonomy.md#is_fts5_syntax_error`.
    pub fn is_fts5_syntax_error(&self) -> bool {
        let Self::Driver {
            capability,
            operation,
            source,
        } = self
        else {
            return false;
        };
        if *capability != StorageCapability::Text || operation.as_ref() != "fts_search" {
            return false;
        }
        let msg = source.to_string();
        msg.contains("fts5: syntax error")
            || msg.contains("fts5: parser stack overflow")
            || msg.contains("fts5: column queries are not supported")
            || msg.contains("fts5: phrase queries are not supported (detail")
            || msg.contains("fts5: NEAR queries are not supported (detail")
    }

    /// Whether this error is a UNIQUE constraint violation from a raw SQL
    /// `execute` (e.g. an `INSERT` racing an existing row under a natural
    /// key). True only for `Driver` errors from the `Sql` capability whose
    /// `operation` is one of `execute`, `pool_writer.execute`, or
    /// `tx.execute`, and whose message contains `UNIQUE constraint failed`.
    /// Batch/script operations are intentionally excluded.
    ///
    /// Callers that treat exact-key duplicates as a tolerated no-op
    /// (ADR-081 §4 serve-ledger idempotency) MUST gate on this predicate
    /// rather than swallowing every `Driver` error at `execute` — that would
    /// also hide genuine write failures (disk full, corruption).
    /// See `crates/khive-storage/docs/api/error-taxonomy.md#is_unique_constraint_violation`.
    pub fn is_unique_constraint_violation(&self) -> bool {
        let Self::Driver {
            capability,
            operation,
            source,
        } = self
        else {
            return false;
        };
        if *capability != StorageCapability::Sql {
            return false;
        }
        if !matches!(
            operation.as_ref(),
            "execute" | "pool_writer.execute" | "tx.execute"
        ) {
            return false;
        }
        source.to_string().contains("UNIQUE constraint failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    struct FakeSource(String);

    impl fmt::Display for FakeSource {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl StdError for FakeSource {}

    fn driver_err(operation: &'static str, message: &str) -> StorageError {
        StorageError::driver(
            StorageCapability::Text,
            operation,
            FakeSource(message.into()),
        )
    }

    #[test]
    fn fts5_syntax_error_at_fts_search_is_classified_as_syntax_error() {
        let e = driver_err("fts_search", "fts5: syntax error near \"@\"");
        assert!(e.is_fts5_syntax_error());
    }

    #[test]
    fn fts5_parser_stack_overflow_is_classified_as_syntax_error() {
        let e = driver_err("fts_search", "fts5: parser stack overflow");
        assert!(e.is_fts5_syntax_error());
    }

    #[test]
    fn fts5_unsupported_column_query_is_classified_as_syntax_error() {
        let e = driver_err(
            "fts_search",
            "fts5: column queries are not supported (detail=none)",
        );
        assert!(e.is_fts5_syntax_error());
    }

    #[test]
    fn timeout_is_not_classified_as_syntax_error() {
        let e = StorageError::Timeout {
            operation: "fts_search".into(),
        };
        assert!(!e.is_fts5_syntax_error());
    }

    #[test]
    fn pool_failure_is_not_classified_as_syntax_error() {
        let e = StorageError::Pool {
            operation: "fts_search".into(),
            message: "pool exhausted".into(),
        };
        assert!(!e.is_fts5_syntax_error());
    }

    #[test]
    fn driver_error_at_non_search_operation_is_not_classified_as_syntax_error() {
        let e = driver_err("open_fts_reader", "fts5: syntax error near \"@\"");
        assert!(!e.is_fts5_syntax_error());
    }

    #[test]
    fn driver_error_with_unrelated_message_is_not_classified_as_syntax_error() {
        let e = driver_err("fts_search", "disk I/O error");
        assert!(!e.is_fts5_syntax_error());
    }

    #[test]
    fn fts5_phrase_detail_query_is_classified_as_syntax_error() {
        let e = driver_err(
            "fts_search",
            "fts5: phrase queries are not supported (detail!=full)",
        );
        assert!(e.is_fts5_syntax_error());
    }

    #[test]
    fn fts5_near_detail_query_is_classified_as_syntax_error() {
        let e = driver_err(
            "fts_search",
            "fts5: NEAR queries are not supported (detail!=full)",
        );
        assert!(e.is_fts5_syntax_error());
    }

    #[test]
    fn unprefixed_detail_message_is_not_classified_as_syntax_error() {
        let e = driver_err(
            "fts_search",
            "phrase queries are not supported (detail!=full)",
        );
        assert!(!e.is_fts5_syntax_error());
    }

    #[test]
    fn fts5_shadow_table_corruption_is_not_classified_as_syntax_error() {
        let e = driver_err(
            "fts_search",
            "fts5: error creating shadow table notes_content: no such table",
        );
        assert!(!e.is_fts5_syntax_error());
    }

    #[test]
    fn non_text_capability_is_not_classified_as_syntax_error() {
        let e = StorageError::Driver {
            capability: StorageCapability::Vectors,
            operation: "fts_search".into(),
            source: Box::new(FakeSource("fts5: syntax error near \"@\"".into())),
        };
        assert!(!e.is_fts5_syntax_error());
    }

    fn driver_err_sql(operation: &'static str, message: &str) -> StorageError {
        StorageError::driver(
            StorageCapability::Sql,
            operation,
            FakeSource(message.into()),
        )
    }

    #[test]
    fn unique_constraint_failure_at_execute_sql_capability_is_classified() {
        let e = driver_err_sql(
            "execute",
            "UNIQUE constraint failed: brain_serve_ledger.namespace, \
             brain_serve_ledger.target_id, brain_serve_ledger.query_class, \
             brain_serve_ledger.served_at",
        );
        assert!(e.is_unique_constraint_violation());
    }

    #[test]
    fn unique_constraint_failure_at_pool_writer_execute_is_classified() {
        let e = driver_err_sql("pool_writer.execute", "UNIQUE constraint failed: t.id");
        assert!(e.is_unique_constraint_violation());
    }

    #[test]
    fn unique_constraint_message_at_non_execute_operation_is_not_classified() {
        let e = driver_err_sql("query_row", "UNIQUE constraint failed: t.id");
        assert!(!e.is_unique_constraint_violation());
    }

    #[test]
    fn non_unique_driver_error_at_execute_is_not_classified() {
        let e = driver_err_sql("execute", "disk I/O error");
        assert!(!e.is_unique_constraint_violation());
    }

    #[test]
    fn non_sql_capability_is_not_classified_as_unique_violation() {
        let e = driver_err("execute", "UNIQUE constraint failed: t.id");
        assert!(!e.is_unique_constraint_violation());
    }

    #[test]
    fn timeout_is_not_classified_as_unique_violation() {
        let e = StorageError::Timeout {
            operation: "execute".into(),
        };
        assert!(!e.is_unique_constraint_violation());
    }
}
