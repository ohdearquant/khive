use super::*;

const KINDS: [StoreSchemaKind; 5] = [
    StoreSchemaKind::Entities,
    StoreSchemaKind::Graph,
    StoreSchemaKind::Notes,
    StoreSchemaKind::Events,
    StoreSchemaKind::Agents,
];

fn schema_attempts(backend: &StorageBackend, kind: StoreSchemaKind) -> usize {
    backend.store_schemas[kind as usize]
        .attempts
        .load(Ordering::Relaxed)
}

fn index_exists(backend: &StorageBackend, name: &str) -> bool {
    let reader = backend.pool.reader().unwrap();
    reader
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn serving_store_schemas_run_every_ensure_once_and_leave_accessors_warm() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("prepare.db")).unwrap();

    backend.prepare_serving_store_schemas().unwrap();
    backend.prepare_serving_store_schemas().unwrap();
    for kind in KINDS {
        assert_eq!(schema_attempts(&backend, kind), 1, "{kind:?}");
    }

    let before = backend.pool.writer_acquisition_snapshot();
    backend.entities_for_namespace("local").unwrap();
    backend.graph_for_namespace("local").unwrap();
    backend.events_for_namespace("local").unwrap();
    backend.agents().unwrap();
    let after = backend.pool.writer_acquisition_snapshot();
    assert_eq!(
        after.pooled_acquisitions, before.pooled_acquisitions,
        "a prepared store accessor takes no writer"
    );
    for kind in KINDS {
        assert_eq!(schema_attempts(&backend, kind), 1, "{kind:?}");
    }
    assert_eq!(backend.notes_seq_repair_run_count(), 0);
}

#[test]
fn notes_seq_repair_reports_the_rows_it_inserted_once() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("notes-seq.db")).unwrap();
    {
        let writer = backend.pool.try_writer().unwrap();
        note::ensure_notes_schema(writer.conn()).unwrap();
        writer
            .conn()
            .execute_batch(
                "INSERT INTO notes(id, namespace, kind, content, created_at, updated_at) VALUES \
                 ('legacy-a', 'local', 'observation', 'a', 1, 1), \
                 ('legacy-b', 'local', 'observation', 'b', 2, 2), \
                 ('legacy-c', 'local', 'observation', 'c', 3, 3); \
                 DELETE FROM notes_seq WHERE note_id IN ('legacy-a', 'legacy-b');",
            )
            .unwrap();
    }

    assert_eq!(backend.repair_notes_seq_once().unwrap(), Some(2));
    assert_eq!(backend.repair_notes_seq_once().unwrap(), None);
    backend.notes().unwrap();
    assert_eq!(backend.notes_seq_repair_run_count(), 1);

    let reader = backend.pool.reader().unwrap();
    let mapped: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM notes_seq WHERE note_id IN ('legacy-a', 'legacy-b', 'legacy-c')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mapped, 3);
}

#[test]
fn notes_seq_repair_after_a_notes_acquisition_does_not_run_again() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("notes-first.db")).unwrap();
    backend.notes().unwrap();
    assert_eq!(backend.repair_notes_seq_once().unwrap(), None);
    assert_eq!(backend.notes_seq_repair_run_count(), 1);
}

#[test]
fn mapped_text_table_keys_list_only_tables_with_a_rowid_map() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("mapped.db")).unwrap();
    backend.text("alpha").unwrap();
    backend.text("beta").unwrap();
    {
        let writer = backend.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE VIRTUAL TABLE fts_triggered USING fts5(body);")
            .unwrap();
    }

    let keys = backend.mapped_text_table_keys().unwrap();
    assert!(keys.iter().any(|key| key == "alpha"), "{keys:?}");
    assert!(keys.iter().any(|key| key == "beta"), "{keys:?}");
    assert!(!keys.iter().any(|key| key == "triggered"), "{keys:?}");
    assert!(
        keys.iter().all(|key| !key.ends_with("_rowids")
            && !key.ends_with("_rowids_state")
            && !key.ends_with("_data")),
        "only FTS tables themselves are listed: {keys:?}"
    );
}

#[test]
fn forced_index_check_rebuilds_a_dropped_index_and_then_reads_clean() {
    let dir = tempfile::tempdir().unwrap();
    let backend = StorageBackend::sqlite_for_test(dir.path().join("forced.db")).unwrap();
    backend.prepare_serving_store_schemas().unwrap();

    let clean = backend.check_serving_forced_indexes().unwrap();
    assert_eq!(clean.checked, 7);
    assert!(clean.rebuilt.is_empty(), "{clean:?}");

    {
        let writer = backend.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "DROP INDEX idx_graph_edges_ns_src_rel; \
                 DROP INDEX idx_notes_message_recipient_direction;",
            )
            .unwrap();
    }
    let repaired = backend.check_serving_forced_indexes().unwrap();
    assert_eq!(
        repaired.rebuilt,
        vec![
            "idx_graph_edges_ns_src_rel",
            "idx_notes_message_recipient_direction"
        ]
    );
    assert!(index_exists(&backend, "idx_graph_edges_ns_src_rel"));
    assert!(index_exists(
        &backend,
        "idx_notes_message_recipient_direction"
    ));
    assert_eq!(schema_attempts(&backend, StoreSchemaKind::Graph), 2);
    assert_eq!(schema_attempts(&backend, StoreSchemaKind::Notes), 2);
    assert_eq!(schema_attempts(&backend, StoreSchemaKind::Entities), 1);

    let again = backend.check_serving_forced_indexes().unwrap();
    assert!(again.rebuilt.is_empty(), "{again:?}");
}

#[test]
fn forced_index_check_on_a_read_only_store_names_the_missing_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forced-ro.db");
    {
        let writable = StorageBackend::sqlite_for_test(&path).unwrap();
        writable.prepare_serving_store_schemas().unwrap();
        let writer = writable.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("DROP INDEX idx_notes_created;")
            .unwrap();
    }
    let readonly = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let error = readonly.check_serving_forced_indexes().unwrap_err();
    assert!(error.to_string().contains("idx_notes_created"), "{error}");
}
