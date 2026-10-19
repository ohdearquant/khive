use super::*;
use khive_storage::types::{EdgeFilter, SqlStatement, SqlValue};
use khive_storage::{EntityFilter, EventFilter};

include!("backend_read_admission_tests.rs");

#[cfg(unix)]
#[tokio::test]
async fn sqlite_detects_chmod_read_only_snapshot_and_core_reads_succeed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chmod_snapshot.db");
    {
        let writable = StorageBackend::sqlite_for_test(&path).expect("create writable database");
        writable
            .prepare_core_schema()
            .expect("migrate writable snapshot source");
    }

    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&path, permissions).unwrap();
    freeze_snapshot_sidecars(&path);

    let read_only = StorageBackend::sqlite(&path).expect("auto-detect read-only mode");
    assert!(read_only.is_read_only());
    assert_eq!(
        read_only.pool().config().write_queue_enabled,
        Some(false),
        "read-only boot must not attempt to spawn a writer task"
    );
    assert!(read_only
        .pool()
        .writer_task_handle()
        .expect("disabled writer task is a valid configuration")
        .is_none());
    read_only
        .prepare_core_schema()
        .expect("current snapshot validates without migration writes");

    let entities = read_only.entities().expect("entity store opens read-only");
    let graph = read_only.graph().expect("graph store opens read-only");
    let notes = read_only.notes().expect("note store opens read-only");
    let events = read_only.events().expect("event store opens read-only");
    assert_eq!(
        entities
            .count_entities("local", khive_storage::EntityFilter::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        graph
            .count_edges(khive_storage::types::EdgeFilter::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(notes.count_notes("local", None).await.unwrap(), 0);
    assert_eq!(
        events
            .count_events(khive_storage::EventFilter::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        read_only.notes_seq_repair_run_count(),
        0,
        "read-only store acquisition must not run the DML repair"
    );
}

#[test]
fn memory_backend_creates_successfully() {
    let backend = StorageBackend::memory().expect("memory backend should create");
    assert!(!backend.is_file_backed());
}

#[test]
fn file_backend_creates_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let backend = StorageBackend::sqlite(&path).expect("file backend should create");
    assert!(backend.is_file_backed());
    assert!(path.exists());
}

#[test]
fn data_dir_returns_none_for_memory_backend() {
    let backend = StorageBackend::memory().expect("memory backend");
    assert!(backend.data_dir().is_none());
}

#[test]
fn data_dir_returns_parent_dir_for_file_backend() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let backend = StorageBackend::sqlite_for_test(&path).expect("file backend");
    let got = backend.data_dir().expect("file backend must return Some");
    assert_eq!(got, dir.path());
}

include!("backend/ann_root_tests.rs");

#[tokio::test]
async fn sql_access_memory_roundtrip() {
    let backend = StorageBackend::memory().unwrap();
    let sql = backend.sql();

    let mut writer = sql.writer().await.unwrap();
    writer
        .execute_script("CREATE TABLE test_rt (id TEXT PRIMARY KEY, value INTEGER NOT NULL)".into())
        .await
        .unwrap();

    let affected = writer
        .execute(SqlStatement {
            sql: "INSERT INTO test_rt (id, value) VALUES (?1, ?2)".into(),
            params: vec![SqlValue::Text("row1".into()), SqlValue::Integer(42)],
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let mut reader = sql.reader().await.unwrap();
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT id, value FROM test_rt WHERE id = ?1".into(),
            params: vec![SqlValue::Text("row1".into())],
            label: None,
        })
        .await
        .unwrap();

    let row = row.expect("should find the inserted row");
    assert_eq!(row.columns.len(), 2);
    match &row.columns[0].value {
        SqlValue::Text(s) => assert_eq!(s, "row1"),
        other => panic!("expected Text, got {other:?}"),
    }
    match &row.columns[1].value {
        SqlValue::Integer(v) => assert_eq!(*v, 42),
        other => panic!("expected Integer, got {other:?}"),
    }
}

#[tokio::test]
async fn sql_access_file_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_roundtrip.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();
    let sql = backend.sql();

    let mut writer = sql.writer().await.unwrap();
    writer
        .execute_script("CREATE TABLE test_f (k TEXT PRIMARY KEY, v TEXT)".into())
        .await
        .unwrap();
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO test_f (k, v) VALUES (?1, ?2)".into(),
            params: vec![
                SqlValue::Text("hello".into()),
                SqlValue::Text("world".into()),
            ],
            label: None,
        })
        .await
        .unwrap();

    let mut reader = sql.reader().await.unwrap();
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT k, v FROM test_f".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    match &rows[0].columns[1].value {
        SqlValue::Text(s) => assert_eq!(s, "world"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn sqlite_read_only_missing_path_does_not_create_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing_ro.db");
    assert!(!path.exists());

    let result = StorageBackend::sqlite_read_only(&path);
    assert!(
        result.is_err(),
        "opening a missing path read-only must fail"
    );
    assert!(
        !path.exists(),
        "opening a missing path read-only must not create the file"
    );
}

#[test]
fn sqlite_read_only_sparse_store_requires_existing_table_without_writer_acquisition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_sparse_tables.db");
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        writable
            .prepare_core_schema()
            .expect("migrate snapshot source");
        writable
            .sparse("present")
            .expect("create the optional sparse table while writable");
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let read_only = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    read_only
        .prepare_core_schema()
        .expect("validate exact current migration ledger");
    read_only
        .sparse("present")
        .expect("an existing sparse table must open read-only");
    let missing = match read_only.sparse("missing") {
        Ok(_) => panic!("a missing sparse table must fail during store acquisition"),
        Err(error) => error,
    };
    assert!(
        missing.to_string().contains("sparse_missing"),
        "the diagnostic must name the absent table: {missing}"
    );
    assert_eq!(
        read_only.pool().writer_acquisition_snapshot(),
        crate::pool::WriterAcquisitionSnapshot::default(),
        "construction, exact-ledger validation, and optional sparse-table inspection must \
             use reader connections only"
    );
}

#[test]
fn sqlite_read_only_text_store_requires_existing_table_without_writer_acquisition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_text_tables.db");
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        writable
            .prepare_core_schema()
            .expect("migrate snapshot source");
        writable
            .text("present")
            .expect("create the optional FTS table while writable");
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let read_only = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    read_only
        .prepare_core_schema()
        .expect("validate exact current migration ledger");
    read_only
        .text("present")
        .expect("an existing FTS table must open read-only");
    let missing = match read_only.text("missing") {
        Ok(_) => panic!("a missing FTS table must fail during store acquisition"),
        Err(error) => error,
    };
    assert!(
        missing.to_string().contains("fts_missing"),
        "the diagnostic must name the absent table: {missing}"
    );
    assert_eq!(
        read_only.pool().writer_acquisition_snapshot(),
        crate::pool::WriterAcquisitionSnapshot::default(),
        "construction, exact-ledger validation, and optional FTS inspection must use reader \
             connections only"
    );
}

#[cfg(feature = "vectors")]
#[test]
fn sqlite_read_only_vector_store_schema_check_uses_no_writer_acquisition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_vector_tables.db");
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        writable
            .prepare_core_schema()
            .expect("migrate snapshot source");
        writable
            .vectors("present", "present", 3)
            .expect("create the optional vector table while writable");
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let read_only = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    read_only
        .prepare_core_schema()
        .expect("validate exact current migration ledger");
    read_only
        .vectors("present", "present", 3)
        .expect("an existing vector table must open read-only");
    assert!(
        read_only.vectors("missing", "missing", 3).is_err(),
        "a missing vector table must fail during store acquisition"
    );
    assert_eq!(
        read_only.pool().writer_acquisition_snapshot(),
        crate::pool::WriterAcquisitionSnapshot::default(),
        "construction, exact-ledger validation, and optional vector inspection must use \
             reader connections only"
    );
}

