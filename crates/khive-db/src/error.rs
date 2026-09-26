//! Error types for the SQLite storage layer.

use std::time::Duration;

use khive_storage::{StorageCapability, StorageError};
use thiserror::Error;

/// Errors produced by the SQLite storage backend.
#[derive(Debug, Error)]
pub enum SqliteError {
    /// A request-scoped read or store acquisition stopped, or read cleanup failed.
    #[error(transparent)]
    RequestReadStopped(khive_storage::StorageError),

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

    /// A file-backed writer was refused before SQLite began the operation
    /// because the volume's free space had reached its configured reserve.
    #[error(
        "refusing sqlite write on {volume}: {available_bytes} bytes available, \
         at or below the {floor_bytes}-byte free-space floor"
    )]
    CapacityFloor {
        volume: String,
        available_bytes: u64,
        floor_bytes: u64,
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

impl SqliteError {
    pub(crate) fn into_storage_error(
        self,
        capability: StorageCapability,
        operation: &'static str,
    ) -> StorageError {
        match self {
            Self::CapacityFloor {
                volume,
                available_bytes,
                floor_bytes,
            } => StorageError::CapacityFloor {
                capability,
                volume,
                available_bytes,
                floor_bytes,
            },
            other => StorageError::driver(capability, operation, other),
        }
    }
}
