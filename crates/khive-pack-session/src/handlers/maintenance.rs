//! Database-wide session mirror diagnostics and explicit SQLite compaction.

use std::path::{Path, PathBuf};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use khive_storage::{SqlReader, TopLevelMaintenance};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

// Keep table names and statements fixed: no caller text becomes SQL.
const TABLE_COUNTS: [(&str, &str); 4] = [
    ("sessions", "SELECT COUNT(*) FROM sessions"),
    ("session_messages", "SELECT COUNT(*) FROM session_messages"),
    (
        "session_mirror_cursor",
        "SELECT COUNT(*) FROM session_mirror_cursor",
    ),
    (
        "session_messages_fts",
        "SELECT COUNT(*) FROM session_messages_fts",
    ),
];

// Aggregate dbstat accounts for table b-trees, indexes, and FTS5 shadow tables.
// The virtual FTS table itself has no b-tree; its shadow tables carry its bytes.
const TABLE_BYTES_SQL: &str = "SELECT d.name AS name, s.tbl_name AS owner, d.pgsize AS bytes \
    FROM dbstat AS d LEFT JOIN sqlite_schema AS s ON s.name = d.name \
    WHERE d.aggregate = TRUE AND \
      (s.tbl_name IN ('sessions', 'session_messages', 'session_mirror_cursor') \
       OR d.name GLOB 'session_messages_fts_*' \
       OR s.tbl_name GLOB 'session_messages_fts_*' \
       OR d.name = 'session_messages_fts')";

fn statement(sql: &str, label: &str) -> SqlStatement {
    SqlStatement {
        sql: sql.to_owned(),
        params: vec![],
        label: Some(label.to_owned()),
    }
}

fn nonnegative(value: i64, field: &str) -> Result<u64, RuntimeError> {
    u64::try_from(value)
        .map_err(|_| RuntimeError::Internal(format!("session maintenance: negative {field}")))
}

fn integer_column(row: &SqlRow, field: &str) -> Result<u64, RuntimeError> {
    match row.get(field) {
        Some(SqlValue::Integer(value)) => nonnegative(*value, field),
        _ => Err(RuntimeError::Internal(format!(
            "session maintenance: missing integer {field}"
        ))),
    }
}

fn text_column<'a>(row: &'a SqlRow, field: &str) -> Result<&'a str, RuntimeError> {
    match row.get(field) {
        Some(SqlValue::Text(value)) => Ok(value),
        _ => Err(RuntimeError::Internal(format!(
            "session maintenance: missing text {field}"
        ))),
    }
}

async fn scalar_u64<R: SqlReader + ?Sized>(
    reader: &mut R,
    sql: &str,
    field: &str,
) -> Result<u64, RuntimeError> {
    match reader.query_scalar(statement(sql, field)).await? {
        Some(SqlValue::Integer(value)) => nonnegative(value, field),
        _ => Err(RuntimeError::Internal(format!(
            "session maintenance: missing integer {field}"
        ))),
    }
}

async fn allocated_bytes<R: SqlReader + ?Sized>(
    reader: &mut R,
) -> Result<(u64, u64, u64), RuntimeError> {
    let page_size = scalar_u64(reader, "PRAGMA page_size", "page_size").await?;
    let page_count = scalar_u64(reader, "PRAGMA page_count", "page_count").await?;
    let bytes = page_size.checked_mul(page_count).ok_or_else(|| {
        RuntimeError::Internal("session maintenance: database byte count overflow".into())
    })?;
    Ok((page_size, page_count, bytes))
}

fn metadata_len(path: &Path, allow_missing: bool) -> Result<u64, RuntimeError> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(RuntimeError::Internal(format!(
            "session maintenance: cannot stat database file: {error}"
        ))),
    }
}

fn file_sizes(path: Option<&Path>) -> Result<(Option<u64>, Option<u64>), RuntimeError> {
    let Some(path) = path else {
        return Ok((None, None));
    };
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    Ok((
        Some(metadata_len(path, false)?),
        Some(metadata_len(&PathBuf::from(wal), true)?),
    ))
}

fn table_index(name: &str, owner: Option<&str>) -> Option<usize> {
    if name == "session_messages_fts"
        || name.starts_with("session_messages_fts_")
        || owner.is_some_and(|owner| owner.starts_with("session_messages_fts_"))
    {
        return Some(3);
    }
    match owner.unwrap_or(name) {
        "sessions" => Some(0),
        "session_messages" => Some(1),
        "session_mirror_cursor" => Some(2),
        _ => None,
    }
}

