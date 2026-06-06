//! Error types for the khive-db storage layer.

use thiserror::Error;

/// Errors produced by SQLite storage operations.
#[derive(Debug, Error)]
pub enum SqliteError {
    /// An error from the underlying `rusqlite` driver.
    #[error("sqlite error: {0}")]
    Rusqlite(#[from] rusqlite::Error),

    /// Data failed validation (schema mismatch, malformed record, etc.).
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// Filesystem I/O error (database file open, WAL checkpoint, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A versioned migration failed to apply.
    #[error("migration v{version} failed: {error}")]
    Migration {
        /// The migration version that failed.
        version: u32,
        /// The underlying error message.
        error: String,
    },
}