#[tokio::test]
async fn sqlite_read_only_sql_writer_rejects_ddl_and_insert() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_writer.db");

    // Create the database and a table while writable.
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        let sql = writable.sql();
        let mut writer = sql.writer().await.unwrap();
        writer
            .execute_script("CREATE TABLE ro_existing (id INTEGER PRIMARY KEY)".into())
            .await
            .unwrap();
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let ro = StorageBackend::sqlite_read_only(&path).unwrap();
    let sql = ro.sql();

    // Writer acquisition itself must fail for a read-only backend.
    let writer_result = sql.writer().await;
    assert!(
        writer_result.is_err(),
        "sql().writer() must be rejected on a read-only backend"
    );
}

#[tokio::test]
#[cfg(feature = "vectors")]
async fn vectors_roundtrip_via_public_api() {
    let backend = StorageBackend::memory().unwrap();
    let store = backend.vectors("test_api", "test_api", 3).unwrap();

    let id = uuid::Uuid::new_v4();
    store
        .insert(
            id,
            khive_types::SubstrateKind::Entity,
            "local",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();

    let hits = store
        .search(khive_storage::types::VectorSearchRequest {
            query_vectors: vec![vec![1.0, 0.0, 0.0]],
            top_k: 1,
            namespace: None,
            kind: None,
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, id);
    assert!(hits[0].score.to_f64() > 0.99);
}

#[tokio::test]
#[cfg(feature = "vectors")]
async fn vectors_direct_store_leaves_provenance_to_migration() {
    let backend = StorageBackend::memory().unwrap();
    {
        let reader = backend.pool.reader().unwrap();
        assert!(!sqlite_table_exists(reader.conn(), "vector_provenance").unwrap());
        assert_eq!(
            crate::migrations::read_schema_version(reader.conn()).unwrap(),
            0
        );
    }
    let store = backend
        .vectors("direct_provenance", "direct_provenance", 3)
        .unwrap();
    let reader = backend.pool.reader().unwrap();
    assert!(!sqlite_table_exists(reader.conn(), "vector_provenance").unwrap());
    assert!(!sqlite_table_exists(reader.conn(), "_schema_migrations").unwrap());
    drop(reader);

    let id = uuid::Uuid::new_v4();
    store
        .insert(
            id,
            khive_types::SubstrateKind::Entity,
            "local",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();
    let provenance = store.provenance(id).await.unwrap().unwrap();
    assert_eq!(provenance.embedding_model, "direct_provenance");
    assert_eq!(provenance.text_fingerprint, None);
    assert_eq!(provenance.updated_at, None);
    assert!(store.delete(id).await.unwrap());
    assert!(store.provenance(id).await.unwrap().is_none());
}

#[tokio::test]
#[cfg(feature = "vectors")]
async fn vectors_after_lazy_notes_and_events_schema_stay_unmigrated() {
    let backend = StorageBackend::memory().unwrap();
    backend.notes().unwrap();
    backend.events().unwrap();
    {
        let reader = backend.pool.reader().unwrap();
        let note_key_columns: u32 = reader
            .conn()
            .query_row(
                "SELECT count(*) FROM pragma_table_xinfo('notes') WHERE name = 'key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(note_key_columns, 1);
        assert_eq!(
            crate::migrations::read_schema_version(reader.conn()).unwrap(),
            0
        );
    }

    let store = backend.vectors("after_notes", "after_notes", 3).unwrap();
    let id = uuid::Uuid::new_v4();
    store
        .insert(
            id,
            khive_types::SubstrateKind::Entity,
            "local",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();
    let provenance = store.provenance(id).await.unwrap().unwrap();
    assert_eq!(provenance.embedding_model, "after_notes");
    assert_eq!(provenance.text_fingerprint, None);
    assert_eq!(provenance.updated_at, None);

    let reader = backend.pool.reader().unwrap();
    assert!(!sqlite_table_exists(reader.conn(), "vector_provenance").unwrap());
    assert!(!sqlite_table_exists(reader.conn(), "_schema_migrations").unwrap());
}

#[tokio::test]
#[cfg(feature = "vectors")]
async fn vectors_creates_table_idempotently() {
    let backend = StorageBackend::memory().unwrap();

    let store1 = backend.vectors("idempotent", "idempotent", 3).unwrap();
    let store2 = backend.vectors("idempotent", "idempotent", 3).unwrap();

    let id = uuid::Uuid::new_v4();
    store1
        .insert(
            id,
            khive_types::SubstrateKind::Entity,
            "local",
            "content",
            vec![vec![1.0, 0.0, 0.0]],
        )
        .await
        .unwrap();

    let count = store2.count().await.unwrap();
    assert_eq!(count, 1);
}

/// DDL for a vec0 table that predates the `field` and `embedding_model`
/// columns, which `ensure_vector_tables` must reject.
#[cfg(feature = "vectors")]
fn legacy_vector_table_ddl(model_key: &str) -> String {
    format!(
        "CREATE VIRTUAL TABLE vec_{model_key} USING vec0(\
             subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, \
             embedding float[3] distance_metric=cosine)"
    )
}

#[cfg(feature = "vectors")]
#[test]
fn repeated_vector_store_fetches_take_the_writer_once_per_model() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("vector_once.db")).unwrap();

    let before = backend.pool.writer_acquisition_snapshot();
    for namespace in ["local", "tenant_a", "tenant_b", "local"] {
        backend
            .vectors_for_namespace("fetched_once", "fetched-once", 3, namespace)
            .expect("vector store");
    }
    let after = backend.pool.writer_acquisition_snapshot();
    assert_eq!(
        after.pooled_acquisitions - before.pooled_acquisitions,
        1,
        "only the first fetch of a model may check out the writer"
    );
    assert_eq!(
        after.writer_task_acquisitions,
        before.writer_task_acquisitions
    );
    assert_eq!(
        after.standalone_acquisitions,
        before.standalone_acquisitions
    );

    // The first fetch still created the vector table and its sidecars.
    {
        let reader = backend.pool.reader().unwrap();
        assert!(sqlite_table_exists(reader.conn(), "vec_fetched_once").unwrap());
        assert!(sqlite_table_exists(reader.conn(), "_embedding_models").unwrap());
        assert!(sqlite_table_exists(reader.conn(), "ann_write_log").unwrap());
    }

    // A model the backend has not prepared yet is still cold.
    let before = backend.pool.writer_acquisition_snapshot();
    backend
        .vectors("fetched_second", "fetched-second", 3)
        .expect("second model");
    backend
        .vectors("fetched_second", "fetched-second", 3)
        .expect("second model again");
    let after = backend.pool.writer_acquisition_snapshot();
    assert_eq!(
        after.pooled_acquisitions - before.pooled_acquisitions,
        1,
        "a second model is prepared once, independently of the first"
    );
    let reader = backend.pool.reader().unwrap();
    assert!(sqlite_table_exists(reader.conn(), "vec_fetched_second").unwrap());
}

#[cfg(feature = "vectors")]
#[test]
fn ensure_vector_tables_prepares_only_models_that_are_not_ready() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("vector_batch.db")).unwrap();
    let checkouts = || {
        backend
            .pool
            .writer_acquisition_snapshot()
            .pooled_acquisitions
    };

    let start = checkouts();
    backend
        .ensure_vector_tables(&[("batch_a", 3), ("batch_b", 3)])
        .expect("cold batch");
    assert_eq!(checkouts() - start, 1, "a cold batch shares one checkout");

    backend
        .ensure_vector_tables(&[("batch_b", 3), ("batch_a", 3)])
        .expect("ready batch");
    assert_eq!(checkouts() - start, 1, "a ready batch takes no writer");

    backend
        .ensure_vector_tables(&[("batch_a", 3), ("batch_c", 3)])
        .expect("partly cold batch");
    assert_eq!(
        checkouts() - start,
        2,
        "a batch with one cold model takes the writer once"
    );
    let reader = backend.pool.reader().unwrap();
    assert!(sqlite_table_exists(reader.conn(), "vec_batch_c").unwrap());
}

#[cfg(feature = "vectors")]
#[test]
fn warm_vector_store_fetch_finishes_while_the_pool_writer_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let backend =
        Arc::new(StorageBackend::sqlite_for_test(dir.path().join("vector_warm.db")).unwrap());
    backend
        .vectors("held_writer", "held-writer", 3)
        .expect("cold fetch");

    let writer = backend.pool.try_writer().unwrap();
    let before = backend.pool.writer_acquisition_snapshot();
    let worker_backend = Arc::clone(&backend);
    let (finished, result) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let fetched = worker_backend
            .vectors_for_namespace("held_writer", "held-writer", 3, "tenant_a")
            .map(|_| ());
        finished.send(fetched).unwrap();
    });
    let while_held = result.recv_timeout(std::time::Duration::from_secs(2));
    // Release before joining even when the fetch incorrectly waits.
    drop(writer);
    worker.join().unwrap();
    while_held
        .expect("a warm vector store fetch must return while the pool writer is held")
        .expect("warm vector store fetch");
    assert_eq!(
        backend
            .pool
            .writer_acquisition_snapshot()
            .pooled_acquisitions,
        before.pooled_acquisitions
    );
}

