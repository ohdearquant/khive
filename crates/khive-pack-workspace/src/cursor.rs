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
            let invalid = tree.write(".khive/notes/invalid.md", [0xff, 0xfe, 0x80]);
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
                vec![std::fs::canonicalize(invalid).unwrap()]
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
            let invalid = tree.write(".khive/notes/invalid.md", [0xff, 0xfe, 0x80]);
            tree.write(".khive/notes/valid.md", "stable valid prose");
            let canonical_invalid = std::fs::canonicalize(invalid).unwrap();
            let (runtime, registry, token) = mirror_fixture();
            let config = crate::WorkspaceMirrorConfig::from_env().unwrap();
            let first =
                crate::mirror::mirror_directory_once(&runtime, &registry, &token, &tree.0, &config)
                    .await
                    .expect("first mixed pass");
            assert_eq!(first.committed, 1);
            assert_eq!(first.skipped_non_utf8, vec![canonical_invalid.clone()]);
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
            assert_eq!(second.skipped_non_utf8, vec![canonical_invalid]);
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
}
