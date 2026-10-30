//! Workspace mirror progress stored outside the graph.
//!
//! Values are caller-owned: this module does not normalize paths, generate hashes,
//! read a clock, or interpret timestamp units. A path is a slot in the supplied
//! database, with no implicit namespace or workspace partition.
//!
//! Prepare parsing and masking before entering a write transaction. Advance a
//! cursor only after the corresponding file writes succeed, using the SAME
//! transaction's writer. This module does not start or commit a transaction.

use khive_storage::types::{SqlRow, SqlStatement, SqlValue, StorageResult};
use khive_storage::{SqlReader, SqlWriter, StorageCapability, StorageError};

/// One stored row, retaining SQL NULL without substituting empty strings or zero.
///
/// New file slots use text paths. Reads also preserve any NULL path admitted by
/// the table's original `TEXT PRIMARY KEY` declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorRow {
    pub path: Option<String>,
    pub last_mtime: Option<i64>,
    pub last_hash: Option<String>,
    pub last_synced_at: Option<i64>,
}

fn decode(row: SqlRow) -> StorageResult<CursorRow> {
    let decode_column =
        |error| StorageError::driver(StorageCapability::Sql, "workspace cursor decode", error);
    Ok(CursorRow {
        path: row
            .opt_text("path")
            .map_err(decode_column)?
            .map(str::to_owned),
        last_mtime: row.opt_i64("last_mtime").map_err(decode_column)?,
        last_hash: row
            .opt_text("last_hash")
            .map_err(decode_column)?
            .map(str::to_owned),
        last_synced_at: row.opt_i64("last_synced_at").map_err(decode_column)?,
    })
}

/// Read one exact text path. Only an absent row becomes `None`; SQL and decoding
/// errors, including a missing table, propagate.
pub async fn get(reader: &mut dyn SqlReader, path: &str) -> StorageResult<Option<CursorRow>> {
    reader
        .query_row(SqlStatement {
            sql: "SELECT path, last_mtime, last_hash, last_synced_at \
                  FROM workspace_mirror_cursor WHERE path = ?1"
                .into(),
            params: vec![SqlValue::Text(path.to_owned())],
            label: Some("workspace_mirror_cursor_get".into()),
        })
        .await?
        .map(decode)
        .transpose()
}

/// List all stored cursor rows. This deliberately materializes the whole table;
/// it is not a bounded or paginated listing. Ties between nullable path slots are
/// ordered by their stored values, without transforming those values.
pub async fn list(reader: &mut dyn SqlReader) -> StorageResult<Vec<CursorRow>> {
    reader
        .query_all(SqlStatement {
            sql: "SELECT path, last_mtime, last_hash, last_synced_at \
                  FROM workspace_mirror_cursor \
                  ORDER BY path, last_mtime, last_hash, last_synced_at"
                .into(),
            params: vec![],
            label: Some("workspace_mirror_cursor_list".into()),
        })
        .await?
        .into_iter()
        .map(decode)
        .collect()
}

/// Replace the progress values for one exact text path on the caller's writer.
///
/// The caller must invoke this only after successful file writes, in their same
/// `SqlAccess::atomic_unit`. On any parse, mask, or write failure, do not advance
/// the cursor. A later error from the atomic unit rolls this DML back as well.
///
/// This helper issues one parameterized DML statement and no transaction control
/// or external work. On an atomic unit's inline writer it must complete on the
/// first poll, just like the other DML in that unit.
pub async fn upsert_on_writer(
    writer: &mut dyn SqlWriter,
    path: &str,
    last_mtime: Option<i64>,
    last_hash: Option<&str>,
    last_synced_at: Option<i64>,
) -> StorageResult<()> {
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO workspace_mirror_cursor \
                  (path, last_mtime, last_hash, last_synced_at) VALUES (?1, ?2, ?3, ?4) \
                  ON CONFLICT(path) DO UPDATE SET \
                  last_mtime = excluded.last_mtime, \
                  last_hash = excluded.last_hash, \
                  last_synced_at = excluded.last_synced_at"
                .into(),
            params: vec![
                SqlValue::Text(path.to_owned()),
                SqlValue::from_opt_i64(last_mtime),
                SqlValue::from_opt_text(last_hash),
                SqlValue::from_opt_i64(last_synced_at),
            ],
            label: Some("workspace_mirror_cursor_upsert".into()),
        })
        .await?;
    Ok(())
}