#[cfg(feature = "vectors")]
#[test]
fn failed_vector_table_check_is_retried_and_not_recorded_as_ready() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("vector_retry.db")).unwrap();
    backend
        .pool
        .try_writer()
        .unwrap()
        .conn()
        .execute_batch(&legacy_vector_table_ddl("retried"))
        .unwrap();

    let before = backend.pool.writer_acquisition_snapshot();
    for _ in 0..2 {
        let error = match backend.vectors("retried", "retried", 3) {
            Ok(_) => panic!("a vec0 table without the required columns must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, SqliteError::InvalidData(message)
                    if message.contains("vec_retried")
                        && message.contains("missing required column")),
            "unexpected error: {error}"
        );
    }
    let after = backend.pool.writer_acquisition_snapshot();
    assert_eq!(
        after.pooled_acquisitions - before.pooled_acquisitions,
        2,
        "a failed check must be repeated by the next fetch"
    );

    // Once the table is replaced the next fetch passes and is recorded.
    backend
        .pool
        .try_writer()
        .unwrap()
        .conn()
        .execute_batch("DROP TABLE vec_retried")
        .unwrap();
    backend
        .vectors("retried", "retried", 3)
        .expect("fetch after the legacy table is gone");
    let before = backend.pool.writer_acquisition_snapshot();
    backend
        .vectors("retried", "retried", 3)
        .expect("warm fetch after the check passed");
    assert_eq!(
        backend
            .pool
            .writer_acquisition_snapshot()
            .pooled_acquisitions,
        before.pooled_acquisitions
    );
}

#[cfg(feature = "vectors")]
#[test]
fn vector_table_created_after_open_is_validated_before_use() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vector_late.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();
    backend
        .vectors("known_model", "known-model", 3)
        .expect("prepare one model");

    // Another connection adds a legacy-schema table after the backend opened.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(&legacy_vector_table_ddl("late_model"))
        .unwrap();

    let error = match backend.vectors("late_model", "late-model", 3) {
        Ok(_) => panic!("a late legacy vec0 table must be validated before use"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, SqliteError::InvalidData(message)
                if message.contains("vec_late_model")
                    && message.contains("missing required column")),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn text_roundtrip_via_public_api() {
    let backend = StorageBackend::memory().unwrap();
    let store = backend.text("test_api").unwrap();

    let id = uuid::Uuid::new_v4();
    let doc = khive_storage::types::TextDocument {
        subject_id: id,
        kind: khive_types::SubstrateKind::Entity,
        record_kind: None,
        title: Some("Test Title".to_string()),
        body: "This is a searchable document about Rust.".to_string(),
        tags: vec!["rust".to_string()],
        namespace: "test_ns".to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    };
    store.upsert_document(doc).await.unwrap();

    let hits = store
        .search(khive_storage::types::TextSearchRequest {
            query: "Rust".to_string(),
            mode: khive_storage::types::TextQueryMode::Plain,
            filter: Some(khive_storage::types::TextFilter {
                namespaces: vec!["test_ns".to_string()],
                ..Default::default()
            }),
            top_k: 1,
            snippet_chars: 64,
        })
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, id);
    assert!(hits[0].score.to_f64() > 0.0);
}

#[tokio::test]
async fn text_creates_table_idempotently() {
    let backend = StorageBackend::memory().unwrap();

    let store1 = backend.text("idempotent_fts").unwrap();
    let store2 = backend.text("idempotent_fts").unwrap();

    let id = uuid::Uuid::new_v4();
    let doc = khive_storage::types::TextDocument {
        subject_id: id,
        kind: khive_types::SubstrateKind::Note,
        record_kind: None,
        title: None,
        body: "Hello world.".to_string(),
        tags: vec![],
        namespace: "test_ns".to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    };
    store1.upsert_document(doc).await.unwrap();

    let count = store2
        .count(khive_storage::types::TextFilter {
            namespaces: vec!["test_ns".to_string()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
}

fn warm_text_document(namespace: &str) -> khive_storage::types::TextDocument {
    khive_storage::types::TextDocument {
        subject_id: uuid::Uuid::new_v4(),
        kind: khive_types::SubstrateKind::Note,
        record_kind: None,
        title: None,
        body: "warm fetch".to_string(),
        tags: vec![],
        namespace: namespace.to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    }
}

/// A search opens its text store on every call. Once the table's rowid-map
/// marker is durable that fetch must not take the writer, or every search
/// queues on the volume lease behind unrelated writes.
#[tokio::test]
async fn warm_text_fetch_does_not_take_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("text_warm.db")).unwrap();
    let acquisitions = || {
        backend
            .pool
            .writer_acquisition_snapshot()
            .pooled_acquisitions
    };

    let before = acquisitions();
    backend.text("warm").unwrap();
    let store = backend.text("warm").unwrap();
    assert_eq!(
        acquisitions() - before,
        2,
        "a table with no rows has no marker, so every fetch re-checks it"
    );

    store
        .upsert_document(warm_text_document("ns"))
        .await
        .unwrap();
    let before = acquisitions();
    backend.text("warm").unwrap();
    assert_eq!(
        acquisitions() - before,
        1,
        "the first fetch after a row exists writes the marker"
    );

    let before = acquisitions();
    backend.text("warm").unwrap();
    backend.text("warm").unwrap();
    assert_eq!(
        acquisitions(),
        before,
        "a fetch after the marker is durable must not take the writer"
    );
}

/// The load symptom itself: while another thread holds the writer, a warm
/// fetch still opens the store instead of waiting out the lease deadline.
#[tokio::test]
async fn warm_text_fetch_succeeds_while_the_writer_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("text_held.db")).unwrap();
    backend
        .text("held")
        .unwrap()
        .upsert_document(warm_text_document("ns"))
        .await
        .unwrap();
    backend.text("held").unwrap();

    let held = backend.pool.try_writer().unwrap();
    let fetched = std::thread::scope(|scope| {
        scope
            .spawn(|| backend.text("held").map(drop))
            .join()
            .unwrap()
    });
    drop(held);
    fetched.expect("a warm fetch must not wait for the writer");
}

/// A table is recorded only after its backfill commits. When the backfill
/// fails, the fetch reports the error and the next fetch takes the writer and
/// runs the backfill again instead of trusting an unproven rowid map.
#[tokio::test]
async fn failed_backfill_leaves_the_text_table_unproven() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("text_unproven.db")).unwrap();
    let acquisitions = || {
        backend
            .pool
            .writer_acquisition_snapshot()
            .pooled_acquisitions
    };
    backend
        .text("unproven")
        .unwrap()
        .upsert_document(warm_text_document("ns"))
        .await
        .unwrap();

    backend
        .pool
        .writer()
        .unwrap()
        .conn()
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Insert {
                    table_name: "fts_unproven_rowids_state"
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    let refused = backend.text("unproven").map(|_| ());
    backend
        .pool
        .writer()
        .unwrap()
        .conn()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    refused.expect_err("a refused marker write fails the fetch");

    let before = acquisitions();
    backend.text("unproven").unwrap();
    assert_eq!(
        acquisitions() - before,
        1,
        "the fetch after a failed backfill takes the writer and runs it again"
    );
    let before = acquisitions();
    backend.text("unproven").unwrap();
    assert_eq!(
        acquisitions(),
        before,
        "once the backfill commits the table is proven"
    );
}

