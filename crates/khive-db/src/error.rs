//! Error types for the SQLite storage layer.

use std::time::Duration;

use thiserror::Error;

/// Errors produced by the SQLite storage backend.
#[derive(Debug, Error)]
pub enum SqliteError {
    /// Underlying rusqlite driver error.
    #[error("sqlite error: {0}")]
    Rusqlite(#[source] rusqlite::Error),

    /// Data invariant violation (corrupt row, unexpected schema state).
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// The process-local writer mutex was not acquired within the pool's
    /// configured finite checkout deadline. This stage happens before SQLite
    /// executes, so callers must not conflate it with SQLite busy/locked or
    /// checkpoint starvation.
    ///
    /// The display text intentionally retains the historical `InvalidData`
    /// prefix for compatibility while the variant supplies stable structural
    /// classification (ADR-135 F6).
    #[error("invalid data: timed out after {timeout:?} waiting for sqlite writer connection")]
    WriterPoolCheckoutTimeout {
        /// Pool checkout deadline that elapsed.
        timeout: Duration,
    },

    /// A file-backed write was refused before SQLite execution because the
    /// database volume no longer clears the configured recovery reserve.
    /// This is a capacity admission outcome, not a busy/lock timeout.
    #[error(
        "refusing sqlite write at {volume}: {available_bytes} bytes available, which does not \
         clear the configured {reserve_bytes}-byte recovery reserve"
    )]
    DiskCapacityFloor {
        /// Canonical database path identifying the affected volume.
        volume: String,
        /// Fresh free-space sample taken at the admission boundary.
        available_bytes: u64,
        /// Configured bytes that must remain available for recovery work.
        reserve_bytes: u64,
    },

    /// A `PoolConfig` value violated a validated invariant at configuration
    /// load time (e.g. ADR-131 Decision 2's `write_admission_deadline_ms`
    /// range). Fires before any connection is opened, and is never silently
    /// clamped into range.
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// Filesystem I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A versioned migration failed to apply.
    #[error("migration v{version} failed: {source}")]
    Migration {
        /// The migration version number that failed.
        version: u32,
        /// Original SQLite failure, retained so callers and escalation
        /// telemetry never have to recover a result code from display text.
        #[source]
        source: rusqlite::Error,
    },
}

impl From<rusqlite::Error> for SqliteError {
    fn from(error: rusqlite::Error) -> Self {
        log_sqlite_full("sqlite_error_conversion", &error);
        Self::Rusqlite(error)
    }
}

/// Return whether `error` is SQLite's out-of-space result class.
///
/// `SQLITE_FULL` is materially different from `SQLITE_BUSY`/`SQLITE_LOCKED`:
/// retrying without recovering capacity can make the incident worse. Keep the
/// classification typed and based on the primary result code, never on
/// locale-dependent display text.
pub(crate) fn is_sqlite_full(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::DiskFull
    )
}

/// Emit the distinct high-severity escalation required when SQLite itself
/// reaches `SQLITE_FULL`, meaning the pre-write reserve guard was bypassed,
/// disabled, or overtaken by an external/concurrent consumer.
pub(crate) fn log_sqlite_full(operation: &str, error: &rusqlite::Error) {
    if !is_sqlite_full(error) {
        return;
    }
    if let rusqlite::Error::SqliteFailure(code, message) = error {
        tracing::error!(
            operation,
            sqlite_primary_code = rusqlite::ffi::SQLITE_FULL,
            sqlite_extended_code = code.extended_code,
            sqlite_message = message.as_deref().unwrap_or("<none>"),
            "SQLITE_FULL escalation: SQLite exhausted capacity inside an admitted operation"
        );
    }
}

/// Wrap a raw rusqlite driver failure while preserving the common
/// `SQLITE_FULL` escalation at every store/SQL-bridge mapping boundary.
pub(crate) fn storage_driver_error(
    capability: khive_storage::StorageCapability,
    operation: &'static str,
    error: rusqlite::Error,
) -> khive_storage::StorageError {
    log_sqlite_full(operation, &error);
    khive_storage::StorageError::driver(capability, operation, error)
}

/// Walk an already-wrapped storage driver's complete source chain for
/// `SQLITE_FULL`. Writer-task closures return `StorageError`, and runtime
/// operations may add domain wrappers around `SqliteError`; following
/// `Error::source` closes both shapes without teaching this layer every wrapper
/// type.
pub(crate) fn log_storage_sqlite_full(operation: &str, error: &khive_storage::StorageError) {
    let khive_storage::StorageError::Driver { source, .. } = error else {
        return;
    };

    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(source.as_ref());
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<rusqlite::Error>() {
            log_sqlite_full(operation, error);
            return;
        }
        current = error.source();
    }
}

/// Preserve a best-effort batch's historical `first_error` contract while
/// escalating a raw `SQLITE_FULL` before the driver error is flattened into
/// display text and the outer operation returns `Ok(BatchWriteSummary)`.
pub(crate) fn record_batch_item_error(
    operation: &str,
    error: &rusqlite::Error,
    first_error: &mut String,
) {
    log_sqlite_full(operation, error);
    if first_error.is_empty() {
        *first_error = error.to_string();
    }
}