/// Read the exact slot on a transaction writer without a trait-object upcast.
pub(crate) async fn get_on_writer(
    writer: &mut dyn SqlWriter,
    path: &str,
) -> StorageResult<Option<CursorRow>> {
    writer
        .query_row(SqlStatement {
            sql: "SELECT path, last_mtime, last_hash, last_synced_at \
                  FROM workspace_mirror_cursor WHERE path = ?1"
                .into(),
            params: vec![SqlValue::Text(path.to_owned())],
            label: Some("workspace_mirror_cursor_get".into()),
        })
        .await?
        .map(decode)
        .transpose()
}

#[cfg(test)]
mod tests {
    include!("cursor/tests.rs");
    include!("cursor/mirror_tests.rs");

    #[tokio::test]
    async fn mirror_non_utf8_file_is_named_while_valid_file_commits() {
        const NAME: &str = "mirror_non_utf8_file_is_named_while_valid_file_commits";
        if mirror_child(NAME) {
            return;
        }
        mirror_bounded(async {
            let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
            {
                let baseline = MirrorTree::new();
                baseline.write(".khive/notes/baseline.md", "valid baseline prose");
                let (runtime, registry, token) = mirror_fixture();
                let result = crate::mirror::mirror_directory_once(
                    &runtime,
                    &registry,
                    &token,
                    &baseline.0,
                    &config,
                )
                .await
                .expect("valid-only caller baseline before mixed tree");
                assert_eq!(result.committed, 1);
                assert!(result.skipped_non_utf8.is_empty());
            }
            let tree = MirrorTree::new();
            tree.write(".khive/notes/invalid.md", [0xff, 0xfe, 0x80]);
            tree.write(".khive/notes/valid.md", "valid mixed-tree prose");
            let (runtime, registry, token) = mirror_fixture();
            let summary =
                crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                    .await
                    .expect("mixed tree must skip invalid UTF-8 and continue");
            assert_eq!(
                summary.committed, 1,
                "valid file must commit in the mixed pass"
            );
            assert_eq!(summary.unchanged, 0);
            assert_eq!(
                summary.skipped_non_utf8,
                vec![std::path::PathBuf::from(".khive/notes/invalid.md")]
            );
            let (valid_uri, valid_key) = mirror_slot(&tree, ".khive/notes/valid.md", &token);
            let (invalid_uri, invalid_key) = mirror_slot(&tree, ".khive/notes/invalid.md", &token);
            let documents = mirror_documents(&runtime, &token).await;
            assert_eq!(
                documents.len(),
                1,
                "invalid bytes must not create a document"
            );
            assert_eq!(documents[0]["properties"]["source_uri"], valid_uri);
            assert_eq!(documents[0]["description"], "valid mixed-tree prose");
            assert!(documents
                .iter()
                .all(|doc| doc["properties"]["source_uri"] != invalid_uri));
            assert!(read(&runtime, &valid_key).await.is_some());
            assert!(read(&runtime, &invalid_key).await.is_none());
            assert_eq!(rows(&runtime).await.len(), 1);
        })
        .await;
        mirror_done(NAME);
    }

    #[tokio::test]
    async fn mirror_non_utf8_second_pass_names_skip_and_preserves_state() {
        const NAME: &str = "mirror_non_utf8_second_pass_names_skip_and_preserves_state";
        if mirror_child(NAME) {
            return;
        }
        mirror_bounded(async {
            let tree = MirrorTree::new();
            tree.write(".khive/notes/invalid.md", [0xff, 0xfe, 0x80]);
            tree.write(".khive/notes/valid.md", "stable valid prose");
            let relative_invalid = std::path::PathBuf::from(".khive/notes/invalid.md");
            let (runtime, registry, token) = mirror_fixture();
            let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
            let first =
                crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                    .await
                    .expect("first mixed pass");
            assert_eq!(first.committed, 1);
            assert_eq!(first.skipped_non_utf8, vec![relative_invalid.clone()]);
            let documents = mirror_documents(&runtime, &token).await;
            let cursors = rows(&runtime).await;
            let edges = mirror_edges(&runtime, &token).await;
            let lexical = mirror_lexical_rows(&runtime).await;
            assert_eq!(documents.len(), 1);
            assert_eq!(cursors.len(), 1);
            let second =
                crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                    .await
                    .expect("second mixed pass must still skip invalid bytes");
            assert_eq!(second.committed, 0);
            assert_eq!(second.unchanged, 1);
            assert_eq!(second.skipped_non_utf8, vec![relative_invalid]);
            assert_eq!(mirror_documents(&runtime, &token).await, documents);
            assert_eq!(rows(&runtime).await, cursors);
            assert_eq!(mirror_edges(&runtime, &token).await, edges);
            assert_eq!(mirror_lexical_rows(&runtime).await, lexical);
            let (_, invalid_key) = mirror_slot(&tree, ".khive/notes/invalid.md", &token);
            assert!(read(&runtime, &invalid_key).await.is_none());
        })
        .await;
        mirror_done(NAME);
    }