/// khive-runtime never caches the `Arc<dyn TextSearch>` `text()`/
/// `text_for_notes()` return — every one of its ~26 call sites in
/// `operations.rs`/`curation.rs` calls `StorageBackend::text()` fresh. A
/// backend proves each table once (`text_tables_ready`), so
/// `ensure_fts_rowid_map_backfilled`'s already-backfilled short-circuit runs
/// on the first open of a table by each backend, and every later open runs no
/// SQL. A first draft of that function used `SELECT COUNT(*)` for the
/// short-circuit — correct, but `COUNT(*)` over an FTS5 table costs real
/// per-call time proportional to row count, which would reintroduce a scan on
/// the path this migration exists to remove. Compare SQLite VM work for a
/// fresh backend's first open at 100 and 5,000 rows: the completion-marker
/// probe should need similar work at both sizes; a full-table count should
/// not. The 500 opens after it must do no writer work at all. Counting
/// executed work keeps scheduler delays out of this growth test.
#[tokio::test]
async fn text_repeated_open_after_backfill_does_not_scale_with_row_count() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Work of the first open on `backend`, then of the 500 opens after it.
    fn open_work(backend: &StorageBackend) -> (u64, u64) {
        let work = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&work);
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .progress_handler(
                    1,
                    Some(move || {
                        counted.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                )
                .unwrap();
        }

        // All opens use this private backend's pooled writer. Release
        // its guard before re-entry, and remove the hook even when an
        // open returns an error, before asserting or growing the fixture.
        let first = backend.text("hot_path_reopen").map(|_| ());
        let cold = work.load(Ordering::Relaxed);
        let result = first
            .and_then(|()| (0..500).try_for_each(|_| backend.text("hot_path_reopen").map(|_| ())));
        backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .progress_handler(0, None::<fn() -> bool>)
            .unwrap();
        result.expect("repeated text-store opens must succeed");
        (cold, work.load(Ordering::Relaxed) - cold)
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hot_path_reopen.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();
    let store = backend.text("hot_path_reopen").unwrap();
    let body = "the quick brown fox jumps over the lazy dog ".repeat(35);
    let mut seeded = 0;
    let mut work = Vec::new();
    for target_rows in [100, 5_000] {
        for _ in seeded..target_rows {
            let doc = khive_storage::types::TextDocument {
                subject_id: uuid::Uuid::new_v4(),
                kind: khive_types::SubstrateKind::Note,
                record_kind: Some("memory".to_string()),
                title: None,
                body: body.clone(),
                tags: vec![],
                namespace: "test_ns".to_string(),
                metadata: None,
                updated_at: chrono::Utc::now(),
            };
            store.upsert_document(doc).await.unwrap();
        }
        seeded = target_rows;
        assert_eq!(
            store
                .count(khive_storage::types::TextFilter {
                    namespaces: vec!["test_ns".to_string()],
                    ..Default::default()
                })
                .await
                .unwrap(),
            target_rows,
            "the work comparison requires both declared row populations"
        );

        // The first populated open records completion outside the work
        // measurement; later upserts maintain the rowid map atomically.
        let _ = backend.text("hot_path_reopen").unwrap();
        let (cold, warm) = open_work(&StorageBackend::sqlite_for_test(&path).unwrap());
        assert_eq!(
            warm, 0,
            "opens after the backend proved the table must not use the writer"
        );
        work.push(cold);
    }

    let [small, large] = [work[0], work[1]];
    assert!(small > 0 && large > 0, "both work meters must be active");
    assert!(
        large <= small * 2,
        "a fresh backend's first backend.text() call used {small} SQLite VM progress \
             units at 100 rows and {large} at 5,000 rows; growing the table 50-fold must \
             not more than double already-backfilled work (for example via COUNT(*))"
    );
}

/// Legacy FTS tables can hold rows with no corresponding rowid-map
/// entries. Seed that state without the maintained write path, then
/// assert that opening the text store restores bidirectional rowid
/// parity without losing records. Repeated-open work growth is covered
/// separately by `text_repeated_open_after_backfill_does_not_scale_with_row_count`.
#[tokio::test]
async fn text_open_after_legacy_seed_backfills_the_map_with_full_parity() {
    let backend = StorageBackend::memory().unwrap();
    let table_key = "legacy_seed_parity";
    let table = format!("fts_{table_key}");
    let map = format!("{table}_rowids");

    // Establishes the schema (empty FTS table + empty map) exactly like
    // any other first call.
    let _ = backend.text(table_key).unwrap();

    // Seed rows with raw SQL directly against the FTS table, bypassing
    // `upsert_document`/the map entirely -- this is the legacy-empty-map
    // state a database predating this migration would be in.
    {
        let writer = backend.pool().writer().unwrap();
        writer.conn().execute_batch("BEGIN").unwrap();
        {
            let mut insert = writer
                .conn()
                .prepare(&format!(
                        "INSERT INTO {table} \
                         (subject_id, kind, title, body, tags, namespace, metadata, updated_at, \
                          record_kind) \
                         VALUES (?1, 'note', '', 'legacy body', '[]', 'test_ns', NULL, 0, 'memory')"
                    ))
                .unwrap();
            for i in 0..500 {
                insert
                    .execute(rusqlite::params![format!("legacy-{i}")])
                    .unwrap();
            }
        }
        writer.conn().execute_batch("COMMIT").unwrap();
    }
    {
        let writer = backend.pool().writer().unwrap();
        let map_count: i64 = writer
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            map_count, 0,
            "the raw-SQL seed must bypass the map, reproducing a genuinely pre-migration db"
        );
    }

    // This call must run the REAL backfill body (not just the any-row
    // short-circuit), since the map is still empty.
    let _ = backend.text(table_key).unwrap();

    {
        let writer = backend.pool().writer().unwrap();
        let mismatched: i64 = writer
                .conn()
                .query_row(
                    &format!(
                        "SELECT \
                         (SELECT COUNT(*) FROM {table} WHERE rowid NOT IN (SELECT rowid FROM {map})) + \
                         (SELECT COUNT(*) FROM {map} WHERE rowid NOT IN (SELECT rowid FROM {table}))"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
        assert_eq!(
            mismatched, 0,
            "backfill must give every FTS row exactly one map entry, both directions"
        );
        let fts_count: i64 = writer
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        let map_count: i64 = writer
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
            .unwrap();
        assert_eq!(fts_count, 500);
        assert_eq!(map_count, 500);
    }
}

/// A map holding a row for B but none for A (the exact state a crash
/// window predating the durable completion marker could leave behind)
/// must be reconciled on the next writable open, not treated as already
/// complete just because it has at least one row. Seeds the FTS table
/// with raw SQL for both A and B, seeds the map with ONLY B's row, then
/// resets the completion marker to reproduce a database that predates
/// the marker's own existence, and asserts the next `backend.text()`
/// call backfills A too.
#[tokio::test]
async fn text_open_reconciles_a_partial_map_instead_of_treating_it_as_complete() {
    let backend = StorageBackend::memory().unwrap();
    let table_key = "partial_map_reconcile";
    let table = format!("fts_{table_key}");
    let map = format!("{table}_rowids");
    let state = format!("{map}_state");

    // Establishes the schema (and, since both tables are still empty,
    // writes a marker for the empty case).
    let _ = backend.text(table_key).unwrap();

    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    {
        let writer = backend.pool().writer().unwrap();
        writer.conn().execute_batch("BEGIN").unwrap();
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (1, ?1, 'note', '', 'doc a', '[]', 'test_ns', NULL, 0, 'memory')"
                ),
                rusqlite::params![a.to_string()],
            )
            .expect("insert A's fts row");
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (2, ?1, 'note', '', 'doc b', '[]', 'test_ns', NULL, 0, 'memory')"
                ),
                rusqlite::params![b.to_string()],
            )
            .expect("insert B's fts row");
        // Only B gets a map entry -- this is the partial-map state.
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {map} (namespace, subject_id, rowid) VALUES ('test_ns', ?1, 2)"
                ),
                rusqlite::params![b.to_string()],
            )
            .expect("insert B's own map entry, leaving A's missing");
        // Undo the marker the schema-establishing call above wrote for
        // the then-empty table: a database whose map already predates
        // the marker mechanism entirely never has this row either.
        writer
            .conn()
            .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
            .expect("clear the completion marker");
        writer.conn().execute_batch("COMMIT").unwrap();
    }

    let store = backend.text(table_key).unwrap();

    let a_mapped: i64 = {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {map} WHERE namespace = 'test_ns' AND subject_id = ?1"
                ),
                rusqlite::params![a.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        a_mapped, 1,
        "the partial map must be reconciled, not left missing A's entry"
    );

    let fetched_a = store.get_document("test_ns", a).await.unwrap();
    assert!(
        fetched_a.is_some(),
        "get_document(A) must work once the partial map is reconciled"
    );
}

