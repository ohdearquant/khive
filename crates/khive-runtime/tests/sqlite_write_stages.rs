//! Native failures at actual transaction boundaries retain stage and code evidence.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use khive_db::{writer_task, ConnectionPool, PoolConfig, SqlBridge};
use khive_runtime::{runtime_error_value, DomainDisposition, RuntimeError};
use khive_storage::{
    error::SqliteWriteStage, SqlAccess, SqlStatement, StorageCapability, StorageError,
};

fn fixture_config(path: PathBuf) -> PoolConfig {
    PoolConfig {
        volume_lock_dir: Some(path.with_extension("locks")),
        path: Some(path),
        code_map_vfs: None,
        #[cfg(any(unix, windows))]
        expected_file_identity: None,
        max_readers: 1,
        reader_max_age: Duration::from_secs(300),
        reader_max_ops: 5000,
        wal_mode: true,
        busy_timeout: Duration::from_millis(20),
        checkout_timeout: Duration::from_secs(1),
        reader_checkout_warn_after: Duration::from_secs(10),
        journal_size_limit_bytes: 64 * 1024 * 1024,
        read_only: false,
        wal_ceiling: khive_db::WalCeilingPolicy {
            bytes: 0,
            source: khive_db::WalCeilingSource::BackendField,
        },
        write_queue_enabled: Some(false),
        write_queue_capacity: 8,
        write_routing_strict: false,
        write_admission_deadline_ms: 2000,
        disk_guard_config: Some(khive_db::EffectiveDiskGuardConfig {
            reserve_bytes: 0,
            guard_deadline_ms: 2000,
            reserve_source: khive_db::DiskGuardConfigSource::Backend,
            deadline_source: khive_db::DiskGuardConfigSource::Backend,
            legacy_environment_present: false,
        }),
        read_tx_max_age: Duration::from_secs(120),
    }
}

fn isolate(marker: &str, sink: &std::path::Path) -> bool {
    khive_storage::test_support::run_exact_test_in_child(marker, false, |command| {
        command.env("KHIVE_TEST_HARNESS", "1");
        command.env("KHIVE_WRITER_TIMEOUT_SINK_DIR", sink);
        command.env("KHIVE_WRITER_TIMEOUT_SINK_HEARTBEAT_MS", "100");
        command.env("KHIVE_WRITER_TIMEOUT_SINK_WRITE_DELAY_MS", "0");
        command.env("KHIVE_SLOW_WRITE_THRESHOLD_MS", "0");
        command.env_remove("KHIVE_WRITER_TIMEOUT_SINK_STARTUP_BARRIER_DIR");
    })
}

fn driver(error: rusqlite::Error) -> StorageError {
    StorageError::driver(StorageCapability::Sql, "stage_fixture", error)
}

fn statement(sql: &str) -> SqlStatement {
    SqlStatement::new(sql, vec![])
}

fn assert_wire(error: StorageError, stage: SqliteWriteStage, extended: i32, queued: bool) {
    let failure = error
        .sqlite_write_failure()
        .expect("native boundary evidence");
    assert_eq!(failure.stage, stage);
    assert_eq!(failure.primary_code, extended & 0xff);
    assert_eq!(failure.extended_code, extended);
    let retryable = error.is_retryable();
    let error = RuntimeError::from(error);
    let message = error.to_string();
    let value = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(value["stage"], stage.as_str());
    assert_eq!(value["code"], stage.as_str());
    assert_eq!(value["sqlite_primary_code"], extended & 0xff);
    assert_eq!(value["sqlite_extended_code"], extended);
    assert_eq!(value["message"], message);
    if queued {
        assert_eq!(value["retryable"], retryable);
        if stage != SqliteWriteStage::Begin {
            assert_eq!(value["request_state"], "transaction_rolled_back");
            assert_eq!(value["task_terminated"], false);
        }
    }
}

