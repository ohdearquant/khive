//! Per-substrate SQLite store implementations.
//!
//! Each module provides a concrete store struct implementing one or more
//! `khive-storage` capability traits against the shared connection pool.

pub mod agents;
pub mod attachment;
pub mod blob;
pub mod blob_s3;
pub mod entity;
pub mod event;
pub mod graph;
pub mod note;
pub mod sparse;
pub mod text;
pub mod vectors;

use khive_storage::{BatchWriteErrorClass, BatchWriteRetryability};

/// Stable refusal classification for SQLite errors captured inside a
/// best-effort per-item batch loop.
fn classify_batch_sqlite_error(
    error: &rusqlite::Error,
) -> (BatchWriteErrorClass, BatchWriteRetryability) {
    use rusqlite::ErrorCode;

    match error.sqlite_error_code() {
        Some(ErrorCode::ConstraintViolation) => (
            BatchWriteErrorClass::Constraint,
            BatchWriteRetryability::Permanent,
        ),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => (
            BatchWriteErrorClass::Busy,
            BatchWriteRetryability::Transient,
        ),
        Some(ErrorCode::OperationInterrupted) => (
            BatchWriteErrorClass::Cancelled,
            BatchWriteRetryability::Transient,
        ),
        Some(_) => (
            BatchWriteErrorClass::Driver,
            BatchWriteRetryability::Unknown,
        ),
        None => (
            BatchWriteErrorClass::Unknown,
            BatchWriteRetryability::Unknown,
        ),
    }
}

#[cfg(test)]
mod batch_error_classification_tests {
    use super::*;

    fn sqlite_failure(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None)
    }

    #[test]
    fn sqlite_refusal_classes_distinguish_permanent_transient_and_unknown() {
        let cases = [
            (
                rusqlite::ffi::SQLITE_CONSTRAINT,
                BatchWriteErrorClass::Constraint,
                BatchWriteRetryability::Permanent,
            ),
            (
                rusqlite::ffi::SQLITE_BUSY,
                BatchWriteErrorClass::Busy,
                BatchWriteRetryability::Transient,
            ),
            (
                rusqlite::ffi::SQLITE_LOCKED,
                BatchWriteErrorClass::Busy,
                BatchWriteRetryability::Transient,
            ),
            (
                rusqlite::ffi::SQLITE_INTERRUPT,
                BatchWriteErrorClass::Cancelled,
                BatchWriteRetryability::Transient,
            ),
            (
                rusqlite::ffi::SQLITE_CORRUPT,
                BatchWriteErrorClass::Driver,
                BatchWriteRetryability::Unknown,
            ),
        ];

        for (code, expected_class, expected_retryability) in cases {
            assert_eq!(
                classify_batch_sqlite_error(&sqlite_failure(code)),
                (expected_class, expected_retryability)
            );
        }

        assert_eq!(
            classify_batch_sqlite_error(&rusqlite::Error::InvalidQuery),
            (
                BatchWriteErrorClass::Unknown,
                BatchWriteRetryability::Unknown
            )
        );
    }
}