    fn mirror_uri_heads(
        documents: &[serde_json::Value],
        edges: &[serde_json::Value],
        uri: &str,
    ) -> Vec<serde_json::Value> {
        documents
            .iter()
            .filter(|document| document["properties"]["source_uri"] == uri)
            .filter(|document| !edges.iter().any(|edge| edge["target_id"] == document["id"]))
            .cloned()
            .collect()
    }

    async fn mirror_assert_head_and_cursor(
        runtime: &KhiveRuntime,
        token: &khive_runtime::NamespaceToken,
        uri: &str,
        key: &str,
        expected_edges: usize,
        expected_cursors: usize,
    ) {
        let documents = mirror_documents(runtime, token).await;
        let edges = mirror_edges(runtime, token).await;
        assert_eq!(
            documents.len(),
            2,
            "exactly one additional document version"
        );
        assert_eq!(edges.len(), expected_edges, "exact supersedes edge count");
        let heads = mirror_uri_heads(&documents, &edges, uri);
        assert_eq!(heads.len(), 1, "the URI must have exactly one current head");
        let cursor = read(runtime, key).await.expect("current file cursor");
        assert_eq!(
            cursor.last_hash,
            heads[0]["properties"]["checksum"]
                .as_str()
                .map(str::to_owned),
            "cursor must match the unique current document"
        );
        assert_eq!(rows(runtime).await.len(), expected_cursors);
        assert_eq!(mirror_lexical_rows(runtime).await, (2, 2));
    }

    async fn accessor_previous_document(
        runtime: &KhiveRuntime,
        token: &khive_runtime::NamespaceToken,
        uri: &str,
    ) -> crate::mirror::PreviousDocument {
        use crate::mirror::PreviousDocument;
        let documents: Vec<_> = mirror_documents(runtime, token)
            .await
            .into_iter()
            .filter(|document| document["properties"]["source_uri"] == uri)
            .collect();
        if documents.is_empty() {
            return PreviousDocument::Absent;
        }
        let edges = mirror_edges(runtime, token).await;
        let heads: Vec<_> = documents
            .iter()
            .filter(|document| !edges.iter().any(|edge| edge["target_id"] == document["id"]))
            .collect();
        if heads.len() != 1 {
            return PreviousDocument::Ambiguous;
        }
        PreviousDocument::Current(heads[0]["id"].as_str().unwrap().parse().unwrap())
    }