#[test]
fn native_begin_statement_and_commit_failures_reach_the_wire() {
    let sink = tempfile::tempdir().unwrap();
    if isolate("KHIVE_SQLITE_WRITE_STAGES_CHILD", sink.path()) {
        return;
    }
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
        for queued in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("stages.db");
            let pool = Arc::new(ConnectionPool::new(fixture_config(path.clone())).unwrap());
            pool.writer().unwrap().execute_batch(
                "CREATE TABLE parent(id INTEGER PRIMARY KEY); \
                 CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);"
            ).unwrap();
            let handle = queued.then(|| writer_task::spawn(&pool, 8).unwrap());
            let bridge = SqlBridge::new(Arc::clone(&pool), true);
            let mut writer = bridge.writer().await.unwrap();
            let lock = rusqlite::Connection::open(&path).unwrap();
            lock.execute_batch("BEGIN IMMEDIATE").unwrap();
            let ran = Arc::new(AtomicBool::new(false));
            let error = if let Some(handle) = &handle {
                let ran = Arc::clone(&ran);
                handle.send(move |conn| {
                    ran.store(true, Ordering::SeqCst);
                    conn.execute("INSERT INTO parent VALUES (1)", []).map_err(driver)
                }).await.unwrap_err()
            } else {
                writer.execute_batch(vec![statement("INSERT INTO parent VALUES (1)")]).await.unwrap_err()
            };
            assert!(!ran.load(Ordering::SeqCst), "queued body must not run after BEGIN refusal");
            assert!(!lock.is_autocommit());
            assert_wire(error, SqliteWriteStage::Begin, rusqlite::ffi::SQLITE_BUSY, queued);
            lock.execute_batch("ROLLBACK").unwrap();

            let error = if let Some(handle) = &handle {
                handle.send(|conn| {
                    conn.execute("INSERT INTO parent VALUES (2)", []).map_err(driver)?;
                    conn.execute("INSERT INTO parent VALUES (2)", []).map_err(driver)
                }).await.unwrap_err()
            } else {
                writer.execute_batch(vec![statement("INSERT INTO parent VALUES (2)"), statement("INSERT INTO parent VALUES (2)")]).await.unwrap_err()
            };
            assert_wire(error, SqliteWriteStage::Statement, rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY, queued);
            assert_eq!(lock.query_row("SELECT COUNT(*) FROM parent", [], |row| row.get::<_, i64>(0)).unwrap(), 0);

            let error = if let Some(handle) = &handle {
                handle.send(|conn| conn.execute("INSERT INTO child VALUES (99)", []).map_err(driver)).await.unwrap_err()
            } else {
                writer.execute_batch(vec![statement("INSERT INTO child VALUES (99)")]).await.unwrap_err()
            };
            assert_wire(error, SqliteWriteStage::Commit, rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY, queued);
            assert_eq!(lock.query_row("SELECT COUNT(*) FROM child", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            // Both handles remain useful after definite rollback, and successful writes persist.
            if let Some(handle) = &handle {
                let error = handle.send::<(), _>(|_| Err(StorageError::InvalidInput {
                    capability: StorageCapability::Sql, operation: "control".into(), message: "not a SQLite error".into(),
                })).await.unwrap_err();
                assert!(error.sqlite_write_failure().is_none());
                assert!(!error.is_retryable());
                handle.send(|conn| conn.execute("INSERT INTO parent VALUES (3)", []).map_err(driver)).await.unwrap();
            } else {
                writer.execute(statement("INSERT INTO parent VALUES (3)")).await.unwrap();
            }
            assert_eq!(lock.query_row("SELECT COUNT(*) FROM parent", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
        }
    });
}

#[test]
fn pooled_guard_preserves_native_stages_and_read_errors_stay_unstaged() {
    let dir = tempfile::tempdir().unwrap();
    if isolate("KHIVE_SQLITE_POOLED_STAGES_CHILD", dir.path()) {
        return;
    }
    let pool = ConnectionPool::new(fixture_config(dir.path().join("pooled.db"))).unwrap();
    let guard = pool.writer().unwrap();
    guard.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)").unwrap();
    let statement = guard
        .transaction(|conn| {
            conn.execute("INSERT INTO parent VALUES (1)", [])?;
            conn.execute("INSERT INTO parent VALUES (1)", [])?;
            Ok(())
        })
        .unwrap_err();
    assert_eq!(
        statement.write_failure().unwrap().stage,
        SqliteWriteStage::Statement
    );
    let commit = guard
        .transaction(|conn| {
            conn.execute("INSERT INTO child VALUES (2)", [])?;
            Ok(())
        })
        .unwrap_err();
    let value = runtime_error_value(RuntimeError::from(commit), DomainDisposition::Unknown);
    assert_eq!(value["stage"], "sqlite_commit_failure");
    assert_eq!(
        value["sqlite_extended_code"],
        rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
    );
    let read = guard
        .query_row::<i64, _, _>("SELECT missing_column", [], |row| row.get(0))
        .unwrap_err();
    let read = runtime_error_value(
        RuntimeError::from(khive_db::SqliteError::from(read)),
        DomainDisposition::Unknown,
    );
    assert!(read.get("stage").is_none());
    assert!(read.get("sqlite_primary_code").is_none());
}