/// A map row surviving at the right rowid but the WRONG key (the crash
/// window `delete_document_dml`'s trailing key re-check guards against at
/// the single-delete level, but which this reconciliation pass must also
/// clean up if it survived into a legacy/pre-marker database) must be
/// removed, not merely supplemented by a second, correct map row for the
/// same rowid. Seeds the FTS table with ONE live row at rowid 7 keyed
/// `(test_ns, B)`, seeds a stale map row `(test_ns, A, 7)` -- as if A's
/// document once lived at rowid 7 and was replaced by B without the map
/// being repaired -- clears the marker, and asserts the next open leaves
/// the map with exactly `(test_ns, B, 7)`: A's stale entry gone, B's
/// entry present, and `get_document` for each key answering accordingly.
#[tokio::test]
async fn text_open_removes_a_wrong_key_map_row_before_backfilling_the_right_one() {
    let backend = StorageBackend::memory().unwrap();
    let table_key = "wrong_key_map_row";
    let table = format!("fts_{table_key}");
    let map = format!("{table}_rowids");
    let state = format!("{map}_state");

    let _ = backend.text(table_key).unwrap();

    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    {
        let writer = backend.pool().writer().unwrap();
        writer.conn().execute_batch("BEGIN").unwrap();
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (7, ?1, 'note', '', 'doc b', '[]', 'test_ns', NULL, 0, 'memory')"
                ),
                rusqlite::params![b.to_string()],
            )
            .expect("insert B's live fts row at rowid 7");
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {map} (namespace, subject_id, rowid) VALUES ('test_ns', ?1, 7)"
                ),
                rusqlite::params![a.to_string()],
            )
            .expect("insert A's stale map row still pointing at rowid 7");
        writer
            .conn()
            .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
            .expect("clear the completion marker");
        writer.conn().execute_batch("COMMIT").unwrap();
    }

    let store = backend.text(table_key).unwrap();

    let map_rows: Vec<(String, i64)> = {
        let writer = backend.pool().writer().unwrap();
        let mut stmt = writer
            .conn()
            .prepare(&format!(
                "SELECT subject_id, rowid FROM {map} ORDER BY rowid"
            ))
            .unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    };
    assert_eq!(
        map_rows,
        vec![(b.to_string(), 7)],
        "the stale (A, 7) map row must be removed and replaced by the correct (B, 7) row, \
             not left alongside it"
    );

    assert!(
        store.get_document("test_ns", a).await.unwrap().is_none(),
        "A's stale map entry is gone, so get_document(A) must find nothing"
    );
    let fetched_b = store.get_document("test_ns", b).await.unwrap();
    assert!(
        fetched_b.is_some(),
        "get_document(B) must find the live row now correctly mapped"
    );
    assert_eq!(fetched_b.unwrap().body, "doc b");
}

/// Two non-NULL-key FTS rows for the SAME `(namespace, subject_id)` at
/// different rowids (a pre-atomic-upsert-era duplicate) must resolve to
/// exactly one map row at the survivor rowid, with the OTHER, losing row
/// actually removed from the FTS table -- not merely left unmapped and
/// invisible to every keyed lookup. Mirrors migration 024's own sweep,
/// which this function's step 3 must reproduce at runtime.
#[tokio::test]
async fn text_open_sweeps_the_duplicate_that_lost_the_survivor_race() {
    let backend = StorageBackend::memory().unwrap();
    let table_key = "duplicate_loser_swept";
    let table = format!("fts_{table_key}");
    let map = format!("{table}_rowids");
    let state = format!("{map}_state");

    let _ = backend.text(table_key).unwrap();

    let dup = uuid::Uuid::new_v4();
    {
        let writer = backend.pool().writer().unwrap();
        writer.conn().execute_batch("BEGIN").unwrap();
        // Lower rowid, OLDER updated_at -- must lose the survivor race.
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (10, ?1, 'note', '', 'older body', '[]', 'test_ns', NULL, 1, \
                          'memory')"
                ),
                rusqlite::params![dup.to_string()],
            )
            .expect("insert the older/losing duplicate");
        // Higher rowid, NEWER updated_at -- must survive.
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (20, ?1, 'note', '', 'newer body', '[]', 'test_ns', NULL, 5, \
                          'memory')"
                ),
                rusqlite::params![dup.to_string()],
            )
            .expect("insert the newer/surviving duplicate");
        writer
            .conn()
            .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
            .expect("clear the completion marker");
        writer.conn().execute_batch("COMMIT").unwrap();
    }

    let _ = backend.text(table_key).unwrap();

    let writer = backend.pool().writer().unwrap();
    let map_rows: Vec<i64> = writer
        .conn()
        .prepare(&format!(
            "SELECT rowid FROM {map} WHERE namespace = 'test_ns' AND subject_id = ?1"
        ))
        .unwrap()
        .query_map(rusqlite::params![dup.to_string()], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        map_rows,
        vec![20],
        "exactly one map row must survive, at the newer (by updated_at) rowid"
    );

    let fts_rowids: Vec<i64> = writer
        .conn()
        .prepare(&format!("SELECT rowid FROM {table} ORDER BY rowid"))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        fts_rowids,
        vec![20],
        "the losing duplicate (rowid 10) must be deleted from the FTS table itself, not just \
             left out of the map as an unmapped live row"
    );
}

