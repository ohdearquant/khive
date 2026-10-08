//! Per-substrate SQLite store implementations.
//!
//! Each module provides a concrete store struct implementing one or more
//! `khive-storage` capability traits against the shared connection pool.

use std::sync::Arc;

use khive_storage::error::StorageError;
use khive_storage::types::StorageResult;
use khive_storage::StorageCapability;

use crate::pool::ConnectionPool;

pub mod agents;
pub mod attachment;
pub mod blob;
pub mod blob_s3;
pub mod entity;
pub mod event;
pub mod graph;
pub(crate) mod index_repair;
pub mod note;
pub mod sparse;
pub mod text;
pub mod vectors;

use khive_storage::{BatchWriteErrorClass, BatchWriteRetryability};

fn validate_json_equality_paths(
    predicates: &[(String, khive_storage::SqlValue)],
    capability: StorageCapability,
    operation: &'static str,
) -> Result<(), StorageError> {
    for (path, _) in predicates {
        if !path.starts_with("$.")
            || !path[2..].split('.').all(|part| {
                !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        {
            return Err(StorageError::InvalidInput {
                capability,
                operation: operation.into(),
                message: format!("invalid JSON equality path: {path:?}"),
            });
        }
    }
    Ok(())
}

fn append_json_equalities(
    conditions: &mut Vec<String>,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    column: &'static str,
    predicates: &[(String, khive_storage::SqlValue)],
) {
    use khive_storage::SqlValue;
    use rusqlite::types::Value;

    for (path, value) in predicates {
        params.push(Box::new(path.clone()));
        let path_index = params.len();
        let value = match value {
            SqlValue::Null => Value::Null,
            SqlValue::Bool(value) => Value::Integer(i64::from(*value)),
            SqlValue::Integer(value) => Value::Integer(*value),
            SqlValue::Float(value) => Value::Real(*value),
            SqlValue::Text(value) => Value::Text(value.clone()),
            SqlValue::Blob(value) => Value::Blob(value.clone()),
            SqlValue::Json(value) => Value::Text(value.to_string()),
            SqlValue::Uuid(value) => Value::Text(value.to_string()),
            SqlValue::Timestamp(value) => Value::Integer(value.timestamp_micros()),
        };
        params.push(Box::new(value));
        conditions.push(format!(
            "json_extract({column}, ?{path_index}) = ?{}",
            params.len()
        ));
    }
}

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

/// Run one typed-store read through the pool's bounded reader admission.
///
/// File-backed and in-memory stores deliberately share this one route. Pool
/// exhaustion is returned as the canonical retryable `AdmissionTimeout`; a
/// cancelled request remains the non-retryable `Timeout`. There is no
/// standalone-reader fallback (ADR-165 Slice 2).
///
/// The reader permit is awaited here, before the blocking task starts, so a
/// read that has to queue holds a task and not a blocking-pool thread. The
/// permit then moves into the blocking closure, which picks its connection
/// with the slot already held.
pub(crate) async fn run_pooled_store_read<F, R>(
    pool: Arc<ConnectionPool>,
    capability: StorageCapability,
    operation: &'static str,
    read: F,
) -> StorageResult<R>
where
    F: FnOnce(&rusqlite::Connection) -> Result<R, StorageError> + Send + 'static,
    R: Send + 'static,
{
    let admission = pool.acquire_reader_admission(capability, operation).await?;
    crate::read_cancellation::run_declared_interruptible_read(capability, operation, move |scope| {
        let mut guard = pool.resolve_reader_checkout(
            capability,
            operation,
            pool.reader_with_admission(admission, || scope.should_stop()),
        )?;
        let result = scope.run_pooled_reader(&mut guard, read);
        if let Err(error) = &result {
            pool.record_reader_query_error(error);
        }
        result
    })
    .await
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

#[cfg(test)]
mod pooled_read_admission_tests {
    use super::*;
    use crate::pool::PoolConfig;
    use std::time::Duration;

    const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(3);
    const QUEUE_SETTLE: Duration = Duration::from_millis(250);
    const WAITING_READS: u64 = 3;

    /// A runtime whose blocking pool holds one thread, so a pooled read that
    /// parks it makes every other blocking task wait its turn.
    fn single_blocking_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("test runtime")
    }

    /// A file-backed pool with one reader permit and a bounded checkout wait.
    fn single_reader_pool(dir: &tempfile::TempDir) -> Arc<ConnectionPool> {
        let config = PoolConfig {
            path: Some(dir.path().join("pooled_read_admission.db")),
            max_readers: 1,
            checkout_timeout: CHECKOUT_TIMEOUT,
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(config).expect("open the single-reader pool"))
    }

    /// Reads that must queue for the sole permit wait as tasks. The permit is
    /// held by a checked-out guard on the test task, because a read that
    /// blocked inside the runtime would itself take the only blocking thread.
    #[test]
    fn queued_pooled_reads_leave_the_blocking_pool_free_and_still_time_out() {
        let runtime = single_blocking_thread_runtime();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let pool = single_reader_pool(&dir);
            let held_reader = pool.reader().expect("hold the sole reader permit");

            let waiting: Vec<_> = (0..WAITING_READS)
                .map(|_| {
                    tokio::spawn(run_pooled_store_read(
                        Arc::clone(&pool),
                        StorageCapability::Entities,
                        "queued_read",
                        |_conn| Ok(()),
                    ))
                })
                .collect();
            tokio::time::sleep(QUEUE_SETTLE).await;

            let probe = tokio::time::timeout(
                Duration::from_secs(1),
                tokio::task::spawn_blocking(|| "probe ran"),
            )
            .await
            .expect("an unrelated blocking task must not wait behind queued pooled reads")
            .expect("probe task panicked");
            assert_eq!(probe, "probe ran");
            assert!(
                waiting.iter().all(|read| !read.is_finished()),
                "the probe must complete while every queued read is still waiting"
            );

            for read in waiting {
                let error = read.await.expect("read task panicked").unwrap_err();
                assert!(
                    matches!(error, StorageError::AdmissionTimeout { .. }),
                    "a queued read must fail with the retryable AdmissionTimeout, got {error:?}"
                );
            }
            drop(held_reader);
            let snapshot = pool.reader_acquisition_snapshot();
            assert_eq!(snapshot.checkout_timeouts, WAITING_READS);
            assert_eq!(snapshot.available_reader_admission_slots, 1);
        });
    }

    /// Cancelling a queued read releases it without waiting for the blocking
    /// pool or the checkout timeout, and takes no permit from the read behind it.
    #[test]
    fn cancelling_a_queued_pooled_read_returns_at_once_and_leaks_no_permit() {
        let runtime = single_blocking_thread_runtime();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let pool = single_reader_pool(&dir);
            let held_reader = pool.reader().expect("hold the sole reader permit");

            let surviving = tokio::spawn(run_pooled_store_read(
                Arc::clone(&pool),
                StorageCapability::Entities,
                "surviving_read",
                |_conn| Ok(()),
            ));
            // The surviving read queues first, so it is the one that would hold
            // the lone blocking thread while the cancelled read waits behind it.
            tokio::time::sleep(QUEUE_SETTLE).await;
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            let cancelled_read = run_pooled_store_read(
                Arc::clone(&pool),
                StorageCapability::Entities,
                "cancelled_read",
                |_conn| Ok(()),
            );
            let cancelled = tokio::spawn(khive_storage::scope_request_read_cancellation(
                cancel_rx,
                cancelled_read,
            ));
            tokio::time::sleep(QUEUE_SETTLE).await;
            cancel_tx.send(true).unwrap();

            let error = tokio::time::timeout(Duration::from_secs(1), cancelled)
                .await
                .expect("a cancelled queued read waited for the blocking pool or the timeout")
                .expect("cancelled read task panicked")
                .unwrap_err();
            assert!(
                matches!(error, StorageError::Timeout { .. }),
                "a cancelled queued read must be the non-retryable Timeout, got {error:?}"
            );
            assert!(
                !surviving.is_finished(),
                "the read that was not cancelled must keep waiting for its permit"
            );

            drop(held_reader);
            surviving
                .await
                .expect("surviving read task panicked")
                .expect("the surviving read must be admitted once the permit is free");
            let snapshot = pool.reader_acquisition_snapshot();
            assert_eq!(snapshot.available_reader_admission_slots, 1);
            assert_eq!(snapshot.checkout_timeouts, 0);
        });
    }
}
