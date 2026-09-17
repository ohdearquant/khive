use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_storage::{SqlAccess, SqlStatement, StorageError};

use crate::{ConnectionPool, PoolConfig, ReaderGuard, ReaderRow, SqlBridge, SqliteError};

const SLOW_READ: &str = "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL \
    SELECT x+1 FROM n WHERE x<10000000) SELECT sum(x) FROM n";

fn owned_pool(file_backed: bool) -> (tempfile::TempDir, Arc<ConnectionPool>) {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        path: file_backed.then(|| dir.path().join("reader-lease.db")),
        max_readers: usize::from(file_backed),
        checkout_timeout: Duration::from_millis(500),
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();
    (dir, Arc::new(pool))
}

fn assert_stopped<T: std::fmt::Debug>(result: Result<T, SqliteError>) {
    assert!(
        matches!(
            result,
            Err(SqliteError::RequestReadStopped(
                StorageError::Timeout { .. }
            ))
        ),
        "expected the pooled reader's typed stop outcome, got {result:?}"
    );
}

#[tokio::test]
async fn reader_lease_pre_stopped_requests_match_sql_reader() {
    for file_backed in [false, true] {
        let (_dir, pool) = owned_pool(file_backed);
        let bridge = SqlBridge::new(Arc::clone(&pool), file_backed);
        for cancelled in [false, true] {
            let (tx, rx) = tokio::sync::watch::channel(cancelled);
            let check = async {
                let called = Cell::new(false);
                {
                    let lease = pool.reader().unwrap();
                    assert_stopped(lease.query_row("SELECT 1", [], |row| {
                        called.set(true);
                        row.get::<_, i64>(0)
                    }));
                }
                assert!(!called.get(), "a stopped request must not enter its mapper");
                let mut reader = bridge.reader().await.unwrap();
                let result = reader
                    .query_row(SqlStatement {
                        sql: "SELECT 1".into(),
                        params: vec![],
                        label: None,
                    })
                    .await;
                assert!(
                    matches!(result, Err(StorageError::Timeout { .. })),
                    "{result:?}"
                );
            };
            if cancelled {
                crate::scope_request_read_cancellation(rx, check).await;
            } else {
                crate::scope_request_read_deadline(Duration::ZERO, check).await;
            }
            drop(tx);
            let lease = pool.reader().unwrap();
            assert_eq!(
                lease
                    .query_row("SELECT 7", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                7
            );
        }
    }
}

#[tokio::test]
async fn reader_lease_interrupts_active_sql_and_reuses_clean_connection() {
    for file_backed in [false, true] {
        let (_dir, pool) = owned_pool(file_backed);
        let lease = pool.reader().unwrap();
        let progress = Arc::new(AtomicUsize::new(0));
        let signal_progress = Arc::clone(&progress);
        let (tx, rx) = tokio::sync::watch::channel(false);
        let signal = std::thread::spawn(move || {
            let started = Instant::now();
            while signal_progress.load(Ordering::Acquire) == 0
                && started.elapsed() < Duration::from_secs(2)
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            let observed = signal_progress.load(Ordering::Acquire) > 0;
            let sent = tx.send(true).is_ok();
            (observed, sent)
        });
        let result = crate::scope_test_read_progress(
            Arc::clone(&progress),
            crate::scope_request_read_cancellation(rx, async {
                lease.query_row(SLOW_READ, [], |row| row.get::<_, i64>(0))
            }),
        )
        .await;
        let (observed, sent) = signal.join().expect("owned cancellation thread");
        assert!(
            observed && sent,
            "cancellation must follow actual SQLite progress"
        );
        assert_stopped(result);
        let stopped_at = progress.load(Ordering::Acquire);
        assert_eq!(
            lease
                .query_row("SELECT 11", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            11
        );
        assert_eq!(progress.load(Ordering::Acquire), stopped_at);

        let deadline_progress = Arc::new(AtomicUsize::new(0));
        let result = crate::scope_test_read_progress(
            Arc::clone(&deadline_progress),
            crate::scope_request_read_deadline(Duration::from_millis(25), async {
                lease.query_row(SLOW_READ, [], |row| row.get::<_, i64>(0))
            }),
        )
        .await;
        assert_stopped(result);
        assert!(deadline_progress.load(Ordering::Acquire) > 0);
        assert_eq!(
            lease
                .query_row("SELECT 13", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            13
        );
    }
}

#[tokio::test]
async fn reader_lease_post_checks_mapper_and_does_not_renew_deadline() {
    let (_dir, pool) = owned_pool(true);
    let lease = pool.reader().unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let called = Cell::new(false);
    let result = crate::scope_request_read_cancellation(rx, async {
        lease.query_row("SELECT 1", [], |row| {
            called.set(true);
            tx.send(true).unwrap();
            row.get::<_, i64>(0)
        })
    })
    .await;
    assert!(called.get());
    assert_stopped(result);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let result = crate::scope_request_read_cancellation(rx, async {
        lease.query_row("SELECT 1", [], |row| {
            tx.send(true).unwrap();
            row.get::<_, i64>("missing")
        })
    })
    .await;
    assert!(matches!(
        result,
        Err(SqliteError::Rusqlite(rusqlite::Error::InvalidColumnName(_)))
    ));

    crate::scope_request_read_deadline(Duration::from_millis(20), async {
        assert_eq!(
            lease
                .query_row("SELECT 2", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_stopped(lease.query_row("SELECT 3", [], |row| row.get::<_, i64>(0)));
    })
    .await;
    let result = crate::scope_request_read_deadline(Duration::from_millis(20), async {
        lease.query_row("SELECT 4", [], |row| {
            std::thread::sleep(Duration::from_millis(30));
            row.get::<_, i64>(0)
        })
    })
    .await;
    assert_stopped(result);
    assert_eq!(
        lease
            .query_row("SELECT 5", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        5
    );
}

#[test]
fn reader_lease_mapper_exposes_values_and_preserves_driver_errors() {
    fn values(row: &ReaderRow<'_, '_>) -> rusqlite::Result<(i64, String)> {
        assert!(matches!(
            row.get_ref("payload")?,
            rusqlite::types::ValueRef::Text(b"ok")
        ));
        Ok((row.get(0)?, row.get("payload")?))
    }
    let (_dir, pool) = owned_pool(true);
    let lease = pool.reader().unwrap();
    assert_eq!(
        lease
            .query_row("SELECT 42, 'ok' AS payload", [], values)
            .unwrap(),
        (42, "ok".into())
    );
    assert!(matches!(
        lease.query_row("SELECT 1 WHERE 0", [], |row| row.get::<_, i64>(0)),
        Err(SqliteError::Rusqlite(rusqlite::Error::QueryReturnedNoRows))
    ));
    assert!(matches!(
        lease.query_row("SELECT 'text'", [], |row| row.get::<_, i64>(0)),
        Err(SqliteError::Rusqlite(rusqlite::Error::InvalidColumnType(
            ..
        )))
    ));
    assert!(matches!(
        lease.query_row("SELECT 1", [], |row| row.get::<_, i64>("missing")),
        Err(SqliteError::Rusqlite(rusqlite::Error::InvalidColumnName(_)))
    ));
    assert!(matches!(
        lease.query_row("BEGIN", [], |row| row.get::<_, i64>(0)),
        Err(SqliteError::InvalidData(_))
    ));
    assert_eq!(
        lease
            .query_row("SELECT 6", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        6
    );
}

#[tokio::test]
async fn reader_lease_cleanup_failure_quarantines_even_during_unwind() {
    for file_backed in [false, true] {
        for panic_mapper in [false, true] {
            let (_dir, pool) = owned_pool(file_backed);
            let lease = pool.reader().unwrap();
            let result = crate::read_cancellation::scope_test_read_cleanup_failure(async {
                catch_unwind(AssertUnwindSafe(|| {
                    lease.query_row("SELECT 1", [], |row| {
                        assert!(!panic_mapper, "owned mapper panic");
                        row.get::<_, i64>(0)
                    })
                }))
            })
            .await;
            if panic_mapper {
                assert!(result.is_err());
            } else {
                assert!(
                    matches!(result.unwrap(), Err(SqliteError::RequestReadStopped(StorageError::Internal(message))) if message.contains("clear failure"))
                );
            }
            let called = Cell::new(false);
            let reuse = lease.query_row("SELECT 2", [], |row| {
                called.set(true);
                row.get::<_, i64>(0)
            });
            assert!(
                matches!(reuse, Err(SqliteError::InvalidData(message)) if message.contains("quarantined"))
            );
            assert!(!called.get());
            drop(lease);
            if file_backed {
                assert_eq!(pool.available_readers(), 1);
                let replacement = pool.reader().unwrap();
                assert_eq!(
                    replacement
                        .query_row("SELECT 3", [], |row| row.get::<_, i64>(0))
                        .unwrap(),
                    3
                );
            } else {
                assert!(pool.try_writer().is_err(), "shared writer must be retired");
                assert!(pool.reader().is_err());
            }
        }
    }
}

#[test]
fn reader_lease_mapper_panic_with_successful_cleanup_keeps_lease_usable() {
    for file_backed in [false, true] {
        let (_dir, pool) = owned_pool(file_backed);
        let lease = pool.reader().unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            lease.query_row("SELECT 1", [], |_| -> rusqlite::Result<i64> {
                panic!("owned mapper panic")
            })
        }));
        assert!(result.is_err());
        assert_eq!(
            lease
                .query_row("SELECT 9", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            9
        );
    }
}

#[tokio::test]
async fn reader_lease_parameter_reentry_cannot_remove_outer_progress_handler() {
    struct NestedReadParameter<'a, 'pool> {
        lease: &'a ReaderGuard<'pool>,
        outcome: &'a RefCell<Option<Result<i64, SqliteError>>>,
        cancellation: Option<&'a tokio::sync::watch::Sender<bool>>,
    }

    impl rusqlite::types::ToSql for NestedReadParameter<'_, '_> {
        fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
            // A converter can swallow an inner refusal and still bind a value.
            self.outcome.replace(Some(
                self.lease
                    .query_row("SELECT 17", [], |row| row.get::<_, i64>(0)),
            ));
            if let Some(tx) = self.cancellation {
                tx.send(true).unwrap();
            }
            Ok(rusqlite::types::ToSqlOutput::Owned(
                rusqlite::types::Value::Integer(0),
            ))
        }
    }

    for file_backed in [false, true] {
        let (_dir, pool) = owned_pool(file_backed);
        let lease = pool.reader().unwrap();
        let outcome = RefCell::new(None);
        let (tx, rx) = tokio::sync::watch::channel(false);
        let parameter = NestedReadParameter {
            lease: &lease,
            outcome: &outcome,
            cancellation: Some(&tx),
        };
        let mapper_called = Cell::new(false);
        let progress = Arc::new(AtomicUsize::new(0));
        let result = crate::scope_test_read_progress(
            Arc::clone(&progress),
            crate::scope_request_read_cancellation(rx, async {
                lease.query_row(
                    "WITH RECURSIVE n(x) AS (VALUES(?1) UNION ALL \
                     SELECT x+1 FROM n WHERE x<10000) SELECT sum(x) FROM n",
                    rusqlite::params![parameter],
                    |row| {
                        mapper_called.set(true);
                        row.get::<_, i64>(0)
                    },
                )
            }),
        )
        .await;
        assert!(
            matches!(outcome.take().unwrap(), Err(SqliteError::InvalidData(message)) if message.contains("already executing"))
        );
        assert_stopped(result);
        assert!(progress.load(Ordering::Acquire) > 0);
        assert!(
            !mapper_called.get(),
            "the outer SQL must stop while stepping, before its mapper"
        );
        assert_eq!(
            lease
                .query_row("SELECT 19", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            19
        );
    }

    // The exclusion is per lease, not a thread-wide ban on nested reads.
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("separate-readers.db")),
        max_readers: 2,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();
    let outer = pool.reader().unwrap();
    let inner = pool.reader().unwrap();
    let outcome = RefCell::new(None);
    let parameter = NestedReadParameter {
        lease: &inner,
        outcome: &outcome,
        cancellation: None,
    };
    assert_eq!(
        outer
            .query_row("SELECT ?1", rusqlite::params![parameter], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    assert_eq!(outcome.take().unwrap().unwrap(), 17);
}
