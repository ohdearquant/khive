//! Error types for the SQLite storage layer.

use std::time::Duration;

use khive_storage::error::{SqliteWriteFailure, SqliteWriteStage};
use khive_storage::{StorageCapability, StorageError, WriterTaskRequestState};
use thiserror::Error;

// Inspect only owned error shapes. User callbacks may supply cyclic or effectful
// source() implementations; attaching evidence must not invoke those again.
pub(crate) fn native_write_failure(
    error: &(dyn std::error::Error + 'static),
    stage: SqliteWriteStage,
) -> Option<SqliteWriteFailure> {
    if let Some(error) = error.downcast_ref::<StorageError>() {
        return match error {
            StorageError::SqliteWrite { failure, .. } => Some(*failure),
            StorageError::Driver { source, .. } => native_write_failure(source.as_ref(), stage),
            StorageError::WriterTaskRequestFailed { source, .. } => {
                native_write_failure(source.as_ref(), stage)
            }
            _ => None,
        };
    }
    if let Some(error) = error.downcast_ref::<SqliteError>() {
        return match error {
            SqliteError::Write { stage, source } => native_write_failure(source, *stage),
            SqliteError::WriteSettlementUnknown { failure } => Some(*failure),
            SqliteError::Rusqlite(source) => native_write_failure(source, stage),
            _ => None,
        };
    }
    if let Some(
        rusqlite::Error::SqliteFailure(code, _)
        | rusqlite::Error::SqlInputError { error: code, .. },
    ) = error.downcast_ref::<rusqlite::Error>()
    {
        return Some(SqliteWriteFailure {
            stage,
            primary_code: code.extended_code & 0xff,
            extended_code: code.extended_code,
            settlement_unknown: false,
        });
    }
    None
}

pub(crate) fn with_write_stage(error: StorageError, stage: SqliteWriteStage) -> StorageError {
    match native_write_failure(&error, stage) {
        Some(failure) => error.with_sqlite_write_failure(failure),
        None => error,
    }
}

pub(crate) fn statement_failure(error: StorageError) -> StorageError {
    with_write_stage(error, SqliteWriteStage::Statement)
}

pub(crate) fn unknown_write_settlement(mut error: StorageError) -> StorageError {
    if let StorageError::SqliteWrite { failure, .. } = &mut error {
        failure.settlement_unknown = true;
    }
    error
}

/// Stable ADR-194 capacity stages. The refusal stage is reserved for the WAL
/// I/O limiter; this configuration-only slice emits only unavailable.
pub const SQLITE_WAL_CAPACITY_REFUSED_STAGE: &str = "sqlite_wal_capacity_refused";
pub const SQLITE_WAL_CAPACITY_UNAVAILABLE_STAGE: &str = "sqlite_wal_capacity_unavailable";

/// Errors produced by the SQLite storage backend.
#[derive(Debug, Error)]
pub enum SqliteError {
    #[error("sqlite error: {source}")]
    Write {
        stage: SqliteWriteStage,
        #[source]
        source: rusqlite::Error,
    },
    /// A request-scoped read or store acquisition stopped, or read cleanup failed.
    #[error(transparent)]
    RequestReadStopped(khive_storage::StorageError),

    /// Underlying rusqlite driver error.
    #[error("sqlite error: {0}")]
    Rusqlite(#[from] rusqlite::Error),

    /// Data invariant violation (corrupt row, unexpected schema state).
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// A pooled connection contained a transaction from an earlier owner.
    /// Its prior side effects cannot be attributed to the new request.
    #[error("pooled writer contains an inherited transaction; prior side effects are unknown")]
    InheritedWriterTransaction,

    /// The writer could not prove transaction settlement before retirement.
    #[error("writer transaction settlement is unknown; connection retired")]
    WriterSettlementUnknown,

    /// Source-free native evidence retained when cleanup cannot prove settlement.
    #[error("writer transaction settlement is unknown; connection retired")]
    WriteSettlementUnknown { failure: SqliteWriteFailure },

    /// An earlier write on this database could not prove its settlement, so
    /// every later write is refused before it starts. Only the write whose
    /// settlement failed reports an unknown outcome; this one never ran.
    #[error("writer refused: an earlier write's settlement is unknown; this write did not start")]
    WriterPoisoned,

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
         at or below the {floor_bytes}-byte free-space floor plus \
         {required_headroom_bytes} bytes of operation headroom"
    )]
    CapacityFloor {
        volume: String,
        available_bytes: u64,
        floor_bytes: u64,
        required_headroom_bytes: u64,
    },

    /// A new logical write could not resolve its volume, acquire its lease,
    /// or sample available space.
    #[error("sqlite capacity admission unavailable in {phase} phase: {message}")]
    CapacityUnavailable {
        phase: khive_storage::CapacityUnavailablePhase,
        message: String,
    },

    /// The thread asking for a volume's write lease already holds it, so
    /// waiting could never succeed. This is a nested write, not lock
    /// contention, and it is refused at once instead of at the deadline.
    #[error(
        "volume lease re-entry: this thread already holds the lease for this volume \
         (held at {holder_site}, requested again at {requester_site})"
    )]
    VolumeLeaseReentry {
        holder_site: String,
        requester_site: String,
    },

    /// A configured WAL ceiling cannot be represented by SQLite's signed
    /// file-offset arithmetic.
    #[error("invalid WAL ceiling {bytes} bytes: exceeds supported SQLite file offsets")]
    WalCeilingOffsetOverflow { bytes: u64 },

    /// A WAL ceiling was enabled for a backend that cannot produce a WAL.
    #[error("invalid WAL ceiling {bytes} bytes: {backend_kind} does not support WAL enforcement")]
    WalCeilingUnsupported {
        bytes: u64,
        backend_kind: &'static str,
    },

    /// One committed WAL frame cannot fit, even immediately after reset.
    #[error(
        "invalid WAL ceiling {bytes} bytes: page size {page_size} requires at least {minimum_bytes} bytes for one WAL frame"
    )]
    WalCeilingBelowMinimum {
        bytes: u64,
        page_size: u64,
        minimum_bytes: u64,
    },

    /// A valid enabled policy cannot run until its WAL I/O limiter exists.
    #[error(
        "{stage}: WAL ceiling {bytes} bytes cannot be enforced: missing {capability}",
        stage = SQLITE_WAL_CAPACITY_UNAVAILABLE_STAGE
    )]
    WalCapacityUnavailable {
        bytes: u64,
        capability: &'static str,
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
    /// Attach a known write boundary while retaining the original native driver error.
    pub fn write(error: rusqlite::Error, stage: SqliteWriteStage) -> Self {
        if matches!(
            error,
            rusqlite::Error::SqliteFailure(..) | rusqlite::Error::SqlInputError { .. }
        ) {
            Self::Write {
                stage,
                source: error,
            }
        } else {
            Self::Rusqlite(error)
        }
    }

    pub(crate) fn settlement_with_cause(
        error: &(dyn std::error::Error + 'static),
        stage: SqliteWriteStage,
    ) -> Self {
        match native_write_failure(error, stage) {
            Some(mut failure) => {
                failure.settlement_unknown = true;
                Self::WriteSettlementUnknown { failure }
            }
            None => Self::WriterSettlementUnknown,
        }
    }

    pub fn write_failure(&self) -> Option<SqliteWriteFailure> {
        match self {
            Self::Write { stage, source } => native_write_failure(source, *stage),
            Self::WriteSettlementUnknown { failure } => Some(*failure),
            _ => None,
        }
    }
    /// Stable structured stage for an ADR-194 WAL-capacity failure.
    pub fn wal_capacity_stage(&self) -> Option<&'static str> {
        match self {
            Self::WalCapacityUnavailable { .. } => Some(SQLITE_WAL_CAPACITY_UNAVAILABLE_STAGE),
            _ => None,
        }
    }

    /// Capacity admission is a property of the SQLite file, so its refusals
    /// carry `StorageCapability::Sql` whichever store requested the write.
    /// Concrete backend callers use the same mapping as trait implementations:
    /// capacity and writer-settlement failures retain their storage taxonomy,
    /// and other failures remain typed sources of `StorageError::Driver`.
    pub fn into_storage_error(
        self,
        capability: StorageCapability,
        operation: &'static str,
    ) -> StorageError {
        match self {
            error @ Self::Write { stage, .. } => {
                with_write_stage(StorageError::driver(capability, operation, error), stage)
            }
            Self::CapacityFloor {
                volume,
                available_bytes,
                floor_bytes,
                required_headroom_bytes,
            } => StorageError::CapacityFloor {
                capability: StorageCapability::Sql,
                volume,
                available_bytes,
                floor_bytes,
                required_headroom_bytes,
            },
            Self::CapacityUnavailable { phase, message } => StorageError::CapacityUnavailable {
                capability: StorageCapability::Sql,
                phase,
                message,
            },
            Self::WriteSettlementUnknown { failure } => {
                StorageError::writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown)
                    .with_sqlite_write_failure(failure)
            }
            Self::InheritedWriterTransaction | Self::WriterSettlementUnknown => {
                StorageError::writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown)
            }
            Self::WriterPoisoned => {
                StorageError::writer_task_terminated(WriterTaskRequestState::NotStarted)
            }
            other => StorageError::driver(capability, operation, other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_storage::CapacityUnavailablePhase;

    #[test]
    fn capacity_and_settlement_errors_keep_their_meaning_from_any_store() {
        for capability in [StorageCapability::Entities, StorageCapability::Sql] {
            let refused = SqliteError::CapacityFloor {
                volume: "/volume".to_string(),
                available_bytes: 99,
                floor_bytes: 100,
                required_headroom_bytes: 12,
            }
            .into_storage_error(capability, "write");
            assert!(
                matches!(
                    refused,
                    StorageError::CapacityFloor {
                        capability: StorageCapability::Sql,
                        available_bytes: 99,
                        floor_bytes: 100,
                        required_headroom_bytes: 12,
                        ..
                    }
                ),
                "{capability:?}: {refused:?}"
            );

            let unavailable = SqliteError::CapacityUnavailable {
                phase: CapacityUnavailablePhase::Lock,
                message: "lease timed out".to_string(),
            }
            .into_storage_error(capability, "write");
            assert!(
                matches!(
                    unavailable,
                    StorageError::CapacityUnavailable {
                        capability: StorageCapability::Sql,
                        phase: CapacityUnavailablePhase::Lock,
                        ..
                    }
                ),
                "{capability:?}: {unavailable:?}"
            );

            for unsettled in [
                SqliteError::InheritedWriterTransaction,
                SqliteError::WriterSettlementUnknown,
            ] {
                let mapped = unsettled.into_storage_error(capability, "write");
                assert!(
                    matches!(
                        mapped,
                        StorageError::WriterTaskTerminated {
                            request_state: WriterTaskRequestState::SideEffectsUnknown,
                            ..
                        }
                    ),
                    "{capability:?}: {mapped:?}"
                );
            }

            let refused = SqliteError::WriterPoisoned.into_storage_error(capability, "write");
            assert!(
                matches!(
                    refused,
                    StorageError::WriterTaskTerminated {
                        request_state: WriterTaskRequestState::NotStarted,
                        ..
                    }
                ),
                "{capability:?}: {refused:?}"
            );
        }
    }
}
