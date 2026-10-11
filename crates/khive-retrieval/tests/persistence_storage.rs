#![cfg(feature = "persist")]

use std::path::PathBuf;
use std::time::Duration;

use khive_db::StorageBackend;
use khive_retrieval::{Bm25Index, HnswIndex, NodeId, PersistError, RetrievalPersistence};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use khive_storage::{SqlAccess, StorageError};
use uuid::Uuid;

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("khive-persistence-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).expect("create private persistence fixture");
        Self(path)
    }

    fn queued_backend(&self) -> StorageBackend {
        let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
            self.0.join("snapshots.db"),
            true,
            Duration::from_secs(10),
            self.0.join("volume-locks"),
        )
        .expect("open file-backed backend");
        backend.prepare_core_schema().expect("migrate core tables");
        assert!(backend.pool().write_queue_active());
        assert!(backend
            .pool()
            .writer_task_handle()
            .expect("start writer task")
            .is_some());
        backend
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn snapshot_rows(sql: &dyn SqlAccess) -> Vec<SqlRow> {
    sql.reader()
        .await
        .expect("read fixture state")
        .query_all(SqlStatement::new(
            "SELECT namespace, index_type, snapshot, created_at FROM retrieval_snapshots \
             ORDER BY namespace, index_type",
            vec![],
        ))
        .await
        .expect("read snapshot rows")
}

fn snapshot_bytes(row: &SqlRow) -> &[u8] {
    match row.get("snapshot") {
        Some(SqlValue::Blob(bytes)) => bytes,
        value => panic!("expected snapshot BLOB, got {value:?}"),
    }
}

#[tokio::test]
async fn queued_snapshots_preserve_bytes_and_clear_only_the_selected_namespace() {
    const FIRST: &str = "snapshots:甲'one";
    const SECOND: &str = "snapshots:two";
    let scratch = ScratchDir::new();
    let backend = scratch.queued_backend();
    let first = RetrievalPersistence::new(backend.sql(), FIRST);
    let second = RetrievalPersistence::new(backend.sql(), SECOND);
    let mut bm25 = Bm25Index::default();
    bm25.index_document("first", "first snapshot").unwrap();
    let mut other = Bm25Index::default();
    other.index_document("other", "other namespace").unwrap();
    let other_bytes = serde_json::to_vec(&other).unwrap();
    let mut hnsw = HnswIndex::new(2);
    let node = NodeId::new([1; 16]);
    hnsw.insert(node, vec![1.0, 0.0]).unwrap();
    let before = backend.pool().writer_acquisition_snapshot();

    first.persist_bm25_index(&bm25).await.unwrap();
    first.persist_hnsw_snapshot(&hnsw).await.unwrap();
    second.persist_bm25_index(&other).await.unwrap();
    bm25.index_document("second", "replacement snapshot")
        .unwrap();
    let expected_bytes = serde_json::to_vec(&bm25).unwrap();
    first.persist_bm25_index(&bm25).await.unwrap();

    let rows = snapshot_rows(&backend).await;
    assert_eq!(rows.len(), 3, "an overwrite must retain one row per index");
    let bm25_row = rows
        .iter()
        .find(|row| {
            row.text("namespace").unwrap() == FIRST && row.text("index_type").unwrap() == "bm25"
        })
        .unwrap();
    assert_eq!(snapshot_bytes(bm25_row), expected_bytes);
    let other_row = rows
        .iter()
        .find(|row| row.text("namespace").unwrap() == SECOND)
        .unwrap();
    assert_eq!(snapshot_bytes(other_row), other_bytes);
    let preserved_other = serde_json::to_value(other_row).unwrap();
    let hnsw_row = rows
        .iter()
        .find(|row| row.text("index_type").unwrap() == "hnsw")
        .unwrap();
    let stats = first.stats().await.unwrap();
    assert_eq!(stats.bm25_snapshot_size, expected_bytes.len());
    assert_eq!(
        stats.bm25_snapshot_at,
        Some(bm25_row.i64("created_at").unwrap())
    );
    assert_eq!(stats.hnsw_snapshot_size, snapshot_bytes(hnsw_row).len());
    assert_eq!(
        stats.hnsw_snapshot_at,
        Some(hnsw_row.i64("created_at").unwrap())
    );
    assert!(stats.bm25_snapshot_at.unwrap() > 1_000_000_000_000_000);
    assert_eq!(
        first.load_bm25_index().await.unwrap().unwrap().doc_count(),
        2
    );
    assert_eq!(
        first
            .load_hnsw_snapshot()
            .await
            .unwrap()
            .unwrap()
            .indexed_ids,
        vec![node]
    );
    assert!(second.load_hnsw_snapshot().await.unwrap().is_none());

    first.clear().await.unwrap();
    let remaining = snapshot_rows(&backend).await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        serde_json::to_value(&remaining[0]).unwrap(),
        preserved_other
    );
    assert!(first.load_bm25_index().await.unwrap().is_none());
    assert!(first.load_hnsw_snapshot().await.unwrap().is_none());
    let cleared = first.stats().await.unwrap();
    assert_eq!(cleared.bm25_snapshot_size, 0);
    assert_eq!(cleared.hnsw_snapshot_size, 0);
    assert_eq!(cleared.bm25_snapshot_at, None);
    assert_eq!(cleared.hnsw_snapshot_at, None);
    assert_eq!(
        second.load_bm25_index().await.unwrap().unwrap().doc_count(),
        1
    );

    let after = backend.pool().writer_acquisition_snapshot();
    assert_eq!(
        after.writer_task_acquisitions - before.writer_task_acquisitions,
        5
    );
    assert_eq!(after.pooled_acquisitions, before.pooled_acquisitions);
    assert_eq!(
        after.standalone_acquisitions,
        before.standalone_acquisitions
    );
}