#[test]
fn failed_pooled_settlement_retains_cause_without_claiming_definite_commit_failure() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    fn deny_commit_and_rollback(context: AuthContext<'_>) -> Authorization {
        match context.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Unknown | TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }
    fn deny_rollback(context: AuthContext<'_>) -> Authorization {
        match context.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }
    let dir = tempfile::tempdir().unwrap();
    if isolate("KHIVE_SQLITE_POOLED_UNKNOWN_CHILD", dir.path()) {
        return;
    }
    for commit in [false, true] {
        let pool = ConnectionPool::new(fixture_config(
            dir.path().join(format!("unknown-{commit}.db")),
        ))
        .unwrap();
        let guard = pool.writer().unwrap();
        guard
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY)")
            .unwrap();
        let error = guard
            .transaction(|conn| {
                conn.execute("INSERT INTO t VALUES (1)", [])?;
                if commit {
                    conn.authorizer(Some(deny_commit_and_rollback))?;
                } else {
                    conn.authorizer(Some(deny_rollback))?;
                    conn.execute("INSERT INTO t VALUES (1)", [])?;
                }
                Ok(())
            })
            .unwrap_err();
        let expected_stage = if commit {
            SqliteWriteStage::Commit
        } else {
            SqliteWriteStage::Statement
        };
        let expected_code = if commit {
            rusqlite::ffi::SQLITE_AUTH
        } else {
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
        };
        let failure = error
            .write_failure()
            .expect("original native evidence survives failed rollback");
        assert_eq!(failure.stage, expected_stage);
        assert_eq!(failure.primary_code, expected_code & 0xff);
        assert_eq!(failure.extended_code, expected_code);
        assert!(failure.settlement_unknown);
        assert!(matches!(
            error,
            khive_db::SqliteError::WriteSettlementUnknown { .. }
        ));
        let value = runtime_error_value(error.into(), DomainDisposition::Unknown);
        assert_eq!(value["code"], "writer_task_terminated");
        assert_eq!(value["stage"], "writer_task_terminated");
        assert_eq!(value["sqlite_write_stage"], expected_stage.as_str());
        assert_eq!(value["sqlite_primary_code"], expected_code & 0xff);
        assert_eq!(value["sqlite_extended_code"], expected_code);
        assert_eq!(value["request_state"], "side_effects_unknown");
        assert_eq!(value["task_terminated"], true);
        assert_eq!(value["retryable"], false);
        drop(guard);
        assert!(matches!(
            pool.writer(),
            Err(khive_db::SqliteError::InvalidData(message))
                if message == "pooled writer connection retired after a terminal transaction fault"
        ));
    }
}
