use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use std::sync::atomic::AtomicU64;

#[test]
fn fts_backfill_checks_capacity_after_begin_and_rolls_back_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("backfill.db")),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let table = "admission_fts";
    let map = text::rowid_map_table(table);
    let state = text::rowid_map_state_table(table);
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(
            "CREATE VIRTUAL TABLE admission_fts USING fts5(content, namespace UNINDEXED, subject_id UNINDEXED, updated_at UNINDEXED);
             INSERT INTO admission_fts(content, namespace, subject_id, updated_at) VALUES ('fixture', 'local', 'one', 1);"
        ).unwrap();
        writer
            .conn()
            .execute_batch(&text::rowid_map_ddl(table))
            .unwrap();
    }
    let available = Arc::new(AtomicU64::new(1_000));
    let probe_available = Arc::clone(&available);
    pool.set_test_write_admission(100, move |_| Ok(probe_available.load(Ordering::SeqCst)));
    let writer = pool.autocommit_write_unit().unwrap();
    let began_available = Arc::clone(&available);
    writer
        .conn()
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin
                }
            ) {
                // The initial autocommit admission succeeded; force the actual
                // backfill transaction to observe a newer, insufficient sample.
                began_available.store(0, Ordering::SeqCst);
            }
            Authorization::Allow
        }))
        .unwrap();
    let result = ensure_fts_rowid_map_backfilled(writer.conn(), table, &pool.write_admission());
    writer
        .conn()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert!(
        matches!(result, Err(SqliteError::CapacityFloor { .. })),
        "backfill must refuse its post-BEGIN sample"
    );
    assert!(
        writer.conn().is_autocommit(),
        "capacity refusal must settle the transaction"
    );
    let count: i64 = writer
        .conn()
        .query_row(&format!("SELECT count(*) FROM {map}"), [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "backfill must not populate the rowid map on refusal"
    );
    let complete: i64 = writer
        .conn()
        .query_row(
            &format!("SELECT count(*) FROM {state} WHERE key='backfill'"),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(complete, 0, "refusal must not stamp a completed backfill");
    available.store(1_000, Ordering::SeqCst);
    ensure_fts_rowid_map_backfilled(writer.conn(), table, &pool.write_admission()).unwrap();
    let count: i64 = writer
        .conn()
        .query_row(&format!("SELECT count(*) FROM {map}"), [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "a fresh explicit attempt can proceed after recovery"
    );
}

#[test]
fn schema_failure_retires_the_pooled_connection_before_release() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("schema-retirement.db")).unwrap();
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }))
            .unwrap();
    }
    let plan = crate::migrations::ServiceSchemaPlan {
        service: "retire-pooled-schema",
        sqlite: &[crate::migrations::Migration {
            id: "bad-sql",
            up_sql: "CREATE TABLE pooled_schema_uncommitted (id INTEGER); SELECT * FROM missing_pooled_schema_table;",
            down_sql: None,
            is_already_applied: None,
        }],
        postgres: &[],
    };
    assert!(matches!(
        backend.apply_schema(&plan),
        Err(SqliteError::WriterSettlementUnknown)
    ));
    assert!(
        backend.pool().try_writer().is_err(),
        "the backend must mark the pooled writer retired"
    );
    assert!(backend
        .pool()
        .probe_retired_pooled_writer_for_test()
        .is_err());
    let conn = rusqlite::Connection::open(dir.path().join("schema-retirement.db")).unwrap();
    conn.busy_timeout(std::time::Duration::ZERO).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let persisted: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='pooled_schema_uncommitted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(persisted, 0);
    conn.execute_batch("ROLLBACK").unwrap();
}
