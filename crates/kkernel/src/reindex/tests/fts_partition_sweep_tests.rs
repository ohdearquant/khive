use super::*;

async fn assert_canonical_companions_present(runtime: &KhiveRuntime) {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.expect("catalog reader");
    for table in [
        "fts_entities_rowids",
        "fts_entities_rowids_state",
        "fts_notes_rowids",
        "fts_notes_rowids_state",
    ] {
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1".into(),
                params: vec![SqlValue::Text(table.into())],
                label: None,
            })
            .await
            .expect("read canonical companion catalog");
        assert_eq!(rows.len(), 1, "reindex dropped {table}");
    }
    for table in ["fts_entities_rowids_state", "fts_notes_rowids_state"] {
        let rows = reader
            .query_all(SqlStatement {
                sql: format!("SELECT value FROM {table} WHERE key = 'backfill'"),
                params: vec![],
                label: None,
            })
            .await
            .expect("read retained completion marker");
        assert_eq!(rows.len(), 1, "missing completion marker in {table}");
        assert!(matches!(rows[0].get("value"), Some(SqlValue::Text(value)) if value == "complete"));
    }
}

async fn assert_document_mapped(runtime: &KhiveRuntime, table: &str, id: Uuid) {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.expect("map reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT COUNT(*) AS matched FROM {table} f \
                 JOIN {table}_rowids m ON m.rowid = f.rowid \
                   AND m.namespace = f.namespace AND m.subject_id = f.subject_id \
                 WHERE f.namespace = 'local' AND f.subject_id = ?1"
            ),
            params: vec![SqlValue::Text(id.to_string())],
            label: None,
        })
        .await
        .expect("read mapped FTS document");
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(rows[0].get("matched"), Some(SqlValue::Integer(1))),
        "{table} must retain exactly one mapped document for {id}"
    );
}

async fn create_and_check_records(runtime: &KhiveRuntime) {
    let token = runtime.authorize(Namespace::local()).expect("namespace");
    // Do not open a TextSearch accessor first: it would repair a missing map.
    let note = runtime
        .create_note(
            &token,
            "insight",
            Some("post-reindex note"),
            "postreindexnotesentinel",
            None,
            None,
            vec![],
        )
        .await
        .expect("ordinary note create after reindex");
    let (entity, _) = runtime
        .create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            "postreindexentitysentinel",
            None,
            None,
            vec![],
        )
        .await
        .expect("ordinary entity create after reindex");
    assert_document_mapped(runtime, "fts_notes", note.id).await;
    assert_document_mapped(runtime, "fts_entities", entity.id).await;
}

#[tokio::test]
async fn reindex_preserves_canonical_rowid_tables_and_subsequent_writes() {
    if crate::test_process::run_in_child() {
        return;
    }

    for prior_note in [false, true] {
        let dir = tempfile::tempdir().expect("owned store");
        let args = snapshot_reindex_args(dir.path(), false);
        let runtime = snapshot_test_runtime(&args);
        let token = runtime.authorize(Namespace::local()).expect("namespace");
        let (entity, _) = runtime
            .create_entity_with_embedding_report(
                &token,
                "concept",
                None,
                "reindexentitysentinel",
                Some("probe"),
                None,
                vec![],
            )
            .await
            .expect("seed entity");
        let note = if prior_note {
            Some(
                runtime
                    .create_note(
                        &token,
                        "insight",
                        Some("prior note"),
                        "prereindexnotesentinel",
                        None,
                        None,
                        vec![],
                    )
                    .await
                    .expect("seed note"),
            )
        } else {
            None
        };
        runtime
            .sql()
            .writer()
            .await
            .expect("fixture writer")
            .execute_script("CREATE VIRTUAL TABLE fts_notes_legacy_4231 USING fts5(body);".into())
            .await
            .expect("seed legacy FTS partition");

        run_reindex_without_embeddings(args)
            .await
            .expect("real graph reindex must succeed");

        // These raw reads precede every accessor or write that could heal DDL.
        assert_canonical_companions_present(&runtime).await;
        {
            let sql = runtime.sql();
            let mut reader = sql.reader().await.expect("legacy catalog reader");
            let rows = reader
                .query_all(SqlStatement {
                    sql: "SELECT name FROM sqlite_master WHERE name = 'fts_notes_legacy_4231'"
                        .into(),
                    params: vec![],
                    label: None,
                })
                .await
                .expect("read legacy partition catalog");
            assert!(
                rows.is_empty(),
                "reindex must still remove legacy partitions"
            );
        }
        create_and_check_records(&runtime).await;
        assert_document_mapped(&runtime, "fts_entities", entity.id).await;
        if let Some(note) = note {
            assert_document_mapped(&runtime, "fts_notes", note.id).await;
        }
        drop(runtime);

        let reopened = snapshot_test_runtime(&snapshot_reindex_args(dir.path(), false));
        assert_canonical_companions_present(&reopened).await;
        create_and_check_records(&reopened).await;
    }
}