/// Once `ensure_fts_rowid_map_backfilled` has written the completion
/// marker, a later open must not re-scan the FTS table at all -- not
/// even to reconcile it. Corrupts the map after the real backfill by
/// deleting one of its rows directly, then asserts a second
/// `backend.text()` call leaves that row missing: had it re-scanned, the
/// full-table `INSERT OR REPLACE` would have restored it.
#[tokio::test]
async fn text_open_after_marker_written_does_not_rescan_even_a_corrupted_map() {
    let backend = StorageBackend::memory().unwrap();
    let table_key = "marker_no_rescan";
    let table = format!("fts_{table_key}");
    let map = format!("{table}_rowids");
    let state = format!("{map}_state");

    let store = backend.text(table_key).unwrap();
    store
        .upsert_document(khive_storage::types::TextDocument {
            subject_id: uuid::Uuid::new_v4(),
            kind: khive_types::SubstrateKind::Note,
            record_kind: Some("memory".to_string()),
            title: None,
            body: "seed".to_string(),
            tags: vec![],
            namespace: "test_ns".to_string(),
            metadata: None,
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    // `khive-runtime` never caches the `Arc<dyn TextSearch>` `text()`
    // returns (see this function's own doc comment) -- it calls
    // `StorageBackend::text()` fresh on essentially every access. This
    // second call is that fresh re-open: the table now holds the row
    // just written above, so `ensure_fts_rowid_map_backfilled` runs its
    // real body and writes the marker.
    let _ = backend.text(table_key).unwrap();

    let marked: bool = {
        let writer = backend.pool().writer().unwrap();
        writer
                .conn()
                .query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM {state} WHERE key = 'backfill' AND value = 'complete')"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap()
    };
    assert!(
        marked,
        "a completion marker must exist once the table has held a row"
    );

    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute(&format!("DELETE FROM {map}"), [])
            .expect("corrupt the map by deleting its row directly");
    }

    let _ = backend.text(table_key).unwrap();

    let map_count: i64 = {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(
        map_count, 0,
        "a marker-complete table must not be re-scanned on open, even to reconcile a map \
             an external actor emptied out from under it"
    );
}

/// The writable legacy backfill (a table opened for the first time with
/// FTS rows already present, predating the map entirely) must exclude
/// NULL-key rows rather than fail the map's NOT NULL constraint.
#[tokio::test]
async fn text_open_writable_legacy_backfill_excludes_null_key_rows() {
    let backend = StorageBackend::memory().unwrap();
    let table_key = "legacy_null_key";
    let table = format!("fts_{table_key}");
    let map = format!("{table}_rowids");
    let state = format!("{map}_state");

    let _ = backend.text(table_key).unwrap();
    {
        let writer = backend.pool().writer().unwrap();
        writer.conn().execute_batch("BEGIN").unwrap();
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (1, NULL, 'note', '', 'null-key body', '[]', NULL, NULL, 0, '')"
                ),
                [],
            )
            .expect("insert legacy NULL-key fts row");
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (2, 'legacy-1', 'note', '', 'normal body', '[]', 'test_ns', NULL, \
                          0, 'memory')"
                ),
                [],
            )
            .expect("insert legacy normal-key fts row");
        writer
            .conn()
            .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
            .expect("clear the completion marker written for the then-empty table");
        writer.conn().execute_batch("COMMIT").unwrap();
    }

    // Must open without erroring against the map's NOT NULL columns.
    let _ = backend.text(table_key).unwrap();

    let writer = backend.pool().writer().unwrap();
    let map_count: i64 = writer
        .conn()
        .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
        .unwrap();
    assert_eq!(map_count, 1, "only the non-NULL-key row may be mapped");
    let mapped_subject: String = writer
        .conn()
        .query_row(&format!("SELECT subject_id FROM {map}"), [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(mapped_subject, "legacy-1");
}

/// The writable legacy backfill must choose the survivor for a
/// duplicate `(namespace, subject_id)` key by `updated_at`, with rowid
/// only as a tie-break -- the same `ORDER BY updated_at ASC, rowid ASC`
/// contract migration 024 uses, not rowid alone.
#[tokio::test]
async fn text_open_writable_legacy_backfill_survivor_is_chosen_by_updated_at_not_rowid() {
    let backend = StorageBackend::memory().unwrap();
    let table_key = "legacy_updated_at_survivor";
    let table = format!("fts_{table_key}");
    let map = format!("{table}_rowids");
    let state = format!("{map}_state");

    let _ = backend.text(table_key).unwrap();
    {
        let writer = backend.pool().writer().unwrap();
        writer.conn().execute_batch("BEGIN").unwrap();
        // Lower rowid, but NEWER updated_at.
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (100, 'dup', 'note', '', 'newer body', '[]', 'test_ns', NULL, \
                          500, 'memory')"
                ),
                [],
            )
            .expect("insert newer-but-lower-rowid fts row");
        // Higher rowid, but OLDER updated_at.
        writer
            .conn()
            .execute(
                &format!(
                    "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (200, 'dup', 'note', '', 'older body', '[]', 'test_ns', NULL, \
                          100, 'memory')"
                ),
                [],
            )
            .expect("insert older-but-higher-rowid fts row");
        writer
            .conn()
            .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
            .expect("clear the completion marker written for the then-empty table");
        writer.conn().execute_batch("COMMIT").unwrap();
    }

    let _ = backend.text(table_key).unwrap();

    let writer = backend.pool().writer().unwrap();
    let mapped_rowid: i64 = writer
        .conn()
        .query_row(
            &format!("SELECT rowid FROM {map} WHERE namespace = 'test_ns' AND subject_id = 'dup'"),
            [],
            |row| row.get(0),
        )
        .expect("read dup's map entry");
    assert_eq!(
        mapped_rowid, 100,
        "the newer document (by updated_at) must survive even though its rowid is lower"
    );
}

include!("backend/key_validation_tests.rs");

#[tokio::test]
async fn sqlite_read_only_graph_store_rejects_upsert_edge() {
    use khive_storage::types::Edge;
    use khive_types::EdgeRelation;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_graph.db");

    // Create the database and the graph schema while writable.
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        writable.graph().unwrap();
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let ro = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let store = match ro.graph() {
        Ok(store) => store,
        // Failing to even open the store on a read-only backend is an
        // acceptable rejection — the write path never becomes reachable.
        Err(_) => return,
    };

    let now = chrono::Utc::now();
    let edge = Edge {
        id: uuid::Uuid::new_v4().into(),
        namespace: "local".to_string(),
        source_id: uuid::Uuid::new_v4(),
        target_id: uuid::Uuid::new_v4(),
        relation: EdgeRelation::Extends,
        weight: 0.8,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };

    let result = store.upsert_edge(edge).await;
    assert!(
        result.is_err(),
        "upsert_edge on a read-only backend must reject, not silently no-op"
    );
}

#[tokio::test]
async fn sqlite_read_only_event_store_rejects_append_event() {
    use khive_types::{EventKind, EventOutcome, SubstrateKind};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_events.db");

    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        writable.events().unwrap();
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let ro = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let store = match ro.events() {
        Ok(store) => store,
        Err(_) => return,
    };

    let event = khive_storage::event::Event::new(
        "local",
        "test.verb",
        EventKind::Audit,
        SubstrateKind::Entity,
        "test-actor",
    )
    .with_outcome(EventOutcome::Success);

    let result = store.append_event(event).await;
    assert!(
        result.is_err(),
        "append_event on a read-only backend must reject, not silently no-op"
    );
}

#[tokio::test]
async fn sqlite_read_only_text_store_rejects_upsert_document() {
    use khive_storage::types::TextDocument;
    use khive_types::SubstrateKind;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_text.db");

    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        writable.text("ro_test").unwrap();
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let ro = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let store = match ro.text("ro_test") {
        Ok(store) => store,
        Err(_) => return,
    };

    let doc = TextDocument {
        subject_id: uuid::Uuid::new_v4(),
        kind: SubstrateKind::Entity,
        record_kind: None,
        title: Some("Title".to_string()),
        body: "Body text.".to_string(),
        tags: vec![],
        namespace: "local".to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    };

    let result = store.upsert_document(doc).await;
    assert!(
        result.is_err(),
        "upsert_document on a read-only backend must reject, not silently no-op"
    );
}

/// A read-only snapshot whose FTS
/// table predates the rowid-map sidecar (created here with raw SQL,
/// bypassing `text()`'s own map creation, to reproduce a pre-migration
/// snapshot) must still open and serve `get_document`/`delete_document`
/// via the scan-fallback predicates, rather than erroring against a
/// sidecar table that was never created.
#[tokio::test]
async fn sqlite_read_only_text_store_without_rowid_map_falls_back_to_scan_predicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_text_no_map.db");

    let id = uuid::Uuid::new_v4();
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        let writer = writable.pool().try_writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS fts_ro_no_map USING fts5(\
                     subject_id UNINDEXED, kind UNINDEXED, title, body, tags UNINDEXED, \
                     namespace UNINDEXED, metadata UNINDEXED, updated_at UNINDEXED, \
                     record_kind, tokenize = 'trigram')",
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO fts_ro_no_map \
                     (subject_id, kind, title, body, tags, namespace, metadata, updated_at, \
                      record_kind) \
                     VALUES (?1, 'note', '', 'legacy body', '[]', 'local', NULL, 0, NULL)",
                rusqlite::params![id.to_string()],
            )
            .unwrap();
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let ro = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let store = ro
        .text("ro_no_map")
        .expect("a read-only FTS table with no sidecar map must still open successfully");

    let fetched = store
        .get_document("local", id)
        .await
        .expect("scan-fallback get_document must not error against a missing map table");
    assert!(
        fetched.is_some(),
        "scan-fallback get_document must still find the legacy row"
    );
    assert_eq!(fetched.unwrap().subject_id, id);
}

