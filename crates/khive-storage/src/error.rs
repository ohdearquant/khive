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
            | Self::Driver { capability, .. } => Some(*capability),
            Self::Pool { .. } | Self::Timeout { .. } | Self::Transaction { .. } => None,
        }
    }

    /// Whether this error is transient and the operation may succeed on retry.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Pool { .. } | Self::Timeout { .. } | Self::Transaction { .. }
        )
    }

    /// Whether this error is an FTS5 query-parser rejection of the MATCH
    /// expression itself, as opposed to a connection/pool/driver-level
    /// failure of the text-search backend.
    ///
    /// Callers that fail-open the FTS leg of a hybrid search (degrading to
    /// vector-only results on a bad query string) MUST gate on this predicate
    /// rather than on `StorageError` broadly. `TextSearch::search` returns the
    /// same `Driver` variant for a malformed MATCH expression *and* for a
    /// genuine backend outage (pool exhaustion, connection failure, reader
    /// open failure) — treating every `Err` as degradable turns a real outage
    /// into a silently-empty "successful" search (issue #389 round-2 High).
    ///
    /// SQLite's FTS5 query parser (`sqlite3Fts5ParseError`, fts5_expr.c)
    /// prefixes every message it emits with the literal `"fts5: "` token —
    /// e.g. `fts5: syntax error near "@"`, `fts5: parser stack overflow`,
    /// `fts5: column queries are not supported (detail=none)`. This is a
    /// stable SQLite-internal convention, not a substring picked to match one
    /// observed message. It excludes non-parser FTS5 subsystem failures such
    /// as `fts5: error creating shadow table ...` (schema/storage corruption)
    /// by requiring the message to name one of the parser's own failure
    /// modes, not just the `fts5:` namespace prefix.
    ///
    /// Only applies to `Driver` errors from the `Text` capability at the
    /// `fts_search` operation — the exact seam `Fts5TextSearch::search` uses
    /// (`crates/khive-db/src/stores/text.rs`). Pool, Timeout, Transaction,
    /// and any other `operation` value (e.g. `fts_count`, `open_fts_reader`)
    /// always propagate.
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
        // Same message text, but at a different operation (e.g. reader open
        // failure) — must not be classified as a syntax error even if the
        // underlying message happened to mention "fts5:".
        let e = driver_err("open_fts_reader", "fts5: syntax error near \"@\"");
        assert!(!e.is_fts5_syntax_error());
    }

    #[test]
    fn driver_error_with_unrelated_message_is_not_classified_as_syntax_error() {
        // A genuine connection/driver outage at the fts_search operation, but
        // whose message does not name a parser failure mode — must propagate.
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
        // Round-3 High: a driver message containing the detail-mode substring
        // WITHOUT the `fts5: ` parser prefix is not from the FTS5 query
        // parser — must propagate, not degrade.
        let e = driver_err(
            "fts_search",
            "phrase queries are not supported (detail!=full)",
        );
        assert!(!e.is_fts5_syntax_error());
    }

    #[test]
    fn fts5_shadow_table_corruption_is_not_classified_as_syntax_error() {
        // A real FTS5-subsystem failure (schema/storage corruption), not a
        // MATCH-expression parser rejection — must propagate, not degrade.
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
}
