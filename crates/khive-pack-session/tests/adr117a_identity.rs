//! ADR-117a Amendment 1 acceptance tests for the session mirror's persisted identity.

use std::collections::HashMap;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_session::mirror::ingest::{mirror_file, LineTailSource};
use khive_pack_session::SessionPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use serde_json::json;
use tempfile::TempDir;

fn file_runtime(db_path: std::path::PathBuf) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
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
        packs: vec!["kg".to_string(), "session".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime")
}

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(SessionPack::new(runtime.clone()));
    builder.build().expect("registry")
}

fn apply_pack_schema(runtime: &KhiveRuntime, registry: &VerbRegistry) {
    registry
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("pack schema boot");
}

async fn rows(runtime: &KhiveRuntime, sql: &str) -> Vec<SqlRow> {
    let sql_access = runtime.sql();
    let mut reader = sql_access.reader().await.expect("SQL reader");
    reader
        .query_all(SqlStatement {
            sql: sql.to_string(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query")
}

fn text<'a>(row: &'a SqlRow, column: &str) -> &'a str {
    match row.get(column) {
        Some(SqlValue::Text(value)) => value,
        value => panic!("expected text {column}, got {value:?}"),
    }
}

fn integer(row: &SqlRow, column: &str) -> i64 {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => *value,
        value => panic!("expected integer {column}, got {value:?}"),
    }
}

include!("adr117a_identity_tests/source_and_namespace_identity_tests.rs");

include!("adr117a_identity_tests/migration_and_readiness_tests.rs");
