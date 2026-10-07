//! Reproducers for two writer-settlement hazards. Each test asserts the
//! settled behaviour, so it fails on the code as it stands and names the
//! hazard in its failure message.

use std::sync::Arc;

use khive_storage::types::SqlStatement;
use khive_storage::SqlReader;

use super::{open_standalone_writer, SqlBridge, SqliteWriter, StandaloneHandle};
use crate::disk_guard_config::EffectiveDiskGuardConfig;
use crate::pool::{ConnectionPool, PoolConfig};

fn statement(sql: &str) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params: vec![],
        label: None,
    }
}

/// (a) A `BEGIN` sent through the pool-backed writer of an in-memory pool must
/// not report success and then be undone when the pooled guard drops: either
/// the `BEGIN` is refused, or the transaction it opened is still there for the
/// caller's `ROLLBACK`, which then removes the row written inside it.
#[tokio::test]
async fn pool_backed_begin_is_refused_or_survives_to_its_rollback() {
    let pool = Arc::new(ConnectionPool::new(PoolConfig::for_test()).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    assert!(
        !bridge.is_file_backed,
        "fixture must take the in-memory writer route"
    );
    let mut writer = khive_storage::SqlAccess::writer(&bridge).await.unwrap();

    writer
        .execute(statement("CREATE TABLE hazard_rows (x INTEGER)"))
        .await
        .unwrap();
    let begin = writer.execute(statement("BEGIN IMMEDIATE")).await;
    if begin.is_err() {
        return;
    }
    writer
        .execute(statement("INSERT INTO hazard_rows (x) VALUES (1)"))
        .await
        .unwrap();
    let rollback = writer.execute(statement("ROLLBACK")).await;
    let rows = SqlReader::query_scalar(&mut *writer, statement("SELECT COUNT(*) FROM hazard_rows"))
        .await
        .unwrap();

    assert!(
        rollback.is_ok(),
        "BEGIN reported Ok, yet the caller's ROLLBACK found no transaction: \
         the pooled guard settled it silently ({rollback:?})"
    );
    assert!(
        matches!(rows, Some(khive_storage::types::SqlValue::Integer(0))),
        "a row written inside the caller's transaction survived its ROLLBACK ({rows:?})"
    );
}

/// (b) While a standalone writer's `BEGIN` has left SQLite inside a
/// transaction, the volume lease that admitted it must still be held: another
/// writer on the same volume waits out the guard deadline instead of being
/// admitted beside the open transaction.
#[tokio::test]
async fn standalone_begin_keeps_the_volume_lease_until_autocommit() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("standalone_begin_lease.db")),
            write_queue_enabled: Some(false),
            // A lock directory of its own keeps this deliberate open transaction
            // off the volume lease the other tests in this process share.
            volume_lock_dir: Some(dir.path().join("volume-locks")),
            disk_guard_config: Some(EffectiveDiskGuardConfig {
                guard_deadline_ms: 200,
                ..EffectiveDiskGuardConfig::default()
            }),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    let writer_slot = pool
        .sql_bridge_writer_slots()
        .acquire_owned()
        .await
        .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(writer_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };

    let begin = khive_storage::SqlWriter::execute(&mut writer, statement("BEGIN IMMEDIATE")).await;
    if begin.is_err() {
        return;
    }
    let in_transaction = !writer.handle.as_ref().unwrap().conn.is_autocommit();
    assert!(
        in_transaction,
        "fixture: BEGIN must leave the connection in a transaction"
    );

    let other = Arc::clone(&pool);
    let second = tokio::task::spawn_blocking(move || {
        other
            .write_admission()
            .acquire()
            .map(|lease| lease.is_some())
    })
    .await
    .unwrap();

    khive_storage::SqlWriter::execute(&mut writer, statement("ROLLBACK"))
        .await
        .unwrap();
    assert!(
        second.is_err(),
        "a second writer was admitted to the volume while the first writer's transaction \
         was still open: the standalone execute released the lease after BEGIN ({second:?})"
    );
}

