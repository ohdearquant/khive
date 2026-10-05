use std::sync::Arc;

use khive_storage::types::VectorRecord;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;
use uuid::Uuid;

use super::*;
use crate::pool::{ConnectionPool, PoolConfig};

fn create_vec_table(pool: &Arc<ConnectionPool>, model_key: &str, dims: usize) {
    let ddl = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
        model_key, dims
    );
    let writer = pool.try_writer().expect("pool writer");
    writer.conn().execute_batch(&ddl).expect("create vec table");
    writer
        .conn()
        .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
        .expect("create vector_provenance");
    writer
        .conn()
        .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
        .expect("create ann_write_log");
}

async fn assert_later_chunk_failure_rolls_back_all(write_queue_enabled: bool) {
    crate::extension::ensure_extensions_loaded();

    let model_key = if write_queue_enabled {
        "delete_subjects_atomic_queue"
    } else {
        "delete_subjects_atomic_legacy"
    };
    let dims = 4usize;
    let namespace = "ns:delete_subjects_atomic";
    let dir = tempfile::tempdir().expect("temporary database directory");
    let path = dir.path().join(format!("{model_key}.db"));
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(write_queue_enabled),
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_vec_table(&pool, model_key, dims);

    let store = Arc::new(
        SqliteVecStore::new(
            Arc::clone(&pool),
            true,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            namespace.to_string(),
        )
        .expect("SqliteVecStore::new"),
    );
    assert_eq!(
        pool.writer_task_spawn_count(),
        if write_queue_enabled { 1 } else { 0 },
        "test setup must exercise the requested writer mode"
    );
    assert_eq!(
        store.writer_task.is_some(),
        write_queue_enabled,
        "the store must retain a WriterTask handle exactly when requested"
    );

    let ids: Vec<Uuid> = (0..401).map(|_| Uuid::new_v4()).collect();
    let records = ids
        .iter()
        .map(|subject_id| VectorRecord {
            subject_id: *subject_id,
            kind: SubstrateKind::Entity,
            namespace: namespace.to_string(),
            field: "body".to_string(),
            embedding_model: None,
            vectors: vec![vec![0.1, 0.2, 0.3, 0.4]],
            text_fingerprint: None,
            updated_at: chrono::Utc::now(),
        })
        .collect();
    let summary = store.insert_batch(records).await.expect("seed vectors");
    assert_eq!(summary.attempted, 401);
    assert_eq!(summary.affected, 401);
    assert_eq!(summary.failed, 0);

    {
        let writer = pool.try_writer().expect("pool writer");
        writer
            .conn()
            .execute("DELETE FROM ann_write_log", [])
            .expect("clear seed log rows");
        let trigger = format!(
            "CREATE TRIGGER fail_second_delete_subjects_chunk \
                 BEFORE INSERT ON ann_write_log \
                 WHEN NEW.op = 'delete' AND NEW.subject_id = '{}' \
                 BEGIN \
                     SELECT RAISE(ABORT, 'injected later-chunk delete failure'); \
                 END;",
            ids[400]
        );
        writer
            .conn()
            .execute_batch(&trigger)
            .expect("install later-chunk failure trigger");
    }

    let delete_result = if write_queue_enabled {
        // A spawn counter or non-None handle only proves setup. Hold the
        // WriterTask's single drain slot, start delete_subjects, and
        // observe its request waiting in the channel to prove this exact
        // operation uses the queued path.
        let writer_task = store
            .writer_task
            .clone()
            .expect("queue-enabled store must retain its writer task");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let occupier = {
            let writer_task = writer_task.clone();
            tokio::spawn(async move {
                writer_task
                    .send(move |_conn| {
                        let _ = started_tx.send(());
                        let _ = release_rx.blocking_recv();
                        Ok::<(), StorageError>(())
                    })
                    .await
            })
        };

        started_rx
            .await
            .expect("occupier must start inside the writer task");
        assert_eq!(
            writer_task.queue_depth(),
            0,
            "the channel must be empty after dequeuing the occupier"
        );

        let delete_task = {
            let store = Arc::clone(&store);
            let ids = ids.clone();
            tokio::spawn(async move { store.delete_subjects(&ids).await })
        };

        let mut saw_enqueued = false;
        for _ in 0..100 {
            if writer_task.queue_depth() >= 1 {
                saw_enqueued = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Always unblock and join both tasks before asserting so a failed
        // routing assertion cannot strand the blocking occupier.
        release_tx
            .send(())
            .expect("occupier must still be waiting for release");
        occupier
            .await
            .expect("occupier task must not panic")
            .expect("occupier write must succeed");
        let result = delete_task.await.expect("delete task must not panic");
        assert!(
            saw_enqueued,
            "delete_subjects never appeared in the WriterTask channel while its drain slot \
                 was occupied"
        );
        result
    } else {
        store.delete_subjects(&ids).await
    };

    let err = delete_result.expect_err("the trigger must fail the second delete chunk");
    assert!(
        err.to_string()
            .contains("injected later-chunk delete failure"),
        "expected injected failure, got: {err}"
    );

    let present = store
        .batch_exists(&ids, namespace)
        .await
        .expect("read vectors after rollback");
    assert_eq!(
        present.len(),
        ids.len(),
        "a later-chunk failure must restore every earlier deleted vector"
    );
    assert!(
        ids.iter().all(|id| present.contains(id)),
        "all requested vectors must remain after rollback"
    );

    let delete_log_count: i64 = pool
        .try_writer()
        .expect("pool writer")
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ann_write_log WHERE op = 'delete'",
            [],
            |row| row.get(0),
        )
        .expect("count delete log rows");
    assert_eq!(
        delete_log_count, 0,
        "rolled-back vector deletes must not leave committed ANN log rows"
    );
}

#[tokio::test]
async fn delete_subjects_later_chunk_failure_rolls_back_all_legacy_writer() {
    assert_later_chunk_failure_rolls_back_all(false).await;
}

#[tokio::test]
async fn delete_subjects_later_chunk_failure_rolls_back_all_writer_task() {
    assert_later_chunk_failure_rolls_back_all(true).await;
}

#[tokio::test]
async fn delete_subjects_unmanaged_commit_failure_rolls_back_and_cleans_connection() {
    crate::extension::ensure_extensions_loaded();

    let model_key = "delete_subjects_commit_failure";
    let dims = 4usize;
    let namespace = "ns:delete_subjects_commit_failure";
    let dir = tempfile::tempdir().expect("temporary database directory");
    let path = dir.path().join("delete_subjects_commit_failure.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .expect("file-backed pool"),
    );
    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        true,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        namespace.to_string(),
    )
    .expect("SqliteVecStore::new");
    assert!(
        store.writer_task.is_none(),
        "commit-failure sentinel must exercise the unmanaged transaction"
    );

    let subject_id = Uuid::new_v4();
    let summary = store
        .insert_batch(vec![VectorRecord {
            subject_id,
            kind: SubstrateKind::Entity,
            namespace: namespace.to_string(),
            field: "body".to_string(),
            embedding_model: None,
            vectors: vec![vec![0.1, 0.2, 0.3, 0.4]],
            text_fingerprint: None,
            updated_at: chrono::Utc::now(),
        }])
        .await
        .expect("seed vector");
    assert_eq!(summary.affected, 1);

    {
        let writer = pool.try_writer().expect("pool writer");
        let conn = writer.conn();
        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("read foreign_keys pragma");
        assert_eq!(foreign_keys, 1, "commit sentinel requires foreign keys");
        conn.execute_batch(
            "DELETE FROM ann_write_log;
                 CREATE TABLE delete_commit_parent (id TEXT PRIMARY KEY);
                 CREATE TABLE delete_commit_child (
                     parent_id TEXT NOT NULL REFERENCES delete_commit_parent(id)
                         DEFERRABLE INITIALLY DEFERRED
                 );
                 CREATE TRIGGER fail_delete_subjects_commit
                 AFTER INSERT ON ann_write_log
                 WHEN NEW.op = 'delete'
                 BEGIN
                     INSERT INTO delete_commit_child(parent_id) VALUES ('missing-parent');
                 END;",
        )
        .expect("install deferred commit-failure sentinel");
    }

    let err = store
        .delete_subjects(&[subject_id])
        .await
        .expect_err("deferred foreign-key violation must fail COMMIT");
    assert!(
        err.to_string().contains("FOREIGN KEY constraint failed"),
        "expected deferred commit failure, got: {err}"
    );

    let present = store
        .batch_exists(&[subject_id], namespace)
        .await
        .expect("read vector after failed commit");
    assert!(
        present.contains(&subject_id),
        "failed COMMIT must roll back the vector deletion"
    );

    let writer = pool.try_writer().expect("pool writer after failed commit");
    let conn = writer.conn();
    assert!(
        conn.is_autocommit(),
        "RAII cleanup must not return an open transaction to the pool"
    );
    let delete_log_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ann_write_log WHERE op = 'delete'",
            [],
            |row| row.get(0),
        )
        .expect("count delete logs after failed commit");
    assert_eq!(
        delete_log_count, 0,
        "failed COMMIT must roll back ANN delete logs"
    );
    let child_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM delete_commit_child", [], |row| {
            row.get(0)
        })
        .expect("count deferred-FK rows after failed commit");
    assert_eq!(
        child_count, 0,
        "failed COMMIT must roll back trigger side effects"
    );
}