pub(crate) async fn handle_stats(
    runtime: &KhiveRuntime,
    params: Value,
) -> Result<Value, RuntimeError> {
    let _: EmptyParams = serde_json::from_value(params).map_err(|error| {
        RuntimeError::InvalidInput(format!("session.stats: invalid params: {error}"))
    })?;

    let sql = runtime.sql();
    let path = sql.database_path();
    let mut reader = sql.reader().await?;
    let mut row_counts = [0u64; 4];
    for (index, (_, count_sql)) in TABLE_COUNTS.iter().enumerate() {
        row_counts[index] = scalar_u64(reader.as_mut(), count_sql, "session_table_rows").await?;
    }

    let mut table_bytes = [0u64; 4];
    let objects = reader
        .query_all(statement(TABLE_BYTES_SQL, "session_table_bytes"))
        .await
        .map_err(|error| {
            RuntimeError::Internal(format!(
                "session.stats requires SQLite dbstat for per-table bytes: {error}"
            ))
        })?;
    for object in &objects {
        let name = text_column(object, "name")?;
        let owner = match object.get("owner") {
            Some(SqlValue::Text(value)) => Some(value.as_str()),
            Some(SqlValue::Null) | None => None,
            _ => {
                return Err(RuntimeError::Internal(
                    "session maintenance: invalid dbstat owner".into(),
                ))
            }
        };
        if let Some(index) = table_index(name, owner) {
            table_bytes[index] = table_bytes[index]
                .checked_add(integer_column(object, "bytes")?)
                .ok_or_else(|| {
                    RuntimeError::Internal("session maintenance: table byte count overflow".into())
                })?;
        }
    }
    let (page_size, page_count, database_bytes) = allocated_bytes(reader.as_mut()).await?;
    drop(reader);
    let (file_bytes, wal_bytes) = file_sizes(path.as_deref())?;

    let tables = TABLE_COUNTS
        .iter()
        .enumerate()
        .map(|(index, (name, _))| {
            (
                name.to_string(),
                json!({ "rows": row_counts[index], "bytes": table_bytes[index] }),
            )
        })
        .collect::<serde_json::Map<String, Value>>();
    Ok(json!({
        "ok": true,
        "count_scope": "database",
        "bytes_scope": "database",
        "bytes_method": "dbstat_including_indexes_and_fts_shadow_tables",
        "tables": tables,
        "page_size_bytes": page_size,
        "page_count": page_count,
        "database_bytes": database_bytes,
        "file_bytes": file_bytes,
        "wal_bytes": wal_bytes,
    }))
}

pub(crate) async fn handle_vacuum(
    runtime: &KhiveRuntime,
    params: Value,
) -> Result<Value, RuntimeError> {
    let _: EmptyParams = serde_json::from_value(params).map_err(|error| {
        RuntimeError::InvalidInput(format!("session.vacuum: invalid params: {error}"))
    })?;

    // The mirror exposes no pass-in-progress signal. The SQL writer serializes
    // this top-level operation with its writes, as in memory.vacuum.
    let sql = runtime.sql();
    let path = sql.database_path();
    let mut writer = sql.writer().await?;
    let (page_size_before, page_count_before, bytes_before) =
        allocated_bytes(writer.as_mut()).await?;
    let (file_bytes_before, wal_bytes_before) = file_sizes(path.as_deref())?;
    writer
        .execute_script_top_level(TopLevelMaintenance::Vacuum)
        .await?;
    // VACUUM has committed. A request-read deadline may have elapsed during
    // this long-running write, so a subsequent PRAGMA can time out. Preserve
    // the committed outcome even when its optional after-measurement fails.
    Ok(vacuum_result_after_commit(
        writer.as_mut(),
        path.as_deref(),
        page_size_before,
        page_count_before,
        bytes_before,
        file_bytes_before,
        wal_bytes_before,
    )
    .await)
}

async fn vacuum_result_after_commit<R: SqlReader + ?Sized>(
    writer: &mut R,
    path: Option<&Path>,
    page_size_before: u64,
    page_count_before: u64,
    bytes_before: u64,
    file_bytes_before: Option<u64>,
    wal_bytes_before: Option<u64>,
) -> Value {
    let after = async {
        let (page_size, page_count, bytes) = allocated_bytes(writer).await?;
        let (file_bytes, wal_bytes) = file_sizes(path)?;
        Ok::<_, RuntimeError>((page_size, page_count, bytes, file_bytes, wal_bytes))
    }
    .await;
    let (
        page_size_after,
        page_count_after,
        bytes_after,
        file_bytes_after,
        wal_bytes_after,
        measurement_status,
        measurement_error,
    ) = match after {
        Ok((page_size, page_count, bytes, file_bytes, wal_bytes)) => (
            Some(page_size),
            Some(page_count),
            Some(bytes),
            file_bytes,
            wal_bytes,
            "available",
            None,
        ),
        Err(error) => (
            None,
            None,
            None,
            None,
            None,
            "unavailable_after_commit",
            Some(error.to_string()),
        ),
    };

    json!({
        "ok": true,
        "post_vacuum_metrics_status": measurement_status,
        "post_vacuum_metrics_error": measurement_error,
        "database_bytes_before": bytes_before,
        "database_bytes_after": bytes_after,
        "allocated_bytes_reclaimed": bytes_after.map(|bytes| bytes_before.saturating_sub(bytes)),
        "page_size_bytes_before": page_size_before,
        "page_size_bytes_after": page_size_after,
        "page_count_before": page_count_before,
        "page_count_after": page_count_after,
        "file_bytes_before": file_bytes_before,
        "file_bytes_after": file_bytes_after,
        "wal_bytes_before": wal_bytes_before,
        "wal_bytes_after": wal_bytes_after,
    })
}