#[tokio::test]
async fn read_only_snapshots_load_and_report_stats_but_refuse_snapshot_clear_and_schema_writes() {
    const NAMESPACE: &str = "snapshots:read-only";
    let scratch = ScratchDir::new();
    let writable = scratch.queued_backend();
    let seed = RetrievalPersistence::new(writable.sql(), NAMESPACE);
    let mut bm25 = Bm25Index::default();
    bm25.index_document("retained", "retained snapshot")
        .unwrap();
    let mut hnsw = HnswIndex::new(2);
    let node = NodeId::new([2; 16]);
    hnsw.insert(node, vec![0.0, 1.0]).unwrap();
    seed.persist_bm25_index(&bm25).await.unwrap();
    seed.persist_hnsw_snapshot(&hnsw).await.unwrap();
    let before = serde_json::to_value(snapshot_rows(&writable).await).unwrap();
    let expected_stats = seed.stats().await.unwrap();
    let read_only = StorageBackend::sqlite_read_only_for_test(scratch.0.join("snapshots.db"))
        .expect("open read-only backend");
    read_only
        .prepare_core_schema()
        .expect("validate migrated schema");
    let reader = RetrievalPersistence::new(read_only.sql(), NAMESPACE);
    let before_writers = read_only.pool().writer_acquisition_snapshot();

    assert_eq!(
        reader.load_bm25_index().await.unwrap().unwrap().doc_count(),
        1
    );
    assert_eq!(
        reader
            .load_hnsw_snapshot()
            .await
            .unwrap()
            .unwrap()
            .indexed_ids,
        vec![node]
    );
    let stats = reader.stats().await.unwrap();
    assert_eq!(stats.bm25_snapshot_size, expected_stats.bm25_snapshot_size);
    assert_eq!(stats.hnsw_snapshot_size, expected_stats.hnsw_snapshot_size);
    assert_eq!(stats.bm25_snapshot_at, expected_stats.bm25_snapshot_at);
    assert_eq!(stats.hnsw_snapshot_at, expected_stats.hnsw_snapshot_at);

    bm25.index_document("refused", "must not be stored")
        .unwrap();
    hnsw.insert(NodeId::new([3; 16]), vec![1.0, 0.0]).unwrap();
    for result in [
        reader.persist_bm25_index(&bm25).await,
        reader.persist_hnsw_snapshot(&hnsw).await,
        reader.clear().await,
        reader.init_schema().await,
    ] {
        match result.expect_err("read-only write must fail") {
            PersistError::Storage(StorageError::Pool { operation, message }) => {
                assert_eq!(operation, "writer");
                assert_eq!(message, "backend is read-only");
            }
            error => panic!("expected typed read-only storage refusal, got {error:?}"),
        }
    }
    assert_eq!(
        read_only.pool().writer_acquisition_snapshot(),
        before_writers
    );
    assert_eq!(
        serde_json::to_value(snapshot_rows(&read_only).await).unwrap(),
        before
    );
    assert_eq!(
        serde_json::to_value(snapshot_rows(&writable).await).unwrap(),
        before
    );
}

#[tokio::test]
async fn compatibility_initializer_recreates_the_migrated_snapshot_schema() {
    const SCHEMA: &str = "SELECT type, name, sql FROM sqlite_schema \
                         WHERE tbl_name = 'retrieval_snapshots' ORDER BY type, name";
    let backend = StorageBackend::memory().expect("isolated schema fixture");
    backend.prepare_core_schema().expect("migrate core tables");
    let migrated = backend
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(SCHEMA, vec![]))
        .await
        .unwrap();
    assert_eq!(
        migrated
            .iter()
            .filter(|row| row.text("type").unwrap() == "table")
            .count(),
        1
    );
    assert!(migrated
        .iter()
        .any(|row| row.text("name").unwrap() == "idx_retrieval_snapshots_namespace"));
    // Only this private fixture loses its table; production initialization still uses V57.
    backend
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new("DROP TABLE retrieval_snapshots", vec![]))
        .await
        .unwrap();
    RetrievalPersistence::new(backend.sql(), "schema")
        .init_schema()
        .await
        .expect("public compatibility initializer");
    let recreated = backend
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(SCHEMA, vec![]))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(recreated).unwrap(),
        serde_json::to_value(migrated).unwrap()
    );
}
