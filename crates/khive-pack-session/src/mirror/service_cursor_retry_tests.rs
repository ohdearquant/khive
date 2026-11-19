use super::{
    delete_cursors, drain_pending_cursor_deletes, finalize_dispatch_stats, load_cursors,
    queue_cursor_deletes, reconcile_cursor_identity, tally_dispatch_errors, CandidateDispatch,
    CursorState, DiscoveredKind, DiscoveryIndex, CURSOR_DELETE_RETRY_LIMIT,
    FILE_ERROR_POLLS_BEFORE_COLD,
};
use crate::mirror::ingest::{file_identity, mirror_file, LineTailSource, MirrorStats};
use crate::vocab::{SESSION_SCHEMA_COLUMN_ADDITIONS, SESSION_SCHEMA_PLAN_STMTS};
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::{NamedTempFile, TempDir};

/// File-backed runtime WITHOUT the session schema applied — cursor DML
/// fails with "no such table", which doubles as the fault injection for
/// the retry path. Caller keeps the `TempDir` alive.
fn runtime_without_schema() -> (KhiveRuntime, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let rt = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: Some(db_path),
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
    (rt, dir)
}

async fn apply_session_schema(rt: &KhiveRuntime) {
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    for stmt in &SESSION_SCHEMA_PLAN_STMTS {
        writer
            .execute_script(stmt.to_string())
            .await
            .expect("schema stmt");
    }
}

async fn insert_cursor_row(rt: &KhiveRuntime, path: &std::path::Path, offset: i64) {
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql:
                "INSERT INTO session_mirror_cursor(file_path, session_id, byte_offset, updated_at) \
                      VALUES(?1, NULL, ?2, 0)"
                    .into(),
            params: vec![
                SqlValue::Text(path.to_string_lossy().into_owned()),
                SqlValue::Integer(offset),
            ],
            label: None,
        })
        .await
        .expect("cursor insert");
}

async fn cursor_row_exists(rt: &KhiveRuntime, path: &std::path::Path) -> bool {
    let mut reader = rt.sql().reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path = ?1".into(),
            params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
            label: None,
        })
        .await
        .expect("cursor query");
    !rows.is_empty()
}

async fn cursor_row_offset(rt: &KhiveRuntime, path: &std::path::Path) -> Option<i64> {
    let mut reader = rt.sql().reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path = ?1".into(),
            params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
            label: None,
        })
        .await
        .expect("cursor query");
    rows.first().and_then(|row| match row.get("byte_offset") {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    })
}

include!("service_cursor_retry_tests/cursor_identity_tests.rs");

include!("service_cursor_retry_tests/persisted_cursor_migration_tests.rs");

include!("service_cursor_retry_tests/cursor_cleanup_retry_tests.rs");

include!("service_cursor_retry_tests/empty_advance_error_tests.rs");

include!("service_cursor_retry_tests/cursor_delete_batch_tests.rs");
