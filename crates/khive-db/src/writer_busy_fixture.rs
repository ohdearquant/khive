//! Real-file refusal fixtures shared by the instrumented production routes.
use crate::pool::{ConnectionPool, PoolConfig};
use khive_storage::{StorageCapability, StorageError};
use std::{future::Future, path::PathBuf, sync::Arc, time::Duration};

pub(crate) const INSERT: &str = "INSERT OR REPLACE INTO direct_busy_fixture VALUES (1)";
const COUNT: &str = "SELECT COUNT(*) FROM direct_busy_fixture";

pub(crate) struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    pub(crate) pool: Arc<ConnectionPool>,
}

impl Fixture {
    pub(crate) fn new(wal_mode: bool, queued: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("direct_busy.db");
        let pool = Arc::new(
            // Each fixture busy-waits on an external SQLite lock while it holds
            // the volume lease; a lock directory of its own keeps that hold off
            // the lease the other tests in this process share.
            ConnectionPool::new(PoolConfig {
                path: Some(path.clone()),
                volume_lock_dir: Some(dir.path().join("volume-locks")),
                wal_mode,
                busy_timeout: Duration::from_millis(50),
                checkout_timeout: Duration::from_secs(2),
                write_queue_enabled: Some(queued),
                write_routing_strict: false,
                ..PoolConfig::for_test()
            })
            .unwrap(),
        );
        pool.try_writer()
            .unwrap()
            .execute_batch("CREATE TABLE direct_busy_fixture (id INTEGER PRIMARY KEY)")
            .unwrap();
        Self {
            _dir: dir,
            path,
            pool,
        }
    }

    pub(crate) fn lock(&self, exclusive: bool) -> rusqlite::Connection {
        let holder = rusqlite::Connection::open(&self.path).unwrap();
        holder
            .execute_batch(if exclusive {
                "BEGIN EXCLUSIVE"
            } else {
                "BEGIN IMMEDIATE"
            })
            .unwrap();
        assert!(!holder.is_autocommit());
        holder
    }

    fn rows(&self) -> i64 {
        self.pool
            .try_writer()
            .unwrap()
            .query_row(COUNT, [], |r| r.get(0))
            .unwrap()
    }
}

pub(crate) fn insert(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute(INSERT, []).map(|_| ())
}

/// Every positive route is called while a separate SQLite connection owns the
/// physical write lock, then called successfully after that lock is released.
/// No elapsed-time threshold substitutes for the returned SQLite code.
pub(crate) async fn assert_direct_busy<F, Fut>(operation: F)
where
    F: Fn(Arc<ConnectionPool>, bool) -> Fut,
    Fut: Future<Output = Result<(), StorageError>>,
{
    for standalone in [false, true] {
        let fixture = Fixture::new(true, false);
        let other = Fixture::new(true, false);
        operation(Arc::clone(&fixture.pool), standalone)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .pool
                .writer_acquisition_snapshot()
                .direct_busy_refusals,
            0
        );
        let before_rows = fixture.rows();
        let holder = fixture.lock(false);
        let error = operation(Arc::clone(&fixture.pool), standalone)
            .await
            .unwrap_err();
        assert_eq!(
            crate::read_cancellation::storage_error_sqlite_code(&error),
            Some(rusqlite::ErrorCode::DatabaseBusy),
            "actual execution refusal: {error:?}"
        );
        assert!(
            !holder.is_autocommit(),
            "external lock must span the actual operation"
        );
        let after = fixture.pool.writer_acquisition_snapshot();
        assert_eq!(after.direct_busy_refusals, 1, "one final direct refusal");
        assert_eq!(after.writer_task_begin_busy, 0);
        assert_eq!(fixture.pool.reader_acquisition_snapshot().busy_timeouts, 0);
        assert_eq!(
            other
                .pool
                .writer_acquisition_snapshot()
                .direct_busy_refusals,
            0,
            "another physical pool must remain isolated"
        );
        assert_eq!(
            fixture.rows(),
            before_rows,
            "refused execution must not change the row"
        );
        holder.execute_batch("ROLLBACK").unwrap();
        operation(Arc::clone(&fixture.pool), standalone)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .pool
                .writer_acquisition_snapshot()
                .direct_busy_refusals,
            1,
            "successful execution after release must add zero"
        );
    }
}