    #[tokio::test]
    async fn mirror_head_query_matches_live_accessor_scope() {
        const NAME: &str = "mirror_head_query_matches_live_accessor_scope";
        if mirror_child(NAME) {
            return;
        }
        mirror_bounded(async {
            use crate::mirror::PreviousDocument;
            let (runtime, _registry, token) = mirror_fixture();
            let id = |number| uuid::Uuid::from_u128(number).to_string();
            let one = id(1);
            let absent_deleted = id(2);
            let foreign = id(3);
            let deleted_peer = id(4);
            let cycle = id(5);
            let first = id(6);
            let second = id(7);
            let foreign_only = id(8);
            let other_kind = id(9);
            mirror_script(&runtime, &format!(
                "INSERT INTO entities(id,namespace,kind,name,properties,created_at,updated_at,deleted_at) VALUES \
                 ('{one}','local','document','live',json_object('source_uri','test://one'),1,1,NULL), \
                 ('{absent_deleted}','local','document','deleted',json_object('source_uri','test://deleted'),1,1,2), \
                 ('{foreign}','other','document','foreign',json_object('source_uri','test://one'),1,1,NULL), \
                 ('{deleted_peer}','local','document','deleted peer',json_object('source_uri','test://one'),1,1,2), \
                 ('{cycle}','local','document','cycle',json_object('source_uri','test://cycle'),1,1,NULL), \
                 ('{first}','local','document','first',json_object('source_uri','test://two'),1,1,NULL), \
                 ('{second}','local','document','second',json_object('source_uri','test://two'),1,1,NULL), \
                 ('{foreign_only}','other','document','foreign only',json_object('source_uri','test://foreign'),1,1,NULL), \
                 ('{other_kind}','local','concept','other kind',json_object('source_uri','test://kind'),1,1,NULL); \
                 INSERT INTO graph_edges(namespace,id,source_id,target_id,relation,created_at,updated_at,deleted_at) VALUES \
                 ('local','{edge_deleted}','{deleted_peer}','{one}','supersedes',1,1,2), \
                 ('other','{edge_foreign}','{foreign}','{one}','supersedes',1,1,NULL), \
                 ('local','{edge_cycle}','{cycle}','{cycle}','supersedes',1,1,NULL);",
                edge_deleted=id(101), edge_foreign=id(102), edge_cycle=id(103),
            )).await;
            let expected = [
                ("test://missing", PreviousDocument::Absent),
                ("test://deleted", PreviousDocument::Absent),
                ("test://foreign", PreviousDocument::Absent),
                ("test://kind", PreviousDocument::Absent),
                ("test://one", PreviousDocument::Current(one.parse().unwrap())),
                ("test://cycle", PreviousDocument::Ambiguous),
                ("test://two", PreviousDocument::Ambiguous),
            ];
            for (uri, expected) in expected {
                let accessor = accessor_previous_document(&runtime, &token, uri).await;
                assert_eq!(accessor, expected, "accessor fixture for {uri}");
                let sql = crate::mirror::previous_document_for_test(&runtime, &token, uri)
                    .await.expect("single-snapshot head lookup");
                assert_eq!(sql, accessor, "head query differs from accessor for {uri}");
            }
        }).await;
        mirror_done(NAME);
    }

    #[tokio::test]
    async fn mirror_missing_graph_schema_refuses_before_document_write() {
        const NAME: &str = "mirror_missing_graph_schema_refuses_before_document_write";
        if mirror_child(NAME) {
            return;
        }
        mirror_bounded(async {
            let tree = MirrorTree::new();
            let relative = ".khive/notes/missing-graph.md";
            tree.write(relative, "first pass with graph schema absent");
            let (runtime, registry, token) = mirror_fixture();
            let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
            let (_, key) = mirror_slot(&tree, relative, &token);
            assert!(mirror_documents(&runtime, &token).await.is_empty());
            assert!(read(&runtime, &key).await.is_none());
            assert_eq!(mirror_lexical_rows(&runtime).await, (0, 0));
            mirror_script(&runtime, "DROP TABLE graph_edges;").await;
            let mut reader = runtime.sql().reader().await.unwrap();
            let graph_table = reader
                .query_row(SqlStatement::new(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'graph_edges'",
                    vec![],
                ))
                .await
                .unwrap();
            assert!(graph_table.is_none(), "missing-schema fixture premise");
            drop(reader);

            let error =
                crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                    .await
                    .expect_err("missing graph schema must refuse the first pass");
            assert_eq!(
                error.commit_state,
                crate::mirror::FileCommitState::NotAttempted
            );
            assert!(
                matches!(
                    error.runtime_error(),
                    Some(khive_runtime::RuntimeError::Storage(_))
                ),
                "missing graph schema must retain the storage error: {error}"
            );
            assert_eq!(error.progress.committed, 0);
            assert!(mirror_documents(&runtime, &token).await.is_empty());
            assert!(read(&runtime, &key).await.is_none());
            assert_eq!(mirror_lexical_rows(&runtime).await, (0, 0));
        })
        .await;
        mirror_done(NAME);
    }

