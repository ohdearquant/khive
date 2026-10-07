use super::*;
use crate::writer_busy_fixture::{assert_direct_busy, Fixture, INSERT};
use khive_storage::SqlAccess;

fn statement(sql: &str) -> SqlStatement {
    SqlStatement {
        sql: sql.to_string(),
        params: vec![],
        label: None,
    }
}

fn callback_cause(error: &StorageError, standalone: bool) -> &StorageError {
    if standalone {
        error
    } else {
        let StorageError::WriterTaskRequestFailed {
            request_state,
            source,
        } = error
        else {
            panic!("pooled callback classification changed: {error:?}");
        };
        assert_eq!(
            *request_state,
            khive_storage::WriterTaskRequestState::TransactionRolledBack
        );
        source
    }
}

/// The pooled route serves in-memory pools only: a file-backed pool takes the
/// guarded route whatever the legacy hint says, so pooled arms run on this.
fn memory_pool() -> Arc<ConnectionPool> {
    let pool = Arc::new(
        ConnectionPool::new(crate::pool::PoolConfig {
            busy_timeout: std::time::Duration::from_millis(50),
            ..crate::pool::PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.try_writer()
        .unwrap()
        .execute_batch("CREATE TABLE direct_busy_fixture (id INTEGER PRIMARY KEY)")
        .unwrap();
    pool
}

/// The unit's primary pool: a file fixture for the guarded route, memory for the
/// pooled one. The fixture is returned so its directory outlives the pool.
fn primary(standalone: bool) -> (Option<Fixture>, Arc<ConnectionPool>) {
    let fixture = standalone.then(|| Fixture::new(true, false));
    let pool = fixture
        .as_ref()
        .map_or_else(memory_pool, |fixture| Arc::clone(&fixture.pool));
    (fixture, pool)
}

macro_rules! sql_case {
    ($name:ident, $method:ident, $argument:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            assert_direct_busy(|pool, standalone| async move {
                let bridge = SqlBridge::new(pool, standalone);
                let mut writer = bridge.writer().await?;
                writer.$method($argument).await.map(|_| ())
            })
            .await;
        }
    };
}

sql_case!(direct_busy_sql_execute, execute, statement(INSERT));
sql_case!(
    direct_busy_sql_batch,
    execute_batch,
    vec![statement(INSERT)]
);
sql_case!(direct_busy_sql_script, execute_script, INSERT.to_string());

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_busy_sql_top_level_vacuum() {
    // PoolBackedWriter uses the trait default; the file-backed method is explicit.
    assert_direct_busy(|pool, standalone| async move {
        let bridge = SqlBridge::new(pool, standalone);
        bridge
            .writer()
            .await?
            .execute_script_top_level(TopLevelMaintenance::Vacuum)
            .await
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_busy_manual_atomic_unit_counts_once() {
    assert_direct_busy(|pool, standalone| async move {
        let bridge = SqlBridge::new(pool, standalone);
        bridge
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    writer.execute(statement(INSERT)).await?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await
            .map(|_| ())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_atomic_absorbed_inner_busy_and_unknown_outcome_add_zero() {
    // ATTACH after the outer BEGIN makes the second physical file's lock a real
    // inner execution refusal, while the original database remains writable.
    for standalone in [false, true] {
        for mode in [0, 1, 2] {
            let (_primary, pool) = primary(standalone);
            let blocked = Fixture::new(true, false);
            let holder = blocked.lock(false);
            let blocked_path = blocked
                .pool
                .config()
                .path
                .as_ref()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let bridge = SqlBridge::new(Arc::clone(&pool), standalone);
            let result = bridge
                .atomic_unit(Box::new(move |writer| {
                    Box::pin(async move {
                        writer
                            .execute(SqlStatement {
                                sql: "ATTACH DATABASE ?1 AS blocked".to_string(),
                                params: vec![SqlValue::Text(blocked_path)],
                                label: None,
                            })
                            .await?;
                        let error = writer
                            .execute(statement(
                                "INSERT INTO blocked.direct_busy_fixture VALUES (2)",
                            ))
                            .await
                            .unwrap_err();
                        assert_eq!(
                            crate::read_cancellation::storage_error_sqlite_code(&error),
                            Some(rusqlite::ErrorCode::DatabaseBusy)
                        );
                        match mode {
                            0 => Ok(Box::new(()) as Box<dyn Any + Send>),
                            1 => Err(error),
                            _ => Err(StorageError::WriterTaskTerminated {
                                request_state:
                                    khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                            }),
                        }
                    })
                }))
                .await;
            assert!(
                !holder.is_autocommit(),
                "lock spans inner execution and unit cleanup"
            );
            let snapshot = pool.writer_acquisition_snapshot();
            match mode {
                0 => {
                    result.unwrap();
                    assert_eq!(
                        snapshot.direct_busy_refusals, 0,
                        "absorbed inner BUSY is excluded"
                    );
                }
                1 => {
                    let error = result.unwrap_err();
                    assert_eq!(
                        crate::read_cancellation::storage_error_sqlite_code(callback_cause(
                            &error, standalone
                        )),
                        Some(rusqlite::ErrorCode::DatabaseBusy)
                    );
                    assert_eq!(
                        snapshot.direct_busy_refusals, 1,
                        "final BUSY counts once, excluding inner execution and rollback cleanup"
                    );
                }
                _ => {
                    let error = result.unwrap_err();
                    assert!(matches!(
                        callback_cause(&error, standalone),
                        StorageError::WriterTaskTerminated {
                            request_state:
                                khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                        }
                    ));
                    assert_eq!(
                        snapshot.direct_busy_refusals, 0,
                        "cause-free unknown outcome is excluded"
                    );
                }
            }
            let guard = pool.try_writer().unwrap();
            assert!(guard.is_autocommit(), "outer unit cleanup did not finish");
            let rows: i64 = guard
                .query_row("SELECT COUNT(*) FROM direct_busy_fixture", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(rows, 0);
            drop(guard);
            assert_eq!(
                blocked
                    .pool
                    .writer_acquisition_snapshot()
                    .direct_busy_refusals,
                0
            );
            holder.execute_batch("ROLLBACK").unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poisoned_batch_wrapper_preserves_real_busy_cause_and_counts_once() {
    let fixture = Fixture::new(true, false);
    let conn = fixture.pool.open_standalone_writer().unwrap();
    let holder = fixture.lock(false);
    let raw = conn.execute(INSERT, []).unwrap_err();
    assert_eq!(
        raw.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    let writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: None,
        writer_task: None,
        origin: fixture.pool.origin(),
        db: crate::timeout_sink::db_label(&fixture.pool),
        pool: Arc::clone(&fixture.pool),
        held_lease: None,
    };
    let mapped = writer.map_direct_batch_failure(BatchFailure {
        error: raw,
        poison_reason: Some(BatchPoisonReason::RollbackFailed(
            rusqlite::Error::InvalidQuery,
        )),
    });
    let StorageError::Driver { source, .. } = &mapped else {
        panic!("wrapper changed");
    };
    let preserved = source
        .downcast_ref::<PoisonedBatchError>()
        .expect("original wrapper retained");
    assert_eq!(
        preserved.original.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert_eq!(
        fixture
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        1
    );
    assert!(!holder.is_autocommit());
    holder.execute_batch("ROLLBACK").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_sql_and_atomic_refusals_do_not_double_count() {
    let fixture = Fixture::new(true, true);
    let bridge = SqlBridge::new(Arc::clone(&fixture.pool), true);
    let mut writer = bridge.writer().await.unwrap();
    let holder = fixture.lock(false);
    let error = writer.execute(statement(INSERT)).await.unwrap_err();
    assert!(matches!(error, StorageError::WriterTaskBusy { .. }));
    let error = bridge
        .atomic_unit(Box::new(|writer| {
            Box::pin(async move {
                writer.execute(statement(INSERT)).await?;
                Ok(Box::new(()) as Box<dyn Any + Send>)
            })
        }))
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::WriterTaskBusy { .. }));
    let snapshot = fixture.pool.writer_acquisition_snapshot();
    assert_eq!(snapshot.writer_task_begin_busy, 2);
    assert_eq!(snapshot.direct_busy_refusals, 0);
    assert!(!holder.is_autocommit());
    holder.execute_batch("ROLLBACK").unwrap();
    writer.execute(statement(INSERT)).await.unwrap();
    assert_eq!(
        fixture
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_atomic_cyclic_source_returns_original_error_and_rolls_back() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct CyclicError(Arc<AtomicUsize>);
    impl std::fmt::Display for CyclicError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("cyclic callback error")
        }
    }
    impl std::error::Error for CyclicError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            let calls = self.0.fetch_add(1, Ordering::Relaxed) + 1;
            // An unbounded observer fails this assertion before it can hang.
            assert!(
                calls <= 32,
                "direct observer exceeded its source-call budget"
            );
            Some(self)
        }
    }

    for standalone in [false, true] {
        let (_primary, pool) = primary(standalone);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let bridge = SqlBridge::new(Arc::clone(&pool), standalone);
        let error = bridge
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    writer.execute(statement(INSERT)).await?;
                    Err(StorageError::driver(
                        StorageCapability::Sql,
                        "cyclic_callback",
                        CyclicError(callback_calls),
                    ))
                })
            }))
            .await
            .unwrap_err();
        let inner = if standalone {
            error
        } else {
            let StorageError::WriterTaskRequestFailed {
                request_state,
                source,
            } = error
            else {
                panic!("pooled callback error classification changed");
            };
            assert_eq!(
                request_state,
                khive_storage::WriterTaskRequestState::TransactionRolledBack
            );
            *source
        };
        let StorageError::Driver {
            source,
            operation,
            capability,
        } = inner
        else {
            panic!("callback error classification changed");
        };
        assert_eq!(capability, StorageCapability::Sql);
        assert_eq!(operation, "cyclic_callback");
        let original = source.downcast_ref::<CyclicError>().unwrap();
        assert!(Arc::ptr_eq(&original.0, &calls), "callback cause replaced");
        // StorageError is node one; pooled execution adds its rollback wrapper.
        assert_eq!(
            calls.load(Ordering::Relaxed),
            if standalone { 31 } else { 30 }
        );
        assert_eq!(pool.writer_acquisition_snapshot().direct_busy_refusals, 0);
        let guard = pool.try_writer().unwrap();
        assert!(guard.is_autocommit(), "manual rollback did not finish");
        let rows: i64 = guard
            .query_row("SELECT COUNT(*) FROM direct_busy_fixture", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "callback write escaped rollback");
    }
}
