use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError};
use khive_storage::types::{SqlColumn, SqlRow, SqlValue};
use serde_json::json;
use tempfile::TempDir;

use super::{
    allocated_bytes, handle_stats, handle_vacuum, integer_column, text_column,
    vacuum_result_after_commit,
};
use crate::vocab::SESSION_SCHEMA_PLAN_STMTS;

#[test]
fn maintenance_integer_decode_preserves_nonnegative_values_and_errors() {
    for value in [0, 23, i64::MAX] {
        let row = SqlRow {
            columns: vec![SqlColumn {
                name: "bytes".into(),
                value: SqlValue::Integer(value),
            }],
        };
        assert_eq!(integer_column(&row, "bytes").unwrap(), value as u64);
    }
    for value in [
        None,
        Some(SqlValue::Null),
        Some(SqlValue::Float(23.0)),
        Some(SqlValue::Text("23".into())),
        Some(SqlValue::Blob(vec![23])),
        Some(SqlValue::Integer(-1)),
    ] {
        let expected = if matches!(value, Some(SqlValue::Integer(-1))) {
            "session maintenance: negative bytes"
        } else {
            "session maintenance: missing integer bytes"
        };
        let row = SqlRow {
            columns: value
                .map(|value| SqlColumn {
                    name: "bytes".into(),
                    value,
                })
                .into_iter()
                .collect(),
        };
        assert!(matches!(
            integer_column(&row, "bytes"),
            Err(RuntimeError::Internal(message)) if message == expected
        ));
    }
}

#[test]
fn maintenance_text_decode_preserves_text_and_domain_errors() {
    for value in ["", "session_messages_fts", "会话"] {
        let row = SqlRow {
            columns: vec![SqlColumn {
                name: "name".into(),
                value: SqlValue::Text(value.into()),
            }],
        };
        assert_eq!(text_column(&row, "name").unwrap(), value);
    }
    for value in [
        None,
        Some(SqlValue::Null),
        Some(SqlValue::Integer(23)),
        Some(SqlValue::Float(23.0)),
        Some(SqlValue::Blob(b"sessions".to_vec())),
    ] {
        let row = SqlRow {
            columns: value
                .map(|value| SqlColumn {
                    name: "name".into(),
                    value,
                })
                .into_iter()
                .collect(),
        };
        assert!(matches!(
            text_column(&row, "name"),
            Err(RuntimeError::Internal(message))
                if message == "session maintenance: missing text name"
        ));
    }
}

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
        .execute_script(include_str!("fixtures/session_maintenance_seed.sql.fixture").to_owned())
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
        .execute_script(include_str!("fixtures/session_maintenance_bulk.sql.fixture").to_owned())
        .await
        .expect("bulk session cursor rows");
    writer
        .execute_script(include_str!("fixtures/session_maintenance_delete.sql.fixture").to_owned())
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
