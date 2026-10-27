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

#[cfg(test)]
mod tests;