/// A read-only snapshot whose sidecar map table EXISTS but whose
/// completion marker was never written (the exact state a crash between
/// the map's creation and `ensure_fts_rowid_map_backfilled` finishing
/// could leave a copy in, since a read-only connection can never run
/// that reconciliation itself) must still fall back to the scan
/// predicates, not trust a map that might be partial. Seeds FTS rows for
/// A and B directly, seeds the map (and its state sidecar, via the same
/// DDL `text_with_tokenizer` uses) with a row for B only, and never
/// writes the `backfill = complete` marker.
#[tokio::test]
async fn sqlite_read_only_text_store_with_unmarked_map_falls_back_to_scan_predicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro_text_unmarked_map.db");

    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        let writer = writable.pool().try_writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS fts_ro_unmarked USING fts5(\
                     subject_id UNINDEXED, kind UNINDEXED, title, body, tags UNINDEXED, \
                     namespace UNINDEXED, metadata UNINDEXED, updated_at UNINDEXED, \
                     record_kind, tokenize = 'trigram')",
            )
            .unwrap();
        writer
            .conn()
            .execute_batch(&text::rowid_map_ddl("fts_ro_unmarked"))
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO fts_ro_unmarked \
                     (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                      updated_at, record_kind) \
                     VALUES (1, ?1, 'note', '', 'doc a', '[]', 'local', NULL, 0, NULL)",
                rusqlite::params![a.to_string()],
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO fts_ro_unmarked \
                     (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                      updated_at, record_kind) \
                     VALUES (2, ?1, 'note', '', 'doc b', '[]', 'local', NULL, 0, NULL)",
                rusqlite::params![b.to_string()],
            )
            .unwrap();
        // Only B gets a map entry, and the `_state` sidecar is left
        // without a `backfill` row -- no marker exists at all.
        writer
            .conn()
            .execute(
                "INSERT INTO fts_ro_unmarked_rowids (namespace, subject_id, rowid) \
                     VALUES ('local', ?1, 2)",
                rusqlite::params![b.to_string()],
            )
            .unwrap();
    }
    #[cfg(unix)]
    freeze_snapshot_sidecars(&path);

    let ro = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let store = ro
        .text("ro_unmarked")
        .expect("a read-only FTS table with an unmarked map must still open successfully");

    let fetched_a = store
        .get_document("local", a)
        .await
        .expect("scan-fallback get_document must not error against an unmarked map");
    assert!(
        fetched_a.is_some(),
        "A has no map entry, so only the scan fallback (not a map join) can find it -- \
             proving the unmarked map was not trusted"
    );
    assert_eq!(fetched_a.unwrap().body, "doc a");
}

#[tokio::test]
async fn blob_store_roundtrip_via_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob_backend.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();

    // Explicit floor_bytes=0, not the default 100GB — the free space on
    // whatever volume runs this test is not this test's concern (and a
    // dev machine or CI runner legitimately may not clear 100GB free).
    let store = backend.blob_store(None, Some(0)).unwrap();
    let bytes = b"backend-level blob roundtrip".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();
    assert_eq!(
        store
            .get_bounded_verified(&content_ref, bytes.len() as u64)
            .await
            .unwrap(),
        bytes
    );
}

#[test]
fn blob_store_defaults_root_beside_db_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob_default.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();

    // `blob_store` creates the root directory eagerly (`FsBlobStore::new`),
    // so its existence at the expected default path is directly
    // observable without reaching into the trait object.
    let _store = backend.blob_store(None, None).unwrap();
    assert!(
        dir.path().join("blobs").is_dir(),
        "default root must be created beside the database file"
    );
}

#[test]
fn blob_store_without_a_configured_floor_enforces_the_volume_default() {
    // The path the daemon takes: no `[storage.blob] floor_bytes`, root beside
    // the database. The store's floor is the ADR-111 Amendment 6 formula over
    // the capacity of the volume that holds its root.
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("floor.db")).unwrap();
    let store = backend.fs_blob_store(None, None).unwrap();
    let capacity = fs4::total_space(store.root()).unwrap();
    assert_eq!(
        store.floor_bytes(),
        blob::FsBlobStore::default_floor_for_capacity(capacity)
    );
    assert_eq!(
        backend.fs_blob_store(None, Some(0)).unwrap().floor_bytes(),
        0
    );
}

#[test]
fn blob_store_errors_for_in_memory_backend_with_no_override() {
    let backend = StorageBackend::memory().unwrap();
    assert!(backend.blob_store(None, None).is_err());
}

#[test]
fn blob_store_accepts_explicit_root_for_in_memory_backend() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::memory().unwrap();
    let store = backend.blob_store(Some(dir.path()), None);
    assert!(store.is_ok());
}

include!("backend/migration_tests.rs");

#[test]
fn pack_ddl_plan_rolls_back_all_statements_on_failure() {
    let backend = StorageBackend::memory().unwrap();
    let error = backend
        .apply_pack_ddl_statements(&[
            "CREATE TABLE IF NOT EXISTS pack_schema_first (id INTEGER PRIMARY KEY)",
            "CREATE INDEX IF NOT EXISTS pack_schema_second ON pack_schema_missing(id)",
        ])
        .unwrap_err();

    assert!(
        error.to_string().contains("pack_schema_missing"),
        "schema-plan error must retain the failing SQLite diagnostic: {error}"
    );

    let reader = backend.pool().reader().unwrap();
    let visible_objects: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
                 WHERE name IN ('pack_schema_first', 'pack_schema_second')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(visible_objects, 0);
}

#[test]
fn pack_ddl_plan_applies_all_statements_idempotently() {
    const PLAN: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS pack_schema_success (id INTEGER PRIMARY KEY, value TEXT)",
        "CREATE INDEX IF NOT EXISTS pack_schema_success_value_idx \
             ON pack_schema_success(value)",
    ];

    let backend = StorageBackend::memory().unwrap();
    backend.apply_pack_ddl_statements(PLAN).unwrap();
    backend.apply_pack_ddl_statements(PLAN).unwrap();

    let reader = backend.pool().reader().unwrap();
    let visible_objects: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
                 WHERE name IN ('pack_schema_success', 'pack_schema_success_value_idx')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(visible_objects, 2);
}

/// khive#1029 repro: a `create_entity`-shaped write sequence (entity
/// upsert, then FTS `upsert_document` on the SAME file-backed DB, SAME
/// `StorageBackend`/pool) against a fresh tenant DB file, with a short
/// `busy_timeout` so a genuine lock hang fails fast instead of burning
/// 30s. Runs with `write_queue_enabled: false` — the legacy pool-mutex /
/// standalone-connection path (`KHIVE_WRITE_QUEUE` unset/0 in the
/// hosted symptom report is one of the two configs to check; see the
/// `_write_queue_enabled` sibling below for the flag-on config).
fn issue_1029_pool(write_queue_enabled: bool) -> (tempfile::TempDir, StorageBackend) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("issue_1029.db");
    let config = crate::pool::PoolConfig {
        path: Some(path.clone()),
        busy_timeout: std::time::Duration::from_millis(200),
        write_queue_enabled: Some(write_queue_enabled),
        ..crate::pool::PoolConfig::for_test()
    };
    let pool = ConnectionPool::new(config).expect("fresh tenant-shaped pool should open");
    let backend = StorageBackend {
        pool: Arc::new(pool),
        is_file_backed: true,
        path: Some(path),
        vector_tables_ready: Default::default(),
        text_tables_ready: Default::default(),
        notes_seq_repair_runs: AtomicUsize::new(0),
        store_schemas: std::array::from_fn(|_| Arc::new(StoreSchemaGate::default())),
    };
    (dir, backend)
}

async fn issue_1029_create_entity_shaped_sequence(backend: &StorageBackend) -> Result<(), String> {
    let entities = backend
        .entities_for_namespace("tenant_ns")
        .map_err(|e| format!("entities_for_namespace: {e}"))?;
    let entity = khive_storage::entity::Entity::new("tenant_ns", "concept", "Issue1029Repro");
    let entity_id = entity.id;
    entities
        .upsert_entity(entity)
        .await
        .map_err(|e| format!("upsert_entity: {e}"))?;

    let text = backend.text("entities").map_err(|e| format!("text: {e}"))?;
    let doc = khive_storage::types::TextDocument {
        subject_id: entity_id,
        kind: khive_types::SubstrateKind::Entity,
        record_kind: None,
        title: Some("Issue1029Repro".to_string()),
        body: "issue 1029 repro body".to_string(),
        tags: vec![],
        namespace: "tenant_ns".to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    };
    text.upsert_document(doc)
        .await
        .map_err(|e| format!("fts_upsert: {e}"))
}