/// (a) Transaction control through a pooled writer's `execute` is refused with
/// a typed error before anything runs, so the reproducer above passes by the
/// refusal and never by a transaction that happens to survive.
#[tokio::test]
async fn pool_backed_execute_refuses_transaction_control() {
    let pool = Arc::new(ConnectionPool::new(PoolConfig::for_test()).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let mut writer = khive_storage::SqlAccess::writer(&bridge).await.unwrap();
    for sql in [
        "BEGIN IMMEDIATE",
        "/* lead */ begin",
        "SAVEPOINT s",
        "COMMIT",
        "ROLLBACK",
    ] {
        let refused = writer.execute(statement(sql)).await;
        assert!(
            matches!(&refused, Err(khive_storage::StorageError::InvalidInput { message, .. })
                if message.contains("transaction control")),
            "{sql}: {refused:?}"
        );
    }
    writer
        .execute(statement("CREATE TABLE after_refusal (x INTEGER)"))
        .await
        .expect("an ordinary statement still runs after the refusals");
}

/// (a) A pooled script that leaves a transaction open is rolled back and
/// refused before its guard is released, so the guard's drop never has to
/// settle it and the write inside the transaction is not kept.
#[tokio::test]
async fn pool_backed_script_left_in_a_transaction_is_rolled_back_and_refused() {
    let pool = Arc::new(ConnectionPool::new(PoolConfig::for_test()).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let mut writer = khive_storage::SqlAccess::writer(&bridge).await.unwrap();
    writer
        .execute(statement("CREATE TABLE script_rows (x INTEGER)"))
        .await
        .unwrap();
    let drops_before = pool
        .writer_acquisition_snapshot()
        .writer_guard_drop_rollbacks;

    let script = writer
        .execute_script("BEGIN; INSERT INTO script_rows (x) VALUES (1);".to_string())
        .await;
    assert!(
        matches!(&script, Err(khive_storage::StorageError::InvalidInput { message, .. })
            if message.contains("left a transaction open")),
        "{script:?}"
    );
    let rows = SqlReader::query_scalar(&mut *writer, statement("SELECT COUNT(*) FROM script_rows"))
        .await
        .unwrap();
    assert!(
        matches!(rows, Some(khive_storage::types::SqlValue::Integer(0))),
        "the row written inside the open transaction was kept ({rows:?})"
    );
    assert_eq!(
        pool.writer_acquisition_snapshot()
            .writer_guard_drop_rollbacks,
        drops_before,
        "the call settled its own transaction; the guard drop had nothing to settle"
    );
    writer
        .execute_script("BEGIN; INSERT INTO script_rows (x) VALUES (2); COMMIT;".to_string())
        .await
        .expect("a script that closes its own transaction still runs");
}

/// (a) A pooled script that fails after opening a transaction keeps its own
/// error, and the transaction is still rolled back before the guard is
/// released.
#[tokio::test]
async fn pool_backed_script_failing_inside_its_transaction_keeps_its_error_and_is_rolled_back() {
    let pool = Arc::new(ConnectionPool::new(PoolConfig::for_test()).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let mut writer = khive_storage::SqlAccess::writer(&bridge).await.unwrap();
    writer
        .execute(statement("CREATE TABLE failing_rows (x INTEGER)"))
        .await
        .unwrap();
    let drops_before = pool
        .writer_acquisition_snapshot()
        .writer_guard_drop_rollbacks;

    let script = writer
        .execute_script(
            "BEGIN; INSERT INTO failing_rows (x) VALUES (1); INSERT INTO no_such_table VALUES (1);"
                .to_string(),
        )
        .await;
    match &script {
        Err(error) => assert!(
            error.to_string().contains("no_such_table"),
            "the statement's own error must be returned: {error:?}"
        ),
        Ok(()) => panic!("a script that names a missing table must fail"),
    }
    let rows =
        SqlReader::query_scalar(&mut *writer, statement("SELECT COUNT(*) FROM failing_rows"))
            .await
            .unwrap();
    assert!(
        matches!(rows, Some(khive_storage::types::SqlValue::Integer(0))),
        "the row written before the failure was kept ({rows:?})"
    );
    assert_eq!(
        pool.writer_acquisition_snapshot()
            .writer_guard_drop_rollbacks,
        drops_before,
        "the call settled its own transaction; the guard drop had nothing to settle"
    );
}

/// (a) When a failed pooled call's transaction cannot be rolled back, the call
/// reports that its side effects are unknown instead of the statement error,
/// and the writer is retired.
#[tokio::test]
async fn pool_backed_failed_call_whose_rollback_fails_reports_side_effects_unknown() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    let pool = Arc::new(ConnectionPool::new(PoolConfig::for_test()).unwrap());
    {
        let guard = pool.try_writer().unwrap();
        guard
            .authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }))
            .unwrap();
    }
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let mut writer = khive_storage::SqlAccess::writer(&bridge).await.unwrap();

    let script = writer
        .execute_script("BEGIN; INSERT INTO no_such_table VALUES (1);".to_string())
        .await;
    assert!(
        matches!(
            &script,
            Err(khive_storage::StorageError::WriterTaskTerminated {
                request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
            })
        ),
        "a failed rollback must not be masked by the statement error: {script:?}"
    );
    assert!(
        writer
            .execute(statement("CREATE TABLE after_retirement (x INTEGER)"))
            .await
            .is_err(),
        "the retired writer must refuse later writes"
    );
}