#[cfg(test)]
mod tests {
    use khive_runtime::{KhiveRuntime, RuntimeConfig};
    use serde_json::json;
    use tempfile::TempDir;

    use super::{allocated_bytes, handle_stats, handle_vacuum, vacuum_result_after_commit};
    use crate::vocab::SESSION_SCHEMA_PLAN_STMTS;

    async fn setup() -> (KhiveRuntime, TempDir) {
        let directory = TempDir::new().expect("temporary database directory");
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = Some(directory.path().join("session-maintenance.db"));
        config.packs = vec!["kg".to_owned()];
        let runtime = KhiveRuntime::new(config).expect("file-backed runtime");
        let mut writer = runtime.sql().writer().await.expect("writer");
        for statement in &SESSION_SCHEMA_PLAN_STMTS {
            writer
                .execute_script((*statement).to_owned())
                .await
                .expect("session schema");
        }
        drop(writer);
        (runtime, directory)
    }

    #[tokio::test]
    async fn stats_rows_follow_session_store_inserts() {
        let (runtime, _directory) = setup().await;
        let before = handle_stats(&runtime, json!({}))
            .await
            .expect("empty stats");
        assert_eq!(before["tables"]["sessions"]["rows"], 0);
        assert_eq!(before["tables"]["session_mirror_cursor"]["rows"], 0);

        let mut writer = runtime.sql().writer().await.expect("writer");
        // Test setup scripts use pack-owned tables and are not standalone lint queries.
        writer
            .execute_script(
                include_str!("fixtures/session_maintenance_seed.sql.fixture").to_owned(),
            )
            .await
            .expect("seed session rows");
        drop(writer);

        let after = handle_stats(&runtime, json!({}))
            .await
            .expect("populated stats");
        assert_eq!(after["count_scope"], "database");
        assert_eq!(after["tables"]["sessions"]["rows"], 1);
        assert_eq!(after["tables"]["session_messages"]["rows"], 1);
        assert_eq!(after["tables"]["session_messages_fts"]["rows"], 1);
        assert_eq!(after["tables"]["session_mirror_cursor"]["rows"], 1);
        assert!(after["tables"]["sessions"]["bytes"].as_u64().unwrap() > 0);
        assert!(
            after["tables"]["session_messages_fts"]["bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(after["file_bytes"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn vacuum_reclaims_allocated_pages_after_row_deletion() {
        let (runtime, _directory) = setup().await;
        let mut writer = runtime.sql().writer().await.expect("writer");
        writer
            .execute_script(
                include_str!("fixtures/session_maintenance_bulk.sql.fixture").to_owned(),
            )
            .await
            .expect("bulk session cursor rows");
        writer
            .execute_script(
                include_str!("fixtures/session_maintenance_delete.sql.fixture").to_owned(),
            )
            .await
            .expect("delete fixture rows");
        drop(writer);

        let result = handle_vacuum(&runtime, json!({})).await.expect("vacuum");
        assert!(
            result["page_count_after"].as_u64().unwrap()
                < result["page_count_before"].as_u64().unwrap(),
            "VACUUM must reclaim pages released by deleted session rows: {result}"
        );
        assert!(result["allocated_bytes_reclaimed"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn committed_vacuum_keeps_success_when_after_read_deadline_has_elapsed() {
        let (runtime, _directory) = setup().await;
        let sql = runtime.sql();
        let mut writer = sql.writer().await.expect("writer");
        let (page_size_before, page_count_before, bytes_before) = allocated_bytes(writer.as_mut())
            .await
            .expect("before metrics");
        writer
            .execute_script_top_level(khive_storage::TopLevelMaintenance::Vacuum)
            .await
            .expect("vacuum committed");
        let path = sql.database_path();

        let result = khive_storage::scope_request_read_deadline(std::time::Duration::ZERO, async {
            vacuum_result_after_commit(
                writer.as_mut(),
                path.as_deref(),
                page_size_before,
                page_count_before,
                bytes_before,
                None,
                None,
            )
            .await
        })
        .await;
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["post_vacuum_metrics_status"],
            "unavailable_after_commit"
        );
        assert!(result["database_bytes_after"].is_null());
        assert_eq!(result["database_bytes_before"], bytes_before);
    }
}
