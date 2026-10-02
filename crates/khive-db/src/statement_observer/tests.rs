use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use khive_storage::{Note, SqlAccess, SqlStatement, StorageCapability, StorageError};

use super::*;
use crate::pool::StandaloneReaderPurpose;
use crate::{ConnectionPool, PoolConfig, SqlBridge, StorageBackend};

fn fixture(wal: bool, file: bool, queue: bool) -> (tempfile::TempDir, Arc<ConnectionPool>) {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: file.then(|| dir.path().join("statement-starts.db")),
            wal_mode: wal,
            max_readers: 1,
            write_queue_enabled: Some(queue),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE statement_fixture(value INTEGER);
         INSERT INTO statement_fixture VALUES(1),(2),(3);",
        )
        .unwrap();
    (dir, pool)
}

fn matching(observation: &StatementStartObservation, sql: &str) -> Vec<StartedStatement> {
    observation
        .started_statements()
        .unwrap()
        .into_iter()
        .filter(|record| record.sql == sql)
        .collect()
}

#[tokio::test]
async fn raw_reads_count_starts_not_rows_or_preparation() {
    for (wal, file) in [(true, true), (false, true), (false, false)] {
        let (_dir, pool) = fixture(wal, file, false);
        let observation = pool.observe_test_statement_starts(256).unwrap();
        {
            let writer = pool.writer().unwrap();
            let _never_stepped = writer
                .conn()
                .prepare("SELECT 999 AS never_stepped")
                .unwrap();
        }
        assert!(matching(&observation, "SELECT 999 AS never_stepped").is_empty());
        let bridge = SqlBridge::new(Arc::clone(&pool), file);
        let mut reader = bridge.reader().await.unwrap();
        let sql = "SELECT value FROM statement_fixture ORDER BY value";
        let rows = reader
            .query_all(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: Some("actual raw read".into()),
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        let records = matching(&observation, sql);
        assert_eq!(records.len(), 1);
        assert!(records[0].readonly);
        reader
            .query_all(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(matching(&observation, sql).len(), 2);
        let comment_sql = "-- ordinary user comment\nSELECT 47";
        reader
            .query_row(SqlStatement {
                sql: comment_sql.into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(matching(&observation, comment_sql).len(), 1);
    }
}

#[tokio::test]
async fn typed_scalar_and_batch_reads_share_the_same_hook() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("typed-notes.db")).unwrap();
    let store = backend.notes().unwrap();
    let notes: Vec<_> = (0..3)
        .map(|i| Note::new("fixture", "observation", i.to_string()))
        .collect();
    for note in &notes {
        store.upsert_note(note.clone()).await.unwrap();
    }
    let observation = backend.pool().observe_test_statement_starts(256).unwrap();
    for note in &notes {
        assert_eq!(store.get_note(note.id).await.unwrap().unwrap().id, note.id);
    }
    let ids: Vec<_> = notes.iter().map(|note| note.id).collect();
    assert_eq!(store.get_notes_batch(&ids).await.unwrap().len(), 3);
    let visibility = store.get_note_visibility_batch(&ids).await.unwrap();
    assert_eq!(visibility.len(), 3);
    let records = observation.started_statements().unwrap();
    let point = records
        .iter()
        .filter(|r| r.sql.contains("FROM notes WHERE id = ?1"))
        .count();
    let batch = records
        .iter()
        .filter(|r| r.sql.contains("FROM notes WHERE id IN ("))
        .count();
    assert_eq!(point, 3);
    assert_eq!(batch, 2);
    assert!(records
        .iter()
        .filter(|r| r.sql.contains("FROM notes WHERE id "))
        .all(|r| r.readonly));
    assert!(
        records.iter().all(|r| !r.sql.contains(&ids[0].to_string())),
        "statement text must not expand bound identifiers"
    );
    drop(observation);
    drop(store);
    let join = backend.pool().take_writer_task_join();
    drop(backend);
    if let Some(join) = join {
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn queued_writer_and_trigger_update_count_top_level_attempts() {
    for wal in [true, false] {
        let (_dir, pool) = fixture(wal, true, true);
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE observer_audit(value INTEGER);
             CREATE TRIGGER observe_update AFTER UPDATE ON statement_fixture
             BEGIN INSERT INTO observer_audit VALUES(new.value); END;",
            )
            .unwrap();
        // Open the actual queued writer after observation starts, so its
        // connection creation must adopt the already active pool hub.
        let observation = pool.observe_test_statement_starts(512).unwrap();
        let handle = pool
            .writer_task_handle()
            .unwrap()
            .expect("queued writer required");
        let sql = "UPDATE statement_fixture SET value = value + 10";
        let affected = handle
            .send_bounded(move |conn| {
                conn.execute(sql, []).map_err(|error| {
                    StorageError::driver(
                        StorageCapability::Sql,
                        "observer actual queued update",
                        error,
                    )
                })
            })
            .await
            .unwrap();
        assert_eq!(affected, 3);
        assert_eq!(matching(&observation, sql).len(), 1);
        assert!(!matching(&observation, sql)[0].readonly);
        assert!(observation
            .started_statements()
            .unwrap()
            .iter()
            .all(|r| !r.sql.starts_with("-- TRIGGER") && !r.sql.starts_with("-- INSERT")));
        assert_eq!(
            pool.reader()
                .unwrap()
                .query_row("SELECT count(*) FROM observer_audit", [], |row| row
                    .get::<_, i64>(0),)
                .unwrap(),
            3
        );
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TRIGGER fail_update BEFORE UPDATE ON statement_fixture
             BEGIN SELECT RAISE(ABORT, 'observer real refusal'); END;",
            )
            .unwrap();
        let failing_sql = "UPDATE statement_fixture SET value = value + 100";
        let error = handle
            .send_bounded(move |conn| {
                conn.execute(failing_sql, []).map_err(|error| {
                    StorageError::driver(
                        StorageCapability::Sql,
                        "observer actual failing update",
                        error,
                    )
                })
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("observer real refusal"));
        assert_eq!(
            matching(&observation, failing_sql).len(),
            1,
            "a failed step is still one top-level execution attempt"
        );
        assert_eq!(
            pool.reader()
                .unwrap()
                .query_row("SELECT sum(value) FROM statement_fixture", [], |row| row
                    .get::<_, i64>(0),)
                .unwrap(),
            36
        );
        drop(observation);
        drop(handle);
        let join = pool.take_writer_task_join().unwrap();
        drop(pool);
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .unwrap()
            .unwrap();
    }
}

#[test]
fn replacement_and_standalone_connections_share_pool_observation() {
    let (_dir, pool) = fixture(true, true, false);
    let observation = pool.observe_test_statement_starts(256).unwrap();
    {
        let reader = pool.reader().unwrap();
        reader.discard();
    }
    let reader = pool.reader().unwrap();
    assert_eq!(
        reader
            .query_row("SELECT 123 AS replacement_reader", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        123
    );
    assert_eq!(
        matching(&observation, "SELECT 123 AS replacement_reader").len(),
        1
    );
    drop(reader);
    let standalone = pool
        .open_standalone_reader(StandaloneReaderPurpose::DiagnosticsIndependentSnapshot)
        .unwrap();
    assert_eq!(
        standalone
            .query_row("SELECT 456 AS standalone_reader", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        456
    );
    assert_eq!(
        matching(&observation, "SELECT 456 AS standalone_reader").len(),
        1
    );
    let writer = pool.open_standalone_writer_untracked().unwrap();
    let sql = "UPDATE statement_fixture SET value = value + 1";
    assert_eq!(writer.execute(sql, []).unwrap(), 3);
    assert_eq!(matching(&observation, sql).len(), 1);
}

#[test]
fn observation_off_is_zero_and_unwind_releases_the_guard() {
    let (_dir, pool) = fixture(false, false, false);
    let observation = pool.observe_test_statement_starts(128).unwrap();
    let retained = Arc::clone(&observation.probe);
    drop(observation);
    pool.reader()
        .unwrap()
        .query_row("SELECT 1 AS guard_off", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert!(retained.records.lock().statements.is_empty());
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _guard = pool.observe_test_statement_starts(128).unwrap();
        panic!("owned observation unwind");
    }));
    assert!(result.is_err());
    let next = pool.observe_test_statement_starts(128).unwrap();
    pool.reader()
        .unwrap()
        .query_row("SELECT 2 AS guard_on", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert_eq!(matching(&next, "SELECT 2 AS guard_on").len(), 1);
}

#[test]
fn record_budget_and_nested_observations_fail_explicitly() {
    let (_dir, pool) = fixture(false, false, false);
    assert!(pool.observe_test_statement_starts(0).is_err());
    let observation = pool.observe_test_statement_starts(1).unwrap();
    assert!(pool.observe_test_statement_starts(1).is_err());
    let writer = pool.writer().unwrap();
    writer
        .conn()
        .query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
        .unwrap();
    writer
        .conn()
        .query_row("SELECT 2", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert!(observation
        .started_statements()
        .unwrap_err()
        .to_string()
        .contains("incomplete"));
}

#[test]
fn connection_can_outlive_pool_and_observation_without_a_dangling_context() {
    let (_dir, pool) = fixture(true, true, false);
    let observation = pool.observe_test_statement_starts(128).unwrap();
    let writer = pool.open_standalone_writer_untracked().unwrap();
    drop(pool);
    let sql = "UPDATE statement_fixture SET value = value + 20";
    assert_eq!(writer.execute(sql, []).unwrap(), 3);
    assert_eq!(matching(&observation, sql).len(), 1);
    drop(observation);
    // Its hub is gone, but the connection's monotonic opaque token is safe.
    assert_eq!(
        writer
            .query_row("SELECT 89", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        89
    );
}
