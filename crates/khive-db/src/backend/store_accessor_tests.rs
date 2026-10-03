use super::*;
use std::sync::{mpsc, Barrier};
use std::time::Duration;

const KINDS: [StoreSchemaKind; 5] = [
    StoreSchemaKind::Entities,
    StoreSchemaKind::Graph,
    StoreSchemaKind::Notes,
    StoreSchemaKind::Events,
    StoreSchemaKind::Agents,
];

fn fetch_store(
    backend: &StorageBackend,
    kind: StoreSchemaKind,
    namespace: &str,
) -> Result<(), SqliteError> {
    match kind {
        StoreSchemaKind::Entities => backend.entities_for_namespace(namespace).map(|_| ()),
        StoreSchemaKind::Graph => backend.graph_for_namespace(namespace).map(|_| ()),
        StoreSchemaKind::Notes => backend.notes_for_namespace(namespace).map(|_| ()),
        StoreSchemaKind::Events => backend.events_for_namespace(namespace).map(|_| ()),
        StoreSchemaKind::Agents => backend.agents().map(|_| ()),
    }
}

fn schema_attempts(backend: &StorageBackend, kind: StoreSchemaKind) -> usize {
    backend.store_schemas[kind as usize]
        .attempts
        .load(Ordering::Relaxed)
}

fn table_for(kind: StoreSchemaKind) -> &'static str {
    match kind {
        StoreSchemaKind::Entities => "entities",
        StoreSchemaKind::Graph => "graph_edges",
        StoreSchemaKind::Notes => "notes",
        StoreSchemaKind::Events => "events",
        StoreSchemaKind::Agents => "agents",
    }
}

#[test]
fn store_accessors_initialize_each_kind_once_across_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("once.db")).unwrap();

    for kind in KINDS {
        let before = backend.pool.writer_acquisition_snapshot();
        for namespace in ["local", "tenant_a", "tenant_b", "local"] {
            fetch_store(&backend, kind, namespace).unwrap();
        }
        let after = backend.pool.writer_acquisition_snapshot();
        assert_eq!(schema_attempts(&backend, kind), 1, "{kind:?}");
        assert_eq!(
            after.pooled_acquisitions - before.pooled_acquisitions,
            1,
            "only the cold {kind:?} accessor checks out a writer"
        );
        assert_eq!(
            after.writer_task_acquisitions,
            before.writer_task_acquisitions
        );
        assert_eq!(
            after.standalone_acquisitions,
            before.standalone_acquisitions
        );
        let reader = backend.pool.reader().unwrap();
        assert!(sqlite_table_exists(reader.conn(), table_for(kind)).unwrap());
    }
    assert_eq!(backend.notes_seq_repair_run_count(), 1);
}

#[test]
fn warm_store_accessors_finish_while_pool_writer_mutex_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(StorageBackend::sqlite_for_test(dir.path().join("warm.db")).unwrap());
    for kind in KINDS {
        fetch_store(&backend, kind, "local").unwrap();
        let writer = backend.pool.try_writer().unwrap();
        let before = backend.pool.writer_acquisition_snapshot();
        let worker_backend = Arc::clone(&backend);
        let (finished, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            finished
                .send(fetch_store(&worker_backend, kind, "tenant_a"))
                .unwrap();
        });
        let while_held = result.recv_timeout(Duration::from_secs(2));
        // Release before joining even when an accessor incorrectly waits.
        drop(writer);
        worker.join().unwrap();
        while_held
            .expect("warm accessor must return while the pool writer mutex is held")
            .unwrap();
        let after = backend.pool.writer_acquisition_snapshot();
        assert_eq!(
            after.pooled_acquisitions, before.pooled_acquisitions,
            "{kind:?}"
        );
        assert_eq!(schema_attempts(&backend, kind), 1);
    }
}

#[test]
fn failed_store_schema_ddl_is_retryable_for_each_kind() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let backend = StorageBackend::sqlite_for_test(dir.path().join("retry.db")).unwrap();
        {
            let writer = backend.pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("PRAGMA query_only = ON")
                .unwrap();
        }
        let error = fetch_store(&backend, kind, "local").unwrap_err();
        assert!(
            matches!(error, SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(ref detail, _))
                if detail.code == rusqlite::ErrorCode::ReadOnly),
            "real SQLite DDL refusal expected for {kind:?}: {error}"
        );
        assert_eq!(schema_attempts(&backend, kind), 1);
        {
            let writer = backend.pool.try_writer().unwrap();
            assert!(!sqlite_table_exists(writer.conn(), table_for(kind)).unwrap());
            writer
                .conn()
                .execute_batch("PRAGMA query_only = OFF")
                .unwrap();
        }
        fetch_store(&backend, kind, "local").unwrap();
        fetch_store(&backend, kind, "tenant_a").unwrap();
        assert_eq!(schema_attempts(&backend, kind), 2, "{kind:?} retries once");
        let reader = backend.pool.reader().unwrap();
        assert!(sqlite_table_exists(reader.conn(), table_for(kind)).unwrap());
    }
}

