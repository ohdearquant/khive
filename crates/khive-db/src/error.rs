//! Error types for the SQLite storage layer.

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

    /// The store was migrated by a newer binary and cannot be downgraded in place.
    #[error(
        "this binary knows migrations up to {max_known_migration} but the store is at version \
         {store_version} — the binary is older than the store; upgrade the binary (in-place \
         downgrade is not supported)"
    )]
    SchemaTooNew {
        /// Highest migration version this binary can apply.
        max_known_migration: u32,
        /// Migration version recorded by the store.
        store_version: u32,
    },
}