macro_rules! direct_busy_case {
    ($name:ident, $operation:expr) => {
        #[cfg(test)]
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            crate::writer_busy_fixture::assert_direct_busy($operation).await;
        }
    };
}
pub(crate) use direct_busy_case;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_busy_and_queued_write_refusals_do_not_count_as_direct() {
    use khive_storage::{SqlAccess, SqlStatement};
    let reader_fixture = Fixture::new(false, false);
    let bridge = crate::SqlBridge::new(Arc::clone(&reader_fixture.pool), true);
    let mut reader = bridge.reader().await.unwrap();
    let holder = reader_fixture.lock(true);
    let error = reader
        .query_scalar(SqlStatement {
            sql: COUNT.to_string(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        crate::read_cancellation::storage_error_sqlite_code(&error),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert_eq!(
        reader_fixture
            .pool
            .reader_acquisition_snapshot()
            .busy_timeouts,
        1
    );
    assert_eq!(
        reader_fixture
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0
    );
    assert!(!holder.is_autocommit());
    holder.execute_batch("ROLLBACK").unwrap();

    let queued = Fixture::new(true, true);
    let handle = queued.pool.writer_task_handle().unwrap().unwrap();
    let holder = queued.lock(false);
    let error = handle
        .send_bounded(|conn| {
            insert(conn)
                .map_err(|error| StorageError::driver(StorageCapability::Sql, "fixture", error))
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            error.without_sqlite_write_stage(),
            StorageError::WriterTaskBusy { .. }
        ),
        "{error:?}"
    );
    let snapshot = queued.pool.writer_acquisition_snapshot();
    assert_eq!(snapshot.writer_task_begin_busy, 1);
    assert_eq!(snapshot.writer_task_begin_busy_absorbed, 0);
    assert_eq!(snapshot.direct_busy_refusals, 0);
    assert!(!holder.is_autocommit());
    holder.execute_batch("ROLLBACK").unwrap();
    handle
        .send_bounded(|conn| {
            insert(conn)
                .map_err(|error| StorageError::driver(StorageCapability::Sql, "fixture", error))
        })
        .await
        .unwrap();
    assert_eq!(
        queued
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0
    );
}

/// A second file remains externally locked after the first file's BEGIN has
/// succeeded, exercising final body-error observation rather than only BEGIN.
pub(crate) async fn assert_transaction_body_busy<F, Fut>(operation: F)
where
    F: Fn(Arc<ConnectionPool>, String) -> Fut,
    Fut: Future<Output = Result<(), StorageError>>,
{
    let primary = Fixture::new(true, false);
    let blocked = Fixture::new(true, false);
    let holder = blocked.lock(false);
    let path = blocked
        .pool
        .config()
        .path
        .as_ref()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let error = operation(Arc::clone(&primary.pool), path)
        .await
        .unwrap_err();
    let StorageError::WriterTaskRequestFailed {
        request_state,
        source,
    } = &error
    else {
        panic!("wrapped body error changed: {error:?}");
    };
    assert_eq!(
        *request_state,
        khive_storage::WriterTaskRequestState::TransactionRolledBack
    );
    // Inspect the preserved body cause independently of the writer observer.
    assert_eq!(
        crate::read_cancellation::storage_error_sqlite_code(source),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert_eq!(
        primary
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        1
    );
    assert_eq!(
        blocked
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0
    );
    assert!(
        primary.pool.try_writer().unwrap().is_autocommit(),
        "rollback must retain a reusable writer"
    );
    assert!(!holder.is_autocommit());
    holder.execute_batch("ROLLBACK").unwrap();

    // A rollback-journal reader permits the body write but blocks COMMIT.
    // Native COMMIT evidence wraps the existing Pool representation without
    // changing the source-chain-based direct BUSY counter.
    let commit_primary = Fixture::new(true, false);
    let commit_blocked = Fixture::new(false, false);
    let reader = rusqlite::Connection::open(&commit_blocked.path).unwrap();
    let journal: String = reader
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "delete", "COMMIT contention fixture setup");
    reader.execute_batch("BEGIN").unwrap();
    let rows: i64 = reader.query_row(COUNT, [], |row| row.get(0)).unwrap();
    assert_eq!(rows, 0);
    let error = operation(
        Arc::clone(&commit_primary.pool),
        commit_blocked.path.to_string_lossy().into_owned(),
    )
    .await
    .unwrap_err();
    let StorageError::WriterTaskRequestFailed {
        request_state,
        source,
    } = &error
    else {
        panic!("wrapped COMMIT error changed: {error:?}");
    };
    assert_eq!(
        *request_state,
        khive_storage::WriterTaskRequestState::TransactionRolledBack
    );
    assert!(
        matches!(
            source.without_sqlite_write_stage(),
            StorageError::Pool { .. }
        ),
        "COMMIT evidence must preserve the existing pool error"
    );
    assert_eq!(
        error.sqlite_write_failure(),
        Some(khive_storage::error::SqliteWriteFailure {
            stage: khive_storage::error::SqliteWriteStage::Commit,
            primary_code: rusqlite::ffi::SQLITE_BUSY,
            extended_code: rusqlite::ffi::SQLITE_BUSY,
            settlement_unknown: false,
        })
    );
    assert_eq!(
        commit_primary
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0,
        "COMMIT metadata does not add a native SQLite cause to the source chain"
    );
    assert!(commit_primary.pool.try_writer().unwrap().is_autocommit());
    assert!(
        !reader.is_autocommit(),
        "read lock spans COMMIT and rollback"
    );
    let rows: i64 = reader.query_row(COUNT, [], |row| row.get(0)).unwrap();
    assert_eq!(
        rows, 0,
        "COMMIT refusal must roll back the completed body write"
    );
    assert_eq!(
        commit_blocked
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0
    );
    reader.execute_batch("ROLLBACK").unwrap();
}

pub(crate) fn insert_blocked(conn: &rusqlite::Connection, path: &str) -> rusqlite::Result<()> {
    conn.execute("ATTACH DATABASE ?1 AS blocked", [path])?;
    conn.execute("INSERT INTO blocked.direct_busy_fixture VALUES (2)", [])
        .map(|_| ())
}