/// (a) A guard released inside a transaction is rolled back by its drop, and
/// the drop is counted instead of discarded.
#[test]
fn writer_guard_drop_inside_a_transaction_is_counted() {
    let pool = ConnectionPool::new(PoolConfig::for_test()).unwrap();
    let before = pool
        .writer_acquisition_snapshot()
        .writer_guard_drop_rollbacks;
    {
        let guard = pool.try_writer().unwrap();
        guard
            .execute_batch(concat!(
                "CREATE TABLE dropped_rows (x INTEGER); ",
                "BEGIN; INSERT INTO dropped_rows VALUES (1);"
            ))
            .unwrap();
        assert!(
            !guard.is_autocommit(),
            "fixture: the guard must drop inside a transaction"
        );
    }
    assert_eq!(
        pool.writer_acquisition_snapshot()
            .writer_guard_drop_rollbacks,
        before + 1
    );
    let guard = pool.try_writer().unwrap();
    assert!(
        guard.is_autocommit(),
        "the drop must leave the writer in autocommit"
    );
    let rows: i64 = guard
        .query_row("SELECT COUNT(*) FROM dropped_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0, "the drop must roll the open transaction back");
}

/// (b) A standalone writer that holds no unit lease refuses transaction
/// control with a typed error instead of opening a transaction its per-call
/// lease would not cover.
#[tokio::test]
async fn standalone_execute_without_a_unit_lease_refuses_transaction_control() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("standalone_refusal.db")),
            write_queue_enabled: Some(false),
            volume_lock_dir: Some(dir.path().join("volume-locks")),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    let writer_slot = pool
        .sql_bridge_writer_slots()
        .acquire_owned()
        .await
        .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(writer_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };
    for sql in ["BEGIN IMMEDIATE", "SAVEPOINT s", "COMMIT"] {
        let refused = khive_storage::SqlWriter::execute(&mut writer, statement(sql)).await;
        assert!(
            matches!(&refused, Err(khive_storage::StorageError::InvalidInput { message, .. })
                if message.contains("transaction control")),
            "{sql}: {refused:?}"
        );
    }
    assert!(writer.handle.as_ref().unwrap().conn.is_autocommit());
    khive_storage::SqlWriter::execute(&mut writer, statement("CREATE TABLE ok_rows (x INTEGER)"))
        .await
        .expect("an ordinary statement still runs after the refusals");
}

async fn standalone_writer(pool: &Arc<ConnectionPool>) -> SqliteWriter {
    let writer_slot = pool
        .sql_bridge_writer_slots()
        .acquire_owned()
        .await
        .unwrap();
    let conn = open_standalone_writer(pool).unwrap();
    SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(writer_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(pool),
        pool: Arc::clone(pool),
        held_lease: None,
    }
}