/// khive#1029 H1/H2 control: `KHIVE_WRITE_QUEUE` unset (legacy pool-mutex
/// / standalone-connection path for both stores, sharing ONE
/// `ConnectionPool` via ONE `StorageBackend` — the topology this test
/// exists to confirm or kill as the lock source, isolated from any
/// multi-pool or multi-backend wiring question).
#[tokio::test]
async fn issue_1029_create_entity_shaped_sequence_write_queue_off() {
    let (_dir, backend) = issue_1029_pool(false);
    let result = issue_1029_create_entity_shaped_sequence(&backend).await;
    assert!(
        result.is_ok(),
        "khive#1029 repro (KHIVE_WRITE_QUEUE off): fts_upsert step failed: {:?}",
        result.err()
    );
}

/// khive#1029 H1 direct test: `KHIVE_WRITE_QUEUE=1`, single shared
/// `ConnectionPool`/`StorageBackend` (so the pool-wide `WriterTask` is
/// shared by construction) — isolates whether the WriterTask's
/// transaction lifecycle itself (not a multi-pool topology) is the lock
/// source.
#[tokio::test]
async fn issue_1029_create_entity_shaped_sequence_write_queue_on() {
    let (_dir, backend) = issue_1029_pool(true);
    let result = issue_1029_create_entity_shaped_sequence(&backend).await;
    assert!(
        result.is_ok(),
        "khive#1029 repro (KHIVE_WRITE_QUEUE=1): fts_upsert step failed: {:?}",
        result.err()
    );
}

/// khive#1029 H2 direct test: TWO independent `ConnectionPool`s (hence
/// two independent writer connections / two independent `WriterTask`
/// `OnceLock`s) opened against the SAME tenant DB file — the shape a
/// per-store (rather than per-backend) pool construction would produce.
/// Entity writes go through pool A, the FTS write through pool B, each
/// with `write_queue_enabled: Some(true)` so each independently spawns its own
/// WriterTask on first access.
#[tokio::test]
async fn issue_1029_two_pools_same_file_write_queue_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("issue_1029_two_pools.db");

    let cfg = |p: std::path::PathBuf| crate::pool::PoolConfig {
        path: Some(p),
        busy_timeout: std::time::Duration::from_millis(200),
        write_queue_enabled: Some(true),
        ..crate::pool::PoolConfig::for_test()
    };

    let pool_a = ConnectionPool::new(cfg(path.clone())).expect("pool A should open");
    let backend_a = StorageBackend {
        pool: Arc::new(pool_a),
        is_file_backed: true,
        path: Some(path.clone()),
        vector_tables_ready: Default::default(),
        text_tables_ready: Default::default(),
        notes_seq_repair_runs: AtomicUsize::new(0),
        store_schemas: std::array::from_fn(|_| Arc::new(StoreSchemaGate::default())),
    };
    let pool_b = ConnectionPool::new(cfg(path.clone())).expect("pool B should open");
    let backend_b = StorageBackend {
        pool: Arc::new(pool_b),
        is_file_backed: true,
        path: Some(path),
        vector_tables_ready: Default::default(),
        text_tables_ready: Default::default(),
        notes_seq_repair_runs: AtomicUsize::new(0),
        store_schemas: std::array::from_fn(|_| Arc::new(StoreSchemaGate::default())),
    };

    let entities = backend_a
        .entities_for_namespace("tenant_ns")
        .expect("entities_for_namespace on pool A");
    let entity = khive_storage::entity::Entity::new("tenant_ns", "concept", "Issue1029TwoPools");
    let entity_id = entity.id;
    entities
        .upsert_entity(entity)
        .await
        .expect("pool A entity upsert should succeed");

    let text = backend_b.text("entities").expect("text on pool B");
    let doc = khive_storage::types::TextDocument {
        subject_id: entity_id,
        kind: khive_types::SubstrateKind::Entity,
        record_kind: None,
        title: Some("Issue1029TwoPools".to_string()),
        body: "issue 1029 two-pool repro body".to_string(),
        tags: vec![],
        namespace: "tenant_ns".to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    };
    let result = text.upsert_document(doc).await;
    assert!(
        result.is_ok(),
        "khive#1029 two-pool repro: fts_upsert on an independent pool for the \
             same tenant DB file failed: {:?}",
        result.err()
    );
}

/// Minimal thread-local capture subscriber for asserting emitted events —
/// mirrors the capture subscriber in `checkpoint.rs`'s tick tests.
struct StarvationCaptureSubscriber {
    events: Arc<std::sync::Mutex<Vec<std::collections::BTreeMap<String, String>>>>,
}

impl tracing::Subscriber for StarvationCaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        #[derive(Default)]
        struct FieldVisitor(std::collections::BTreeMap<String, String>);
        impl tracing::field::Visit for FieldVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Regression coverage for the lock-starvation diagnostic itself: when a
/// text write starves on the SQLite write lock, `with_writer_unmanaged`
/// must emit the WARN carrying the `tx_registry` snapshot — operation
/// name, open-transaction count, and the registered labels.
///
/// `#[serial(tx_registry)]`: the registry is a process-wide singleton
/// shared across this test binary; this group serializes every test that
/// registers fixture entries or asserts snapshot contents (see
/// `checkpoint.rs`, `pool.rs`, `sql_bridge.rs`). The assertion checks the
/// fixture label is PRESENT rather than the snapshot being exactly one
/// entry, so unrelated short-lived production registrations elsewhere in
/// the binary cannot flake it.
#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn issue_1029_starvation_warn_reports_registered_transactions() {
    let (_dir, backend) = issue_1029_pool(false);
    // Create the store (and its FTS DDL) BEFORE the lock is held, so the
    // starvation happens inside `upsert_document` itself.
    let text = backend.text("entities").expect("text store");

    // Hold a genuine SQLite write lock on a separate standalone writer
    // connection, with a registered fixture transaction the diagnostic
    // must surface.
    let holder =
        crate::pool::open_external_writer_fixture(&backend.pool).expect("holder connection");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("holder BEGIN IMMEDIATE");
    let fixture = khive_storage::tx_registry::register(Some("issue_1029_fixture_tx".to_string()));

    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = StarvationCaptureSubscriber {
        events: Arc::clone(&events),
    };
    let guard = tracing::subscriber::set_default(subscriber);

    let doc = khive_storage::types::TextDocument {
        subject_id: uuid::Uuid::new_v4(),
        kind: khive_types::SubstrateKind::Entity,
        record_kind: None,
        title: Some("Issue1029Starved".to_string()),
        body: "issue 1029 starvation diagnostic body".to_string(),
        tags: vec![],
        namespace: "tenant_ns".to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    };
    let result = text.upsert_document(doc).await;

    drop(guard);
    drop(fixture);
    holder
        .execute_batch("ROLLBACK")
        .expect("holder ROLLBACK releases the lock");

    assert!(
        result.is_err(),
        "upsert_document must starve while another connection holds the write lock"
    );

    let events = events.lock().unwrap();
    let warn = events
        .iter()
        .find(|fields| {
            fields
                .get("message")
                .is_some_and(|m| m.contains("text write starved"))
        })
        .unwrap_or_else(|| panic!("expected a starvation WARN, captured events: {events:?}"));
    assert!(
        warn.get("op").is_some_and(|op| op.contains("fts_upsert")),
        "WARN must name the starved operation, got: {warn:?}"
    );
    assert!(
        warn.get("open_txs")
            .is_some_and(|txs| txs.contains("issue_1029_fixture_tx")),
        "WARN must list the registered holder label, got: {warn:?}"
    );
    let count: usize = warn
        .get("open_tx_count")
        .expect("WARN must carry open_tx_count")
        .parse()
        .expect("open_tx_count must be numeric");
    assert!(
        count >= 1,
        "open_tx_count must count the fixture, got {count}"
    );
}
