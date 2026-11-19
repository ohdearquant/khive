//! Raw SQL pooled reads wait for their reader permit on the async side, in
//! arrival order with typed store reads, so a read that has to queue holds a
//! task and not a blocking-pool thread.

use std::sync::Arc;
use std::time::Duration;

use khive_storage::{SqlAccess, SqlStatement, StorageCapability, StorageError};

use crate::stores::run_pooled_store_read;
use crate::{ConnectionPool, PoolConfig, SqlBridge};

const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(3);
const QUEUE_SETTLE: Duration = Duration::from_millis(250);
const WAITING_READS: u64 = 3;
const TYPED_CAPABILITY: StorageCapability = StorageCapability::Entities;
const TYPED_OPERATION: &str = "typed_probe";
const TYPED_PROBE_SQL: &str = "SELECT 2 AS typed_read_probe";
const RAW_PROBE_SQL: &str = "SELECT 1 AS raw_read_probe";

/// A runtime whose blocking pool holds one thread, so a read that parks it
/// makes every other blocking task wait its turn.
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
        path: Some(dir.path().join("raw_sql_reader_admission.db")),
        max_readers: 1,
        checkout_timeout: CHECKOUT_TIMEOUT,
        ..PoolConfig::default()
    };
    Arc::new(ConnectionPool::new(config).expect("open the single-reader pool"))
}

/// One raw SQL read through the file-backed bridge's pooled reader route.
async fn raw_read(pool: Arc<ConnectionPool>) -> Result<(), StorageError> {
    let mut reader = SqlBridge::new(pool, true).reader().await?;
    reader
        .query_row(SqlStatement {
            sql: RAW_PROBE_SQL.into(),
            params: vec![],
            label: None,
        })
        .await
        .map(|_| ())
}

/// One typed store read that runs a statement the raw read does not.
async fn typed_read(pool: Arc<ConnectionPool>) -> Result<(), StorageError> {
    run_pooled_store_read(pool, TYPED_CAPABILITY, TYPED_OPERATION, |conn| {
        conn.query_row(TYPED_PROBE_SQL, [], |row| row.get::<_, i64>(0))
            .map(|_| ())
            .map_err(|error| StorageError::driver(TYPED_CAPABILITY, TYPED_OPERATION, error))
    })
    .await
}

/// Queue one typed read and one raw SQL read behind the sole permit, in the
/// given order, release it, and return the order their statements started in.
async fn queued_start_order(typed_first: bool) -> Vec<&'static str> {
    let dir = tempfile::tempdir().unwrap();
    let pool = single_reader_pool(&dir);
    let observation = pool
        .observe_test_statement_starts(64)
        .expect("observe statement starts");
    let held_reader = pool.reader().expect("hold the sole reader permit");

    let mut reads = Vec::new();
    for typed in [typed_first, !typed_first] {
        let pool = Arc::clone(&pool);
        reads.push(if typed {
            tokio::spawn(typed_read(pool))
        } else {
            tokio::spawn(raw_read(pool))
        });
        tokio::time::sleep(QUEUE_SETTLE).await;
    }
    drop(held_reader);
    for read in reads {
        let outcome = read.await.expect("read task panicked");
        assert!(
            outcome.is_ok(),
            "a queued read must be admitted once the permit is free: {outcome:?}"
        );
    }

    observation
        .started_statements()
        .expect("observed statement starts")
        .into_iter()
        .filter_map(|statement| match statement.sql.as_str() {
            TYPED_PROBE_SQL => Some("typed"),
            RAW_PROBE_SQL => Some("raw"),
            _ => None,
        })
        .collect()
}

/// Raw SQL reads that must queue for the sole permit wait as tasks. The permit
/// is held by a checked-out guard on the test task, because a read that
/// blocked inside the runtime would itself take the only blocking thread.
#[test]
fn queued_raw_sql_reads_leave_the_blocking_pool_free_and_still_time_out() {
    let runtime = single_blocking_thread_runtime();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let pool = single_reader_pool(&dir);
        let held_reader = pool.reader().expect("hold the sole reader permit");

        let waiting: Vec<_> = (0..WAITING_READS)
            .map(|_| tokio::spawn(raw_read(Arc::clone(&pool))))
            .collect();
        tokio::time::sleep(QUEUE_SETTLE).await;

        let probe = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(|| "probe ran"),
        )
        .await
        .expect("an unrelated blocking task must not wait behind queued raw SQL reads")
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

/// A raw SQL read and a typed read compete for the same permit in arrival
/// order: whichever queued first starts first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_raw_sql_and_typed_reads_are_admitted_in_arrival_order() {
    assert_eq!(
        queued_start_order(true).await,
        ["typed", "raw"],
        "a typed read queued before a raw SQL read must be admitted first"
    );
    assert_eq!(
        queued_start_order(false).await,
        ["raw", "typed"],
        "a raw SQL read queued before a typed read must be admitted first"
    );
}

/// Cancelling a queued raw SQL read releases it without waiting for the
/// blocking pool or the checkout timeout, and takes no permit from the read
/// behind it.
#[test]
fn cancelling_a_queued_raw_sql_read_returns_at_once_and_leaks_no_permit() {
    let runtime = single_blocking_thread_runtime();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let pool = single_reader_pool(&dir);
        let held_reader = pool.reader().expect("hold the sole reader permit");

        let surviving = tokio::spawn(raw_read(Arc::clone(&pool)));
        // The surviving read queues first, so it is the one that would hold
        // the lone blocking thread while the cancelled read waits behind it.
        tokio::time::sleep(QUEUE_SETTLE).await;
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let cancelled_read = raw_read(Arc::clone(&pool));
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
        let outcome = surviving.await.expect("surviving read task panicked");
        assert!(
            outcome.is_ok(),
            "the surviving read must be admitted once the permit is free: {outcome:?}"
        );
        let snapshot = pool.reader_acquisition_snapshot();
        assert_eq!(snapshot.available_reader_admission_slots, 1);
        assert_eq!(snapshot.checkout_timeouts, 0);
    });
}
