use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    KhiveRuntime, PackRuntime, RuntimeConfig, RuntimeError, StorageBackend, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::Pack;
use serde_json::{json, Value};
use tempfile::NamedTempFile;

fn runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime")
}

fn unbooted_registry(runtime: &KhiveRuntime, namespace: &str) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace(namespace);
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn registry(runtime: &KhiveRuntime, namespace: &str) -> VerbRegistry {
    let registry = unbooted_registry(runtime, namespace);
    registry.apply_schema_plans(runtime.backend());
    registry
}

async fn seed_atom(registry: &VerbRegistry, slug: &str, signal: &str, finalized: bool) {
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": slug,
                    "name": format!("{slug} evaluation atom"),
                    "content": format!(
                        "{signal} retrieval evaluation fixture provides deterministic lexical evidence \
                         for ranked corpus search quality metrics while preserving realistic knowledge \
                         atom content validation requirements across repeated offline benchmark runs"
                    ),
                    "finalized": finalized
                }]
            }),
        )
        .await
        .expect("seed atom");
}

fn query_set(contents: &str) -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("query-set tempfile");
    file.write_all(contents.as_bytes())
        .expect("write query set");
    file.flush().expect("flush query set");
    file
}

fn canonical_query_set_path(file: &NamedTempFile) -> String {
    std::fs::canonicalize(file.path())
        .expect("canonical query-set path")
        .to_string_lossy()
        .into_owned()
}