fn standalone_pool(dir: &tempfile::TempDir, name: &str) -> Arc<ConnectionPool> {
    Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join(name)),
            write_queue_enabled: Some(false),
            volume_lock_dir: Some(dir.path().join("volume-locks")),
            disk_guard_config: Some(EffectiveDiskGuardConfig {
                guard_deadline_ms: 200,
                ..EffectiveDiskGuardConfig::default()
            }),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    )
}

/// (b) A standalone script that leaves a transaction open without a unit
/// lease is rolled back before the lease is released and refused, so the
/// transaction never continues outside the lease that admitted it.
#[tokio::test]
async fn standalone_script_left_in_a_transaction_is_rolled_back_before_the_lease_is_released() {
    let dir = tempfile::tempdir().unwrap();
    let pool = standalone_pool(&dir, "standalone_script.db");
    let mut writer = standalone_writer(&pool).await;
    khive_storage::SqlWriter::execute(
        &mut writer,
        statement("CREATE TABLE script_rows (x INTEGER)"),
    )
    .await
    .unwrap();

    let script = khive_storage::SqlWriter::execute_script(
        &mut writer,
        "BEGIN IMMEDIATE; INSERT INTO script_rows (x) VALUES (1);".to_string(),
    )
    .await;
    assert!(
        matches!(&script, Err(khive_storage::StorageError::InvalidInput { message, .. })
            if message.contains("left a transaction open")),
        "{script:?}"
    );
    let conn = &writer
        .handle
        .as_ref()
        .expect("a rolled-back handle is kept")
        .conn;
    assert!(
        conn.is_autocommit(),
        "the script's transaction must be rolled back"
    );
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM script_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        rows, 0,
        "the row written inside the open transaction was kept"
    );

    let other = Arc::clone(&pool);
    let second = tokio::task::spawn_blocking(move || {
        other
            .write_admission()
            .acquire()
            .map(|lease| lease.is_some())
    })
    .await
    .unwrap();
    assert!(
        second.is_ok(),
        "the lease must be released once the call has settled: {second:?}"
    );

    khive_storage::SqlWriter::execute_script(
        &mut writer,
        "BEGIN IMMEDIATE; INSERT INTO script_rows (x) VALUES (2); COMMIT;".to_string(),
    )
    .await
    .expect("a script that closes its own transaction still runs");
}

/// (b) When the open transaction cannot be rolled back on the connection,
/// the connection is closed while the lease is held, which settles it, and
/// the handle is dropped instead of being reused.
#[tokio::test]
async fn standalone_script_whose_rollback_is_refused_closes_the_connection_under_the_lease() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    let dir = tempfile::tempdir().unwrap();
    let pool = standalone_pool(&dir, "standalone_script_retire.db");
    let mut writer = standalone_writer(&pool).await;
    khive_storage::SqlWriter::execute(
        &mut writer,
        statement("CREATE TABLE retire_rows (x INTEGER)"),
    )
    .await
    .unwrap();
    writer
        .handle
        .as_ref()
        .unwrap()
        .conn
        .authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }))
        .unwrap();

    let script = khive_storage::SqlWriter::execute_script(
        &mut writer,
        "BEGIN IMMEDIATE; INSERT INTO retire_rows (x) VALUES (1);".to_string(),
    )
    .await;
    assert!(
        matches!(&script, Err(khive_storage::StorageError::InvalidInput { message, .. })
            if message.contains("left a transaction open")),
        "{script:?}"
    );
    assert!(
        writer.handle.is_none(),
        "a connection whose rollback was refused must be closed, not kept"
    );
    let reader = open_standalone_writer(&pool).unwrap();
    let rows: i64 = reader
        .query_row("SELECT COUNT(*) FROM retire_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        rows, 0,
        "closing the connection must discard the open transaction"
    );
}

#[test]
fn file_backed_pool_cannot_take_in_memory_writer_route_from_legacy_hint() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("hint-mismatch.db")),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    let bridge = SqlBridge::new(pool, false);
    assert!(bridge.is_file_backed);
}