#[test]
fn notes_sequence_repair_retries_after_schema_success() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("notes-repair.db")).unwrap();
    let denied = Arc::new(AtomicBool::new(false));
    {
        let writer = backend.pool.try_writer().unwrap();
        note::ensure_notes_schema(writer.conn()).unwrap();
        writer
            .conn()
            .execute_batch(
                "INSERT INTO notes(id, namespace, kind, content, created_at, updated_at) \
                 VALUES ('legacy-note', 'local', 'observation', 'legacy', 1, 1); \
                 DELETE FROM notes_seq WHERE note_id = 'legacy-note';",
            )
            .unwrap();
        let fired = Arc::clone(&denied);
        writer
            .conn()
            .authorizer(Some(move |ctx: AuthContext<'_>| {
                if matches!(ctx.action, AuthAction::Insert { table_name } if table_name == "notes_seq") {
                    fired.store(true, Ordering::Relaxed);
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))
            .unwrap();
    }
    let error = backend.notes().map(|_| ()).unwrap_err();
    assert!(matches!(error, SqliteError::Rusqlite(_)));
    assert!(
        denied.load(Ordering::Relaxed),
        "repair reached SQLite INSERT"
    );
    assert_eq!(schema_attempts(&backend, StoreSchemaKind::Notes), 1);
    assert_eq!(backend.notes_seq_repair_run_count(), 0);
    {
        let writer = backend.pool.try_writer().unwrap();
        writer
            .conn()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        let missing: i64 = writer
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM notes_seq WHERE note_id = 'legacy-note'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missing, 0);
    }
    backend.notes().unwrap();
    backend.notes_for_namespace("tenant_a").unwrap();
    assert_eq!(schema_attempts(&backend, StoreSchemaKind::Notes), 1);
    assert_eq!(backend.notes_seq_repair_run_count(), 1);
    let reader = backend.pool.reader().unwrap();
    let repaired: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM notes_seq WHERE note_id = 'legacy-note'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(repaired, 1);
}

#[test]
fn readonly_store_accessors_do_not_attempt_schema_or_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("readonly.db");
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        for kind in KINDS {
            fetch_store(&writable, kind, "local").unwrap();
        }
    }
    let readonly = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let before = readonly.pool.writer_acquisition_snapshot();
    for kind in KINDS {
        fetch_store(&readonly, kind, "local").unwrap();
        fetch_store(&readonly, kind, "tenant_a").unwrap();
        assert_eq!(schema_attempts(&readonly, kind), 0, "{kind:?}");
    }
    let after = readonly.pool.writer_acquisition_snapshot();
    assert_eq!(after.pooled_acquisitions, before.pooled_acquisitions);
    assert_eq!(
        after.writer_task_acquisitions,
        before.writer_task_acquisitions
    );
    assert_eq!(readonly.notes_seq_repair_run_count(), 0);
}

#[tokio::test]
async fn cold_store_accessors_honor_admitted_request_stop() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let backend = StorageBackend::sqlite_for_test(dir.path().join("stopped.db")).unwrap();
        let before = backend.pool.writer_acquisition_snapshot();
        let (_sender, receiver) = tokio::sync::watch::channel(true);
        let result = khive_storage::scope_request_read_cancellation(receiver, async {
            khive_storage::capture_request_read_context()
                .scope_store_acquisition("cold_store_accessor", || {
                    fetch_store(&backend, kind, "local")
                })
        })
        .await;
        assert!(
            matches!(result, Err(SqliteError::RequestReadStopped(_))),
            "{kind:?}"
        );
        assert_eq!(schema_attempts(&backend, kind), 0);
        assert_eq!(
            backend
                .pool
                .writer_acquisition_snapshot()
                .pooled_acquisitions,
            before.pooled_acquisitions
        );
    }
}

#[test]
fn concurrent_first_store_accessors_apply_one_schema_batch() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            Arc::new(StorageBackend::sqlite_for_test(dir.path().join("racing.db")).unwrap());
        let start = Arc::new(Barrier::new(9));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let backend = Arc::clone(&backend);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    fetch_store(&backend, kind, "local")
                })
            })
            .collect();
        start.wait();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert_eq!(schema_attempts(&backend, kind), 1, "{kind:?}");
        if matches!(kind, StoreSchemaKind::Notes) {
            assert_eq!(backend.notes_seq_repair_run_count(), 1);
        }
    }
}

#[test]
fn schema_index_dropped_externally_is_repaired_on_backend_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.db");
    {
        let backend = StorageBackend::sqlite_for_test(&path).unwrap();
        backend.entities().unwrap();
        {
            let external = rusqlite::Connection::open(&path).unwrap();
            external
                .execute_batch("DROP INDEX idx_entities_namespace")
                .unwrap();
        }
        backend.entities_for_namespace("tenant_a").unwrap();
        let reader = backend.pool.reader().unwrap();
        let present: bool = reader
            .conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'idx_entities_namespace')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!present);
    }
    let reopened = StorageBackend::sqlite_for_test(&path).unwrap();
    reopened.entities().unwrap();
    let reader = reopened.pool.reader().unwrap();
    let present: bool = reader
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'idx_entities_namespace')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(present);
}

#[test]
fn empty_scoped_store_namespace_is_rejected_before_initialization() {
    let backend = StorageBackend::memory().unwrap();
    let before = backend.pool.writer_acquisition_snapshot();
    for kind in [
        StoreSchemaKind::Entities,
        StoreSchemaKind::Graph,
        StoreSchemaKind::Notes,
        StoreSchemaKind::Events,
    ] {
        assert!(matches!(
            fetch_store(&backend, kind, " \t"),
            Err(SqliteError::InvalidData(_))
        ));
        assert_eq!(schema_attempts(&backend, kind), 0);
    }
    assert_eq!(
        backend
            .pool
            .writer_acquisition_snapshot()
            .pooled_acquisitions,
        before.pooled_acquisitions
    );
}