    #[tokio::test]
    async fn mirror_head_observation_is_consistent_across_concurrent_commit() {
        const NAME: &str = "mirror_head_observation_is_consistent_across_concurrent_commit";
        if mirror_child(NAME) {
            return;
        }
        mirror_bounded(async {
            let tree = MirrorTree::new();
            let relative = ".khive/notes/snapshot.md";
            tree.write(relative, "initial deterministic mirror body");
            let (runtime, registry, token) = mirror_fixture();
            let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
            let baseline =
                crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                    .await
                    .expect("positive initial mirror fixture");
            assert_eq!(baseline.committed, 1);
            assert_eq!(baseline.unchanged, 0);
            assert!(baseline.skipped_no_current.is_empty());
            assert!(baseline.skipped_entries.is_empty());
            assert!(baseline.refused.is_empty());
            assert!(baseline.skipped_non_utf8.is_empty());
            assert_eq!(mirror_documents(&runtime, &token).await.len(), 1);
            assert!(mirror_edges(&runtime, &token).await.is_empty());
            assert_eq!(mirror_lexical_rows(&runtime).await, (1, 1));
            let (uri, key) = mirror_slot(&tree, relative, &token);
            assert!(read(&runtime, &key).await.is_some());
            tree.write(relative, "changed deterministic mirror body");

            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let mut hook = crate::mirror::HeadReadHook {
                reached: Some(reached_tx),
                release: Some(release_rx),
            };
            let (paused, winner) = tokio::join!(
                crate::mirror::mirror_directory_once_with_head_hook(
                    &runtime, &registry, &token, &tree.0, &config, &mut hook,
                ),
                async {
                    reached_rx
                        .await
                        .expect("paused pass captured its head observation");
                    let winner = crate::mirror::mirror_directory_once(
                        &runtime, &registry, &token, &tree.0, &config,
                    )
                    .await;
                    release_tx.send(()).expect("release paused pass");
                    winner
                },
            );
            let winner = winner.expect("the competing pass commits");
            assert_eq!(winner.committed, 1);
            assert_eq!(winner.unchanged, 0);
            assert!(winner.skipped_no_current.is_empty());
            assert!(winner.skipped_entries.is_empty());
            assert!(winner.refused.is_empty());
            assert!(winner.skipped_non_utf8.is_empty());

            let paused_error = match paused {
                Ok(summary) => {
                    assert!(
                        summary.skipped_no_current.is_empty(),
                        "mixed history/edge snapshots reported skipped_no_current: {summary:?}"
                    );
                    panic!("the stale cursor must be refused, got {summary:?}");
                }
                Err(error) => error,
            };
            assert_eq!(
                paused_error.commit_state,
                crate::mirror::FileCommitState::Unconfirmed
            );
            assert!(paused_error.progress.skipped_no_current.is_empty());
            assert!(
                paused_error
                    .to_string()
                    .contains("workspace mirror cursor changed before commit"),
                "unexpected stale-pass result: {paused_error}"
            );
            mirror_assert_head_and_cursor(&runtime, &token, &uri, &key, 1, 1).await;
        })
        .await;
        mirror_done(NAME);
    }