async fn stats(registry: &VerbRegistry) -> Value {
    registry
        .dispatch("knowledge.stats", json!({}))
        .await
        .expect("knowledge stats")
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-12,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn knowledge_pack_declares_the_eval_table_as_pack_owned_schema() {
    let declared = <KnowledgePack as Pack>::SCHEMA_PLAN
        .expect("KnowledgePack must declare its auxiliary schema statically");
    assert_eq!(declared.pack, "knowledge");
    assert_eq!(declared.statements.len(), 2);

    let runtime_plan = KnowledgePack::new(runtime()).schema_plan();
    assert_eq!(runtime_plan.pack, declared.pack);
    assert_eq!(runtime_plan.statements, declared.statements);

    let combined = declared.statements.join(" ");
    assert!(combined.contains("CREATE TABLE IF NOT EXISTS knowledge_eval_runs"));
    assert!(combined.contains("CREATE INDEX IF NOT EXISTS idx_knowledge_eval_runs_ns_run_at"));
}

#[tokio::test]
async fn knowledge_schema_plan_boot_is_idempotent_and_enables_stats() {
    let runtime = runtime();
    let registry = unbooted_registry(&runtime, "local");

    registry.apply_schema_plans(runtime.backend());
    registry.apply_schema_plans(runtime.backend());

    let booted_stats = stats(&registry).await;
    assert_eq!(booted_stats["retrieval_eval_coverage"], 0.0);
    assert_eq!(booted_stats["retrieval_eval_run_count"], 0);
    assert!(booted_stats["retrieval_eval_last_run_at"].is_null());
    assert!(booted_stats["retrieval_eval_last_mrr"].is_null());
}

#[tokio::test]
async fn knowledge_schema_plan_routes_only_to_the_assigned_backend() {
    let default_backend = Arc::new(StorageBackend::memory().expect("default memory backend"));
    let knowledge_backend = Arc::new(StorageBackend::memory().expect("knowledge memory backend"));
    let knowledge_runtime =
        KhiveRuntime::from_backend(knowledge_backend.clone(), RuntimeConfig::no_embeddings());
    let registry = unbooted_registry(&knowledge_runtime, "local");
    let mut backend_map: HashMap<&str, &StorageBackend> = HashMap::new();
    backend_map.insert("knowledge", knowledge_backend.as_ref());

    registry
        .apply_schema_plans_with_map(&backend_map, default_backend.as_ref())
        .expect("pack schema routing must succeed");

    let schema_count = |label: &str| SqlStatement {
        sql: "SELECT COUNT(*) FROM sqlite_master \
              WHERE (type = 'table' AND name = 'knowledge_eval_runs') \
                 OR (type = 'index' AND name = 'idx_knowledge_eval_runs_ns_run_at')"
            .into(),
        params: vec![],
        label: Some(label.into()),
    };

    let knowledge_sql = knowledge_backend.sql();
    let mut knowledge_reader = knowledge_sql.reader().await.expect("knowledge reader");
    let knowledge_objects = knowledge_reader
        .query_scalar(schema_count("knowledge.schema_plan.assigned_backend"))
        .await
        .expect("inspect knowledge schema");
    assert!(matches!(knowledge_objects, Some(SqlValue::Integer(2))));

    let default_sql = default_backend.sql();
    let mut default_reader = default_sql.reader().await.expect("default reader");
    let default_objects = default_reader
        .query_scalar(schema_count("knowledge.schema_plan.default_backend"))
        .await
        .expect("inspect default schema");
    assert!(matches!(default_objects, Some(SqlValue::Integer(0))));
}

#[tokio::test]
async fn malformed_query_set_reports_path_and_parser_position_without_persisting() {
    let runtime = runtime();
    let registry = registry(&runtime, "local");
    let malformed = query_set(
        r#"
            [[queries]]
            query = "unterminated
            expected_slugs = ["alpha"]
        "#,
    );
    let path = malformed.path().display().to_string();
    let canonical_path = canonical_query_set_path(&malformed);

    let error = registry
        .dispatch(
            "knowledge.eval_retrieval",
            json!({"query_set": path.clone()}),
        )
        .await
        .expect_err("malformed TOML must fail before retrieval");
    assert!(matches!(&error, RuntimeError::InvalidInput(_)));
    let message = error.to_string();
    assert!(
        message.contains(&canonical_path),
        "error must name {canonical_path}: {message}"
    );
    assert!(
        message.contains("line"),
        "error must name a line: {message}"
    );
    assert!(
        message.contains("column"),
        "error must name a column: {message}"
    );
    assert_eq!(stats(&registry).await["retrieval_eval_run_count"], 0);
}

#[tokio::test]
async fn relative_query_set_is_rejected_before_retrieval_or_persistence() {
    let runtime = runtime();
    let registry = registry(&runtime, "local");

    let error = registry
        .dispatch(
            "knowledge.eval_retrieval",
            json!({"query_set": "tests/fixtures/eval_set.toml"}),
        )
        .await
        .expect_err("relative query-set paths must fail before evaluation");
    assert!(matches!(&error, RuntimeError::InvalidInput(_)));
    let message = error.to_string();
    assert!(message.contains("must be an absolute path"), "{message}");
    assert!(
        message.contains("tests/fixtures/eval_set.toml"),
        "{message}"
    );
    assert_eq!(stats(&registry).await["retrieval_eval_run_count"], 0);
}

#[tokio::test]
async fn eval_subhandler_persists_complete_runs_and_stats_reads_latest() {
    let runtime = runtime();
    let registry = registry(&runtime, "local");
    assert!(registry.is_subhandler_verb("knowledge.eval_retrieval"));
    assert!(!registry
        .all_verbs()
        .iter()
        .any(|handler| handler.name == "knowledge.eval_retrieval"));
    assert!(registry
        .all_handlers_with_names()
        .iter()
        .any(|(_, handler)| handler.name == "knowledge.eval_retrieval"));

    seed_atom(&registry, "alpha", "alphasignal", true).await;
    let before = stats(&registry).await;
    assert_eq!(before["eval_coverage"], 1.0);
    assert_eq!(before["retrieval_eval_coverage"], 0.0);
    assert_eq!(before["retrieval_eval_run_count"], 0);
    assert!(before["retrieval_eval_last_run_at"].is_null());
    assert!(before["retrieval_eval_last_mrr"].is_null());

    let invalid = query_set(
        r#"
            [[queries]]
            query = "alphasignal"
            expected_slugs = ["alpha"]

            [[queries]]
            query = " "
            expected_slugs = ["missing"]
        "#,
    );
    let invalid_path = invalid.path().display().to_string();
    let canonical_invalid_path = canonical_query_set_path(&invalid);
    let error = registry
        .dispatch(
            "knowledge.eval_retrieval",
            json!({"query_set": invalid_path.clone()}),
        )
        .await
        .expect_err("invalid set must fail before a run is written");
    assert!(matches!(&error, RuntimeError::InvalidInput(_)));
    let message = error.to_string();
    assert!(
        message.contains(&canonical_invalid_path),
        "error must name {canonical_invalid_path}: {message}"
    );
    assert!(
        message.contains("[1]"),
        "error must name semantic entry [1]: {message}"
    );
    assert_eq!(stats(&registry).await["retrieval_eval_run_count"], 0);

    let miss = query_set(
        r#"
            [[queries]]
            query = "alphasignal"
            expected_slugs = ["missing"]
        "#,
    );
    let miss_result = registry
        .dispatch(
            "knowledge.eval_retrieval",
            json!({"query_set": miss.path().to_string_lossy()}),
        )
        .await
        .expect("complete miss run");
    assert_eq!(miss_result["total_queries"], 1);
    assert_eq!(miss_result["precision_at_5"], 0.0);
    assert_eq!(miss_result["recall_at_5"], 0.0);
    assert_eq!(miss_result["mrr"], 0.0);

    let hit = query_set(
        r#"
            [[queries]]
            query = "alphasignal"
            expected_slugs = ["alpha"]
        "#,
    );
    let hit_path = hit
        .path()
        .parent()
        .expect("query-set parent")
        .join(".")
        .join(hit.path().file_name().expect("query-set filename"));
    let canonical_hit_path = canonical_query_set_path(&hit);
    let hit_result = registry
        .dispatch(
            "knowledge.eval_retrieval",
            json!({"query_set": hit_path.to_string_lossy()}),
        )
        .await
        .expect("complete hit run");
    assert_close(
        hit_result["precision_at_5"]
            .as_f64()
            .expect("precision number"),
        0.2,
    );
    assert_close(
        hit_result["recall_at_5"].as_f64().expect("recall number"),
        1.0,
    );
    assert_close(hit_result["mrr"].as_f64().expect("mrr number"), 1.0);

    let sql = runtime.sql();
    let mut reader = sql.reader().await.expect("eval-run reader");
    let persisted_query_set = reader
        .query_scalar(SqlStatement {
            sql: "SELECT query_set FROM knowledge_eval_runs \
                  WHERE namespace = ?1 ORDER BY run_at DESC, rowid DESC LIMIT 1"
                .into(),
            params: vec![SqlValue::Text("local".into())],
            label: Some("knowledge.eval_retrieval.persisted_query_set".into()),
        })
        .await
        .expect("read persisted query-set path");
    match persisted_query_set {
        Some(SqlValue::Text(path)) => assert_eq!(path, canonical_hit_path),
        other => panic!("expected persisted canonical query-set path, got {other:?}"),
    }

    let after = stats(&registry).await;
    assert_eq!(after["eval_coverage"], 1.0);
    assert_eq!(after["retrieval_eval_run_count"], 2);
    assert_close(
        after["retrieval_eval_coverage"]
            .as_f64()
            .expect("coverage number"),
        0.2,
    );
    assert_close(
        after["retrieval_eval_last_mrr"]
            .as_f64()
            .expect("last mrr number"),
        1.0,
    );
    assert!(after["retrieval_eval_last_run_at"].is_i64());
}

#[tokio::test]
async fn eval_includes_draft_atoms_from_an_unfinalized_corpus() {
    let runtime = runtime();
    let registry = registry(&runtime, "local");
    seed_atom(&registry, "draft-alpha", "draftalphasignal", false).await;

    let ordinary_search = registry
        .dispatch(
            "knowledge.search",
            json!({"query": "draftalphasignal", "type": "atom", "limit": 5}),
        )
        .await
        .expect("ordinary search");
    assert_eq!(ordinary_search["results"], json!([]));

    let set = query_set(
        r#"
            [[queries]]
            query = "draftalphasignal"
            expected_slugs = ["draft-alpha"]
        "#,
    );
    let result = registry
        .dispatch(
            "knowledge.eval_retrieval",
            json!({"query_set": set.path().to_string_lossy()}),
        )
        .await
        .expect("draft-inclusive eval run");

    assert_close(
        result["precision_at_5"].as_f64().expect("precision number"),
        0.2,
    );
    assert_close(result["recall_at_5"].as_f64().expect("recall number"), 1.0);
    assert_close(result["mrr"].as_f64().expect("mrr number"), 1.0);
}

#[tokio::test]
async fn eval_runs_and_stats_are_namespace_scoped() {
    let runtime = runtime();
    let local = registry(&runtime, "local");
    seed_atom(&local, "alpha", "alphasignal", true).await;
    local
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "namespace": "other",
                "atoms": [{
                    "slug": "beta",
                    "name": "beta evaluation atom",
                    "content": "betasignal retrieval evaluation fixture provides deterministic lexical evidence for ranked corpus search quality metrics while preserving realistic knowledge atom content validation requirements across repeated offline benchmark runs",
                    "finalized": true
                }]
            }),
        )
        .await
        .expect("seed other-namespace atom");

    let set = query_set(
        r#"
            [[queries]]
            query = "alphasignal"
            expected_slugs = ["alpha"]
        "#,
    );
    local
        .dispatch(
            "knowledge.eval_retrieval",
            json!({"query_set": set.path().to_string_lossy()}),
        )
        .await
        .expect("local eval run");

    let local_stats = stats(&local).await;
    let other_stats = local
        .dispatch("knowledge.stats", json!({"namespace": "other"}))
        .await
        .expect("other-namespace knowledge stats");
    assert_eq!(local_stats["namespace"], "local");
    assert_eq!(local_stats["retrieval_eval_run_count"], 1);
    assert_eq!(other_stats["namespace"], "other");
    assert_eq!(other_stats["retrieval_eval_run_count"], 0);
    assert!(other_stats["retrieval_eval_last_run_at"].is_null());
}
