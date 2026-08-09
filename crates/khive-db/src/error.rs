//! Error types for the SQLite storage layer.

use std::time::Duration;

use thiserror::Error;

/// Errors produced by the SQLite storage backend.
#[derive(Debug, Error)]
pub enum SqliteError {
    /// Underlying rusqlite driver error.
    #[error("sqlite error: {0}")]
    Rusqlite(#[from] rusqlite::Error),

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
    #[error("migration v{version} failed: {error}")]
    Migration {
        /// The migration version number that failed.
        version: u32,
        /// Human-readable description of the failure.
        error: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