    #[tokio::test]
    async fn mirror_concurrent_changed_passes_keep_one_current_head() {
        const NAME: &str = "mirror_concurrent_changed_passes_keep_one_current_head";
        if mirror_child(NAME) {
            return;
        }
        mirror_bounded(async {
            let tree = MirrorTree::new();
            let relative = ".khive/notes/concurrent.md";
            tree.write(relative, "initial concurrent mirror body");
            let (runtime, registry, token) = mirror_fixture();
            let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
            let baseline = crate::mirror::mirror_directory_once(
                &runtime, &registry, &token, &tree.0, &config,
            ).await.expect("positive concurrent-pass baseline");
            assert_eq!(baseline.committed, 1);
            assert_eq!(mirror_documents(&runtime, &token).await.len(), 1);
            assert_eq!(mirror_lexical_rows(&runtime).await, (1, 1));
            assert!(mirror_edges(&runtime, &token).await.is_empty());
            let (uri, key) = mirror_slot(&tree, relative, &token);
            assert!(read(&runtime, &key).await.is_some());
            tree.write(relative, "changed concurrent mirror body");
            let (left, right) = tokio::join!(
                crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config),
                crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config),
            );
            let mut committed = 0;
            for result in [left, right] {
                match result {
                    Ok(summary) => {
                        assert!(summary.skipped_non_utf8.is_empty());
                        assert_eq!(summary.committed + summary.unchanged, 1);
                        committed += summary.committed;
                    }
                    Err(error) => {
                        let pre_unit = error.commit_state == crate::mirror::FileCommitState::NotAttempted
                            && matches!(error.runtime_error(),
                                Some(khive_runtime::RuntimeError::InvalidInput(message))
                                if message == "workspace mirror source URI has no unique current document");
                        let cursor_fence = error.commit_state == crate::mirror::FileCommitState::Unconfirmed
                            && matches!(error.runtime_error(), Some(khive_runtime::RuntimeError::Storage(_)))
                            && error.to_string().contains("workspace mirror cursor changed before commit");
                        assert!(pre_unit || cursor_fence, "unexpected concurrent-pass failure: {error}");
                    }
                }
            }
            assert_eq!(committed, 1, "one actual changed-file commit is required");
            mirror_assert_head_and_cursor(&runtime, &token, &uri, &key, 1, 1).await;
        }).await;
        mirror_done(NAME);
    }

    async fn mirror_competing_head_case(existing: bool) {
        let tree = MirrorTree::new();
        let relative = ".khive/notes/competing.md";
        let baseline_path = if existing {
            relative
        } else {
            ".khive/notes/baseline.md"
        };
        tree.write(baseline_path, "positive head-check baseline body");
        let (runtime, registry, token) = mirror_fixture();
        let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
        let baseline =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("positive current-head baseline before trigger");
        assert_eq!(baseline.committed, 1);
        assert_eq!(mirror_documents(&runtime, &token).await.len(), 1);
        assert_eq!(mirror_lexical_rows(&runtime).await, (1, 1));
        assert!(mirror_edges(&runtime, &token).await.is_empty());
        assert_eq!(rows(&runtime).await.len(), 1);
        tree.write(relative, "new competing-head target body");
        let (uri, key) = mirror_slot(&tree, relative, &token);
        let before_cursor = read(&runtime, &key).await;
        assert_eq!(before_cursor.is_some(), existing, "target cursor premise");
        let documents = mirror_documents(&runtime, &token).await;
        let edges = mirror_edges(&runtime, &token).await;
        let lexical = mirror_lexical_rows(&runtime).await;
        let cursors = rows(&runtime).await;
        assert_eq!(
            mirror_uri_heads(&documents, &edges, &uri).len(),
            usize::from(existing),
            "target head premise"
        );
        let escaped_uri = uri.replace('\'', "''");
        let competitor = "62350000-0000-4000-8000-000000000001";
        mirror_script(&runtime, &format!(
            "CREATE TRIGGER mirror_competing_head AFTER INSERT ON entities \
             WHEN NEW.kind='document' AND NEW.id<>'{competitor}' \
               AND json_extract(NEW.properties, '$.source_uri')='{escaped_uri}' \
             BEGIN INSERT INTO entities \
               (id,namespace,kind,entity_type,name,description,properties,tags,created_at,updated_at,deleted_at,merged_into,merge_event_id) \
               VALUES ('{competitor}',NEW.namespace,'document',NULL,'Competing mirror head',NEW.description,NEW.properties,NEW.tags,NEW.created_at,NEW.updated_at,NULL,NULL,NULL); END;"
        )).await;
        let error =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect_err("competing current head must abort before cursor commit");
        assert!(
            matches!(
                error.runtime_error(),
                Some(khive_runtime::RuntimeError::Storage(_))
            ),
            "head refusal must keep storage error type: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("workspace mirror source URI head changed before commit"),
            "wrong head-refusal stage: {error}"
        );
        assert_eq!(
            error.commit_state,
            crate::mirror::FileCommitState::Unconfirmed
        );
        assert_eq!(
            read(&runtime, &key).await,
            before_cursor,
            "head refusal must preserve existing/absent cursor"
        );
        assert_eq!(
            rows(&runtime).await,
            cursors,
            "head refusal must preserve all cursor rows"
        );
        assert_eq!(
            mirror_documents(&runtime, &token).await,
            documents,
            "head refusal must roll back both inserted documents"
        );
        assert_eq!(
            mirror_edges(&runtime, &token).await,
            edges,
            "head refusal must roll back supersedes edge"
        );
        assert_eq!(
            mirror_lexical_rows(&runtime).await,
            lexical,
            "head refusal must roll back real document FTS writes"
        );
        mirror_script(&runtime, "DROP TRIGGER mirror_competing_head;").await;
        let retry =
            crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                .await
                .expect("retry after removing competing-head trigger");
        assert_eq!(retry.committed, 1);
        mirror_assert_head_and_cursor(
            &runtime,
            &token,
            &uri,
            &key,
            usize::from(existing),
            if existing { 1 } else { 2 },
        )
        .await;
    }

    #[tokio::test]
    async fn mirror_competing_head_during_update_rolls_back_and_retries() {
        const NAME: &str = "mirror_competing_head_during_update_rolls_back_and_retries";
        if mirror_child(NAME) {
            return;
        }
        mirror_bounded(mirror_competing_head_case(true)).await;
        mirror_done(NAME);
    }

    #[tokio::test]
    async fn mirror_competing_head_during_first_version_rolls_back_and_retries() {
        const NAME: &str = "mirror_competing_head_during_first_version_rolls_back_and_retries";
        if mirror_child(NAME) {
            return;
        }
        mirror_bounded(mirror_competing_head_case(false)).await;
        mirror_done(NAME);
    }
}