/// Observe a raw SQLite result whose caller deliberately preserves the
/// historical best-effort control flow and discards the outcome. This keeps
/// non-FULL failures silent while ensuring capacity exhaustion is escalated
/// before a rollback/savepoint/preflight error can be swallowed.
pub(crate) fn log_ignored_sqlite_result<T>(operation: &str, result: rusqlite::Result<T>) {
    if let Err(error) = result {
        log_sqlite_full(operation, &error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct NestedSqliteError(SqliteError);

    impl std::fmt::Display for NestedSqliteError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "nested sqlite failure: {}", self.0)
        }
    }

    impl std::error::Error for NestedSqliteError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    struct ErrorCapture {
        events: Arc<Mutex<Vec<(tracing::Level, String)>>>,
    }

    impl tracing::Subscriber for ErrorCapture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor(Option<String>);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0 = Some(format!("{value:?}"));
                    }
                }
            }
            let mut visitor = Visitor(None);
            event.record(&mut visitor);
            self.events
                .lock()
                .unwrap()
                .push((*event.metadata().level(), visitor.0.unwrap_or_default()));
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn sqlite_full_is_a_distinct_escalation_class() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".to_string()),
        );
        assert!(is_sqlite_full(&error));
        assert!(!is_sqlite_full(&rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("busy".to_string()),
        )));

        let events = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::with_default(
            ErrorCapture {
                events: Arc::clone(&events),
            },
            || log_sqlite_full("test_write", &error),
        );
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, tracing::Level::ERROR);
        assert!(events[0].1.contains("SQLITE_FULL escalation"));
    }

    #[test]
    fn storage_sqlite_full_escalation_walks_nested_error_sources() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".to_string()),
        );
        let storage_error = khive_storage::StorageError::driver(
            khive_storage::StorageCapability::Entities,
            "merge_entity",
            NestedSqliteError(SqliteError::Rusqlite(error)),
        );
        let events = Arc::new(Mutex::new(Vec::new()));

        tracing::subscriber::with_default(
            ErrorCapture {
                events: Arc::clone(&events),
            },
            || log_storage_sqlite_full("writer_task_operation", &storage_error),
        );

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, tracing::Level::ERROR);
        assert!(events[0].1.contains("SQLITE_FULL escalation"));
    }

    #[test]
    fn batch_error_recording_escalates_full_before_flattening() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".to_string()),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut first_error = "earlier item failure".to_string();

        tracing::subscriber::with_default(
            ErrorCapture {
                events: Arc::clone(&events),
            },
            || record_batch_item_error("entity_upsert_batch_item", &error, &mut first_error),
        );

        assert_eq!(first_error, "earlier item failure");
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, tracing::Level::ERROR);
        assert!(events[0].1.contains("SQLITE_FULL escalation"));
    }

    #[test]
    fn batch_error_recording_preserves_first_error_for_non_full_failures() {
        let first = rusqlite::Error::InvalidQuery;
        let later = rusqlite::Error::InvalidParameterName("later".to_string());
        let mut first_error = String::new();

        record_batch_item_error("batch_item", &first, &mut first_error);
        record_batch_item_error("batch_item", &later, &mut first_error);

        assert_eq!(first_error, first.to_string());
    }

    #[test]
    fn ignored_rollback_result_still_escalates_full() {
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".to_string()),
        );
        let events = Arc::new(Mutex::new(Vec::new()));

        tracing::subscriber::with_default(
            ErrorCapture {
                events: Arc::clone(&events),
            },
            || log_ignored_sqlite_result("fts_batch_rollback", Err::<(), _>(error)),
        );

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, tracing::Level::ERROR);
        assert!(events[0].1.contains("SQLITE_FULL escalation"));
    }

    #[test]
    fn ignored_non_full_result_remains_silent() {
        let events = Arc::new(Mutex::new(Vec::new()));

        tracing::subscriber::with_default(
            ErrorCapture {
                events: Arc::clone(&events),
            },
            || {
                log_ignored_sqlite_result(
                    "fts_batch_rollback",
                    Err::<(), _>(rusqlite::Error::InvalidQuery),
                )
            },
        );

        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn production_store_census_has_no_raw_sqlite_error_swallow_seams() {
        let entity = include_str!("stores/entity.rs");
        let note = include_str!("stores/note.rs");
        let sparse = include_str!("stores/sparse.rs");
        let text = include_str!("stores/text.rs");
        let vectors = include_str!("stores/vectors.rs");
        let sources = [
            ("agents", include_str!("stores/agents.rs")),
            ("entity", entity),
            ("event", include_str!("stores/event.rs")),
            ("graph", include_str!("stores/graph.rs")),
            ("note", note),
            ("sparse", sparse),
            ("text", text),
            ("vectors", vectors),
            ("sql_bridge", include_str!("sql_bridge.rs")),
        ];

        for (name, source) in sources {
            assert!(
                !source.contains("let _ = conn.execute_batch")
                    && !source.contains("let _ = guard.execute_batch"),
                "{name} still discards a raw SQLite rollback/savepoint result"
            );
        }

        for (name, source, expected_non_sqlite_count) in [
            ("entity", entity, 0),
            ("note", note, 0),
            // Sparse retains one serde_json serialization failure with this
            // spelling; its SQLite item failure must use the typed helper.
            ("sparse", sparse, 1),
            ("text", text, 0),
            ("vectors", vectors, 0),
        ] {
            assert_eq!(
                source.matches("first_error = e.to_string();").count(),
                expected_non_sqlite_count,
                "{name} still flattens a raw SQLite batch-item error directly"
            );
        }

        assert!(
            !include_str!("sql_bridge.rs")
                .contains("Err(_) => prepared.push(PreparedBatchStatement::PrepareAtExecution)"),
            "SQL batch preflight still discards the concrete prepare error before escalation"
        );
    }
}
