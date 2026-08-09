//! Behavioral tests for the L2 code-symbol tier.
//!
//! The suite exercises both direct ingest and `code.ingest` against fresh
//! temporary databases. Scanner and extractor behavior is verified
//! black-box through stored concepts and `contains`, `depends_on`, and
//! `implements` edges. Fixture sources need only parse with `syn`; they are
//! not compiled.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use chrono::Utc;
use khive_pack_code::source_ingest::{
    run_code_ingest, CodeSourceIngestOptions, CodeSourceIngestReport,
};
use khive_pack_code::{CodeSourceIngestL2Report, CODE_INGEST_NAMESPACE};
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{Direction, SqlStatement, SqlValue};
use khive_storage::EdgeRelation;
use rusqlite::Connection;
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared harness (mirrors tests/source_ingest.rs's conventions).
// ---------------------------------------------------------------------------

fn rust_only() -> BTreeSet<&'static str> {
    ["rust"].into_iter().collect()
}

fn rt_at(db_path: &Path) -> KhiveRuntime {
    let config = RuntimeConfig {
        db_path: Some(db_path.to_path_buf()),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    };
    KhiveRuntime::new(config).expect("target runtime opens")
}

/// Explicit wire targets are pre-existing map databases. Tests that exercise
/// successful explicit routing initialize that operator-owned target first;
/// omitted `db` tests continue to exercise workspace-local creation.
fn initialize_explicit_map_db(db_path: &Path) {
    drop(rt_at(db_path));
}

/// Default (omitted `tiers`) wire behavior and explicit `[l1, l1.5]`: L1 +
/// L1.5 only, L2 disabled.
fn default_opts<'a>(path: &'a Path) -> CodeSourceIngestOptions<'a> {
    CodeSourceIngestOptions {
        path,
        languages: rust_only(),
        sweep_time: Utc::now(),
        enable_l1: true,
        enable_l1_5: true,
        enable_l2: false,
    }
}

/// `[l2]` only.
fn l2_only_opts<'a>(path: &'a Path) -> CodeSourceIngestOptions<'a> {
    CodeSourceIngestOptions {
        path,
        languages: rust_only(),
        sweep_time: Utc::now(),
        enable_l1: false,
        enable_l1_5: false,
        enable_l2: true,
    }
}

/// All three tiers.
fn all_tiers_opts<'a>(path: &'a Path) -> CodeSourceIngestOptions<'a> {
    CodeSourceIngestOptions {
        path,
        languages: rust_only(),
        sweep_time: Utc::now(),
        enable_l1: true,
        enable_l1_5: true,
        enable_l2: true,
    }
}

/// `[]` — every tier disabled.
fn no_tiers_opts<'a>(path: &'a Path) -> CodeSourceIngestOptions<'a> {
    CodeSourceIngestOptions {
        path,
        languages: rust_only(),
        sweep_time: Utc::now(),
        enable_l1: false,
        enable_l1_5: false,
        enable_l2: false,
    }
}

fn write_manifest(root: &Path, pkg: &str) {
    std::fs::create_dir_all(root.join(pkg).join("src")).unwrap();
    std::fs::write(
        root.join(pkg).join("Cargo.toml"),
        format!("[package]\nname = \"{pkg}\"\n"),
    )
    .unwrap();
}

/// A secret-shaped AWS-style token for gate tests, assembled at runtime so
/// the repository never contains the contiguous literal a secret scanner
/// would flag.
fn secret_shaped_token() -> String {
    format!("AKIA{}", "1234567890ABCDEF")
}

/// One crate ("pkg_sym") exercising every canonical symbol kind, the call
/// floor, the type-reference floor, and positive/inherent/negative impls
/// used by the scanner and storage tests.
fn write_l2_symbol_fixture(root: &Path, pkg: &str) {
    write_manifest(root, pkg);
    let lib = "\
/// Adds two integers together.
pub fn helper(a: i32, b: i32) -> i32 {
    a + b
}

/// Calls `helper` directly — an `ExprCall` whose callee is `Expr::Path`.
pub fn caller() -> i32 {
    helper(1, 2)
}

/// A struct — maps to `datatype`.
pub struct Widget {
    /// Field type reference to another datatype.
    pub inner: Payload,
}

/// A struct referenced only from `Widget::inner` — proves the type-reference
/// floor independently of the call floor.
pub struct Payload {
    pub value: i32,
}

/// An enum — maps to `datatype`.
pub enum Status {
    Ready,
    Blocked,
}

/// A union — maps to `datatype`.
pub union Raw {
    pub i: i32,
    pub f: f32,
}

/// A type alias — maps to `datatype`.
pub type WidgetAlias = Widget;

/// A trait — maps to `interface`.
pub trait Greet {
    /// Required trait method — a function with a qualified name.
    fn greet(&self) -> i32;
}

impl Widget {
    /// Inherent method — a function with a qualified name; an inherent impl
    /// contributes no `implements` edge.
    pub fn method(&self) -> i32 {
        0
    }

    /// Calls `method` on `self` — an `Expr::MethodCall`, not `Expr::Call`;
    /// the call floor must not claim this as an edge.
    pub fn calls_method(&self) -> i32 {
        self.method()
    }
}

/// Positive trait implementation — `Widget` implements `Greet`.
impl Greet for Widget {
    fn greet(&self) -> i32 {
        self.method()
    }
}

/// Negative impl — syntactically an impl relation, but not a positive
/// implementation; must not emit an `implements` edge.
impl !Send for Widget {}

/// An inline module — maps to `module` and is recursively traversed.
pub mod inner {
    /// A function nested inside an inline module.
    pub fn nested() -> i32 {
        1
    }
}
";
    std::fs::write(root.join(pkg).join("src/lib.rs"), lib).unwrap();
}

/// A crate with one syntactically invalid Rust file and one valid file, used
/// to prove that parse failures are isolated per file.
fn write_partial_parse_failure_fixture(root: &Path, pkg: &str) {
    write_manifest(root, pkg);
    std::fs::write(
        root.join(pkg).join("src/lib.rs"),
        "pub fn broken( {\n    // unbalanced delimiters: not valid Rust syntax\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(pkg).join("src")).unwrap();
    std::fs::write(
        root.join(pkg).join("src/other.rs"),
        "pub fn still_parses() -> i32 { 7 }\n",
    )
    .unwrap();
}

fn valid_lib_rs(fn_name: &str, body: i64) -> String {
    format!("pub fn {fn_name}() -> i64 {{ {body} }}\n")
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=khive-test"])
        .args(["-c", "user.email=khive-test@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git command starts");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is utf-8")
        .trim()
        .to_string()
}

async fn concepts_by_type(
    rt: &KhiveRuntime,
    source_project: &str,
    language: &str,
    entity_type: &str,
) -> Vec<(String, Value)> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT name, properties FROM entities \
                  WHERE deleted_at IS NULL AND kind='concept' AND entity_type=?1 \
                  AND json_extract(properties,'$.source_project')=?2 \
                  AND json_extract(properties,'$.language')=?3 \
                  ORDER BY name"
                .into(),
            params: vec![
                SqlValue::Text(entity_type.to_string()),
                SqlValue::Text(source_project.to_string()),
                SqlValue::Text(language.to_string()),
            ],
            label: Some("test_l2_concepts_by_type".into()),
        })
        .await
        .expect("query concepts by entity_type");
    rows.into_iter()
        .filter_map(|row| {
            let name = match row.get("name") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => return None,
            };
            let properties = match row.get("properties") {
                Some(SqlValue::Text(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
                _ => Value::Null,
            };
            Some((name, properties))
        })
        .collect()
}

async fn all_symbol_rows(rt: &KhiveRuntime, source_project: &str) -> Vec<(String, String, Value)> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT name, entity_type, properties FROM entities \
                  WHERE deleted_at IS NULL AND kind='concept' \
                  AND entity_type IN ('function','datatype','interface','module') \
                  AND json_extract(properties,'$.source_project')=?1 \
                  AND json_extract(properties,'$.language')='rust'"
                .into(),
            params: vec![SqlValue::Text(source_project.to_string())],
            label: Some("test_l2_all_symbol_rows".into()),
        })
        .await
        .expect("query all symbol-shaped rows");
    rows.into_iter()
        .filter_map(|row| {
            let name = match row.get("name") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => return None,
            };
            let entity_type = match row.get("entity_type") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => return None,
            };
            let properties = match row.get("properties") {
                Some(SqlValue::Text(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
                _ => Value::Null,
            };
            Some((name, entity_type, properties))
        })
        .collect()
}

/// `(relation, source name, target name, sorted l2_evidence, l2_derived)` for
/// every non-deleted edge — the L2 analogue of `tests/source_ingest.rs`'s
/// `edge_fingerprints`, additionally surfacing the `l2_evidence` array
/// so call-floor and type-reference-floor edges are distinguishable.
async fn l2_edge_fingerprints(
    rt: &KhiveRuntime,
) -> Vec<(String, String, String, Vec<String>, bool)> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT e.relation, s.name AS src_name, t.name AS tgt_name, \
                  e.metadata AS metadata \
                  FROM graph_edges e \
                  JOIN entities s ON s.id = e.source_id \
                  JOIN entities t ON t.id = e.target_id \
                  WHERE e.deleted_at IS NULL \
                  ORDER BY e.relation, src_name, tgt_name"
                .into(),
            params: vec![],
            label: Some("test_l2_edge_fingerprints".into()),
        })
        .await
        .expect("query edges");
    rows.into_iter()
        .map(|r| {
            let relation = match r.get("relation") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let src = match r.get("src_name") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let tgt = match r.get("tgt_name") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let metadata_str = match r.get("metadata") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let metadata: Value = serde_json::from_str(&metadata_str).unwrap_or(Value::Null);
            let mut evidence: Vec<String> = metadata
                .get("l2_evidence")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            evidence.sort();
            let l2_derived = metadata
                .get("l2_derived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (relation, src, tgt, evidence, l2_derived)
        })
        .collect()
}

async fn l2_edge_metadata(
    rt: &KhiveRuntime,
    source_project: &str,
    relation: &str,
    source_name: &str,
    target_name: &str,
) -> Value {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT e.metadata FROM graph_edges e \
                  JOIN entities s ON s.id=e.source_id \
                  JOIN entities t ON t.id=e.target_id \
                  WHERE e.deleted_at IS NULL AND e.relation=?1 \
                  AND s.name=?2 AND t.name=?3 \
                  AND json_extract(s.properties,'$.source_project')=?4"
                .into(),
            params: vec![
                SqlValue::Text(relation.to_string()),
                SqlValue::Text(source_name.to_string()),
                SqlValue::Text(target_name.to_string()),
                SqlValue::Text(source_project.to_string()),
            ],
            label: Some("test_l2_edge_metadata".into()),
        })
        .await
        .expect("query edge metadata")
        .expect("matching edge");
    match row.get("metadata") {
        Some(SqlValue::Text(value)) => serde_json::from_str(value).expect("valid edge metadata"),
        value => panic!("unexpected edge metadata value: {value:?}"),
    }
}

async fn edge_time_fingerprint(
    rt: &KhiveRuntime,
    source_project: &str,
    relation: &str,
    source_name: &str,
    target_name: &str,
) -> (String, String) {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT e.created_at, e.updated_at FROM graph_edges e \
                  JOIN entities s ON s.id=e.source_id \
                  JOIN entities t ON t.id=e.target_id \
                  WHERE e.deleted_at IS NULL AND e.relation=?1 \
                  AND s.name=?2 AND t.name=?3 \
                  AND json_extract(s.properties,'$.source_project')=?4"
                .into(),
            params: vec![
                SqlValue::Text(relation.to_string()),
                SqlValue::Text(source_name.to_string()),
                SqlValue::Text(target_name.to_string()),
                SqlValue::Text(source_project.to_string()),
            ],
            label: Some("test_l2_edge_times".into()),
        })
        .await
        .expect("query edge times")
        .expect("matching edge");
    (
        format!("{:?}", row.get("created_at")),
        format!("{:?}", row.get("updated_at")),
    )
}

async fn entity_properties_by_id(rt: &KhiveRuntime, id: Uuid) -> Value {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT properties FROM entities WHERE deleted_at IS NULL AND id=?1".into(),
            params: vec![SqlValue::Uuid(id)],
            label: Some("test_l2_entity_properties_by_id".into()),
        })
        .await
        .expect("query entity properties")
        .expect("matching entity");
    match row.get("properties") {
        Some(SqlValue::Text(value)) => serde_json::from_str(value).expect("valid properties"),
        value => panic!("unexpected properties value: {value:?}"),
    }
}

async fn edge_exists_by_id(
    rt: &KhiveRuntime,
    relation: &str,
    source_id: Uuid,
    target_id: Uuid,
) -> bool {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    reader
        .query_row(SqlStatement {
            sql: "SELECT 1 AS present FROM graph_edges WHERE deleted_at IS NULL \
                  AND relation=?1 AND source_id=?2 AND target_id=?3 LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(relation.to_string()),
                SqlValue::Uuid(source_id),
                SqlValue::Uuid(target_id),
            ],
            label: Some("test_l2_edge_exists_by_id".into()),
        })
        .await
        .expect("query edge")
        .is_some()
}

async fn entity_count(rt: &KhiveRuntime) -> i64 {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) AS n FROM entities WHERE deleted_at IS NULL".into(),
            params: vec![],
            label: Some("test_l2_entity_count".into()),
        })
        .await
        .expect("query")
        .expect("row");
    match row.get("n") {
        Some(SqlValue::Integer(n)) => *n,
        _ => -1,
    }
}

async fn edge_count(rt: &KhiveRuntime) -> i64 {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) AS n FROM graph_edges WHERE deleted_at IS NULL".into(),
            params: vec![],
            label: Some("test_l2_edge_count".into()),
        })
        .await
        .expect("query")
        .expect("row");
    match row.get("n") {
        Some(SqlValue::Integer(n)) => *n,
        _ => -1,
    }
}

async fn unresolved_dependency_kinds(rt: &KhiveRuntime) -> Vec<String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT properties FROM entities WHERE deleted_at IS NULL \
                  AND json_extract(properties,'$.unresolved_specifiers') IS NOT NULL"
                .into(),
            params: vec![],
            label: Some("test_l2_unresolved_dependency_kinds".into()),
        })
        .await
        .expect("query unresolved specifiers");
    let mut kinds = Vec::new();
    for row in rows {
        let properties = match row.get("properties") {
            Some(SqlValue::Text(value)) => {
                serde_json::from_str::<Value>(value).expect("valid json")
            }
            _ => continue,
        };
        kinds.extend(
            properties["unresolved_specifiers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry["dependency_kind"].as_str().map(str::to_string)),
        );
    }
    kinds.sort();
    kinds
}

async fn map_fingerprint(rt: &KhiveRuntime) -> (Vec<String>, Vec<String>) {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let entity_rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id, kind, entity_type, name, properties FROM entities \
                  WHERE deleted_at IS NULL ORDER BY id"
                .into(),
            params: vec![],
            label: Some("test_l2_map_entity_fingerprint".into()),
        })
        .await
        .expect("query entities");
    let mut entities = Vec::new();
    for row in entity_rows {
        let mut properties = match row.get("properties") {
            Some(SqlValue::Text(value)) => {
                serde_json::from_str::<Value>(value).expect("valid json")
            }
            _ => Value::Null,
        };
        if let Some(object) = properties.as_object_mut() {
            object.remove("last_seen_at");
            object.remove("sweep_clock");
        }
        entities.push(format!(
            "{:?}|{:?}|{:?}|{:?}|{}",
            row.get("id"),
            row.get("kind"),
            row.get("entity_type"),
            row.get("name"),
            serde_json::to_string(&properties).expect("properties serialize")
        ));
    }
    let edge_rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id, source_id, target_id, relation, metadata FROM graph_edges \
                  WHERE deleted_at IS NULL ORDER BY id"
                .into(),
            params: vec![],
            label: Some("test_l2_map_edge_fingerprint".into()),
        })
        .await
        .expect("query edges");
    let edges = edge_rows
        .into_iter()
        .map(|row| {
            format!(
                "{:?}|{:?}|{:?}|{:?}|{:?}",
                row.get("id"),
                row.get("source_id"),
                row.get("target_id"),
                row.get("relation"),
                row.get("metadata")
            )
        })
        .collect();
    (entities, edges)
}

async fn module_properties(rt: &KhiveRuntime, source_project: &str, module_path: &str) -> Value {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT properties FROM entities WHERE deleted_at IS NULL \
                  AND entity_type='module' \
                  AND name=?2 \
                  AND json_extract(properties,'$.source_project')=?1 \
                  AND json_extract(properties,'$.module_path')=?2"
                .into(),
            params: vec![
                SqlValue::Text(source_project.to_string()),
                SqlValue::Text(module_path.to_string()),
            ],
            label: Some("test_l2_module_properties".into()),
        })
        .await
        .expect("query")
        .expect("module row");
    match row.get("properties") {
        Some(SqlValue::Text(value)) => serde_json::from_str(value).expect("valid properties"),
        _ => Value::Null,
    }
}

async fn symbol_id(rt: &KhiveRuntime, source_project: &str, name: &str) -> Uuid {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities WHERE deleted_at IS NULL AND kind='concept' \
                  AND entity_type IN ('function','datatype','interface','module') AND name=?1 \
                  AND json_extract(properties,'$.source_project')=?2"
                .into(),
            params: vec![
                SqlValue::Text(name.to_string()),
                SqlValue::Text(source_project.to_string()),
            ],
            label: Some("test_l2_symbol_id".into()),
        })
        .await
        .expect("query")
        .expect("symbol row");
    match row.get("id") {
        Some(SqlValue::Uuid(id)) => *id,
        Some(SqlValue::Text(id)) => Uuid::parse_str(id).expect("valid UUID"),
        value => panic!("unexpected symbol id value: {value:?}"),
    }
}

fn registry(rt: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(khive_pack_code::CodePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn dispatch(registry: &VerbRegistry, verb: &str, args: Value) -> Result<Value, RuntimeError> {
    registry.dispatch(verb, args).await
}

// ---------------------------------------------------------------------------
// API and default boundary
// ---------------------------------------------------------------------------

/// Omitted `tiers` on the wire selects L1 + L1.5 with L2 disabled.
#[tokio::test]
async fn wire_omitted_tiers_defaults_to_l1_and_l1_5() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_default");
    let db = root.path().join("default.db");
    initialize_explicit_map_db(&db);
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let reg = registry(rt);

    let result = dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": root.path().join("pkg_default").to_string_lossy(),
            "db": db.to_string_lossy(),
            "languages": ["rust"],
        }),
    )
    .await
    .expect("default ingest succeeds");

    assert!(
        result.get("l2").is_none(),
        "omitted tiers must not emit an l2 report group: {result}"
    );

    let target = rt_at(&db);
    assert_eq!(
        concepts_by_type(&target, "pkg_default", "rust", "function")
            .await
            .len(),
        0,
        "omitted tiers (default L1+L1.5) must create zero function symbols"
    );
}

/// Explicit `null` tiers behaves identically to omitted.
#[tokio::test]
async fn wire_null_tiers_defaults_to_l1_and_l1_5() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_null");
    let db = root.path().join("null.db");
    initialize_explicit_map_db(&db);
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let reg = registry(rt);

    let result = dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": root.path().join("pkg_null").to_string_lossy(),
            "db": db.to_string_lossy(),
            "languages": ["rust"],
            "tiers": null,
        }),
    )
    .await
    .expect("null-tiers ingest succeeds");
    assert!(result.get("l2").is_none());
}

/// `tiers=[]` is valid and performs no map writes.
#[tokio::test]
async fn direct_empty_tier_selection_writes_nothing() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_empty");
    let db = root.path().join("empty.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let report = run_code_ingest(&rt, &token, no_tiers_opts(&root.path().join("pkg_empty")))
        .await
        .expect("empty-tier ingest succeeds");

    assert_eq!(report.projects_created, 0);
    assert_eq!(report.projects_updated, 0);
    assert_eq!(report.modules_created, 0);
    assert_eq!(report.modules_updated, 0);
    assert_eq!(report.edges_created, 0);
    assert_eq!(report.edges_updated, 0);
    assert!(report.l2.is_none());
    assert_eq!(
        entity_count(&rt).await,
        0,
        "no tier selected must create zero rows"
    );
}

#[tokio::test]
async fn wire_empty_tiers_write_no_map_rows() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_wire_empty");
    let db = root.path().join("wire-empty.db");
    initialize_explicit_map_db(&db);
    let reg = registry(KhiveRuntime::memory().expect("memory runtime"));

    let result = dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": root.path().join("pkg_wire_empty").to_string_lossy(),
            "db": db.to_string_lossy(),
            "languages": ["rust"],
            "tiers": [],
        }),
    )
    .await
    .expect("empty tier selection succeeds");

    assert!(result.get("symbols_created").is_none());
    assert!(result.get("symbol_parse_failures").is_none());
    let target = rt_at(&db);
    assert_eq!(entity_count(&target).await, 0);
    assert_eq!(edge_count(&target).await, 0);
}

/// Wire-level malformed `tiers` values are rejected as `InvalidInput` before
/// any database is resolved or opened. Covers a non-array scalar,
/// non-string array entries, and an unknown tier token.
#[tokio::test]
async fn wire_malformed_tiers_rejected_before_db_open() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_malformed");
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let reg = registry(rt);

    for (label, tiers_value) in [
        ("scalar string", json!("l2")),
        ("non-string entries", json!([1, 2])),
        ("unknown token", json!(["l3"])),
        ("mixed valid/unknown", json!(["l1", "l9"])),
    ] {
        let db = root
            .path()
            .join(format!("malformed-{label}.db").replace(' ', "_"));
        let result = dispatch(
            &reg,
            "code.ingest",
            json!({
                "path": root.path().join("pkg_malformed").to_string_lossy(),
                "db": db.to_string_lossy(),
                "languages": ["rust"],
                "tiers": tiers_value,
            }),
        )
        .await;
        let err = match result {
            Ok(value) => panic!("{label} must be rejected, got {value:?}"),
            Err(error) => error,
        };
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "{label} must fail as InvalidInput, got {err:?}"
        );
        assert!(
            !db.exists(),
            "{label}: tier parsing must fail before the target database is ever opened/created"
        );
    }
}

/// An explicit target is an operator claim that the map database already
/// exists and is current. A typo must not be interpreted as permission to
/// create and migrate a new SQLite file at that path.
#[tokio::test]
async fn wire_explicit_missing_db_is_rejected_without_creation() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_missing_target");
    let db = root.path().join("mistyped").join("map.db");
    let reg = registry(KhiveRuntime::memory().expect("memory runtime"));

    let error = dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": root.path().join("pkg_missing_target").to_string_lossy(),
            "db": db.to_string_lossy(),
            "languages": ["rust"],
            "tiers": [],
        }),
    )
    .await
    .expect_err("an explicit missing target must fail closed");

    assert!(
        matches!(error, RuntimeError::InvalidInput(_)),
        "target refusal must be InvalidInput, got {error:?}"
    );
    assert!(
        error.to_string().contains("existing") && error.to_string().contains("map database"),
        "the remedy must explain the explicit-target contract: {error}"
    );
    assert!(
        !db.exists() && !db.parent().expect("parent").exists(),
        "a mistyped explicit target must create neither the database nor its parent"
    );
}

/// Merely finding a SQLite-compatible file is not enough: an explicit map
/// target must already carry the current khive schema. Validation happens
/// before migrations or write-intent sidecars can mutate that file.
#[tokio::test]
async fn wire_explicit_unmigrated_db_is_rejected_byte_identically() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_unmigrated_target");
    let db = root.path().join("unmigrated.db");
    std::fs::write(&db, []).expect("create empty SQLite-compatible target");
    let before = std::fs::read(&db).expect("read target before dispatch");
    let reg = registry(KhiveRuntime::memory().expect("memory runtime"));

    let error = dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": root.path().join("pkg_unmigrated_target").to_string_lossy(),
            "db": db.to_string_lossy(),
            "languages": ["rust"],
            "tiers": [],
        }),
    )
    .await
    .expect_err("an explicit unmigrated target must fail closed");

    assert!(
        error.to_string().contains("schema") && error.to_string().contains("migrate"),
        "the refusal must identify the schema remedy: {error}"
    );
    assert_eq!(
        std::fs::read(&db).expect("read target after dispatch"),
        before,
        "target validation must not migrate or otherwise rewrite the file"
    );
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db.as_os_str().to_owned();
        sidecar.push(suffix);
        assert!(
            !std::path::PathBuf::from(sidecar).exists(),
            "target validation must not create the {suffix} sidecar"
        );
    }
}

/// A `MAX(version)` match is insufficient admission evidence: a valid SQLite
/// file may forge only the migration-head row while omitting the canonical
/// history. Explicit targets must reject that ledger read-only, without
/// creating write-intent sidecars.
#[tokio::test]
async fn wire_explicit_fabricated_migration_head_is_rejected_byte_identically() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_fabricated_ledger_target");
    let db = root.path().join("fabricated-ledger.db");
    initialize_explicit_map_db(&db);

    let conn = Connection::open(&db).expect("open initialized map");
    conn.execute(
        "DELETE FROM _schema_migrations \
         WHERE version <> (SELECT MAX(version) FROM _schema_migrations)",
        [],
    )
    .expect("retain only a forged-looking migration head");
    conn.close().expect("close fabricated-ledger connection");

    let before = std::fs::read(&db).expect("read target before dispatch");
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db.as_os_str().to_owned();
        sidecar.push(suffix);
        assert!(
            !std::path::PathBuf::from(sidecar).exists(),
            "fabricated target must start without the {suffix} sidecar"
        );
    }

    let reg = registry(KhiveRuntime::memory().expect("memory runtime"));
    let error = dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": root.path().join("pkg_fabricated_ledger_target").to_string_lossy(),
            "db": db.to_string_lossy(),
            "languages": ["rust"],
            "tiers": [],
        }),
    )
    .await
    .expect_err("a fabricated migration head must not admit an explicit target");

    assert!(
        error.to_string().contains("migration") && error.to_string().contains("ledger"),
        "the refusal must identify the untrusted migration ledger: {error}"
    );
    assert_eq!(
        std::fs::read(&db).expect("read target after dispatch"),
        before,
        "fabricated-ledger validation must not rewrite the target"
    );
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db.as_os_str().to_owned();
        sidecar.push(suffix);
        assert!(
            !std::path::PathBuf::from(sidecar).exists(),
            "fabricated-ledger validation must not create the {suffix} sidecar"
        );
    }
}

/// A complete canonical ledger remains an admissible explicit write target.
#[tokio::test]
async fn wire_explicit_canonical_current_db_remains_writable() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_canonical_current_target");
    let db = root.path().join("canonical-current.db");
    initialize_explicit_map_db(&db);
    let reg = registry(KhiveRuntime::memory().expect("memory runtime"));

    dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": root.path().join("pkg_canonical_current_target").to_string_lossy(),
            "db": db.to_string_lossy(),
            "languages": ["rust"],
            "tiers": [],
        }),
    )
    .await
    .expect("a canonical current explicit target remains writable");
}

/// The workspace-local default remains an intentional creation surface. The
/// guard applies only when the caller supplies `db` explicitly.
#[tokio::test]
async fn wire_omitted_db_still_initializes_workspace_map() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_default_target");
    let package = root.path().join("pkg_default_target");
    let expected_db = package.join(".khive").join("code-map.db");
    let reg = registry(KhiveRuntime::memory().expect("memory runtime"));

    dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": package.to_string_lossy(),
            "languages": ["rust"],
            "tiers": [],
        }),
    )
    .await
    .expect("the documented workspace-local default remains creatable");

    assert!(
        expected_db.exists(),
        "omitting db must still initialize the workspace-local map database"
    );
}

/// Duplicate tier entries dedupe to the same flags as the deduplicated set.
#[tokio::test]
async fn wire_duplicate_tiers_canonicalize() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_dup");
    let db_dup = root.path().join("dup.db");
    let db_single = root.path().join("single.db");
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let reg = registry(rt);

    for (db, tiers) in [
        (&db_dup, json!(["l2", "l2", "l2"])),
        (&db_single, json!(["l2"])),
    ] {
        initialize_explicit_map_db(db);
        dispatch(
            &reg,
            "code.ingest",
            json!({
                "path": root.path().join("pkg_dup").to_string_lossy(),
                "db": db.to_string_lossy(),
                "languages": ["rust"],
                "tiers": tiers,
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("ingest with {tiers:?} must succeed: {e}"));
    }

    let rt_dup = rt_at(&db_dup);
    let rt_single = rt_at(&db_single);
    assert_eq!(entity_count(&rt_dup).await, entity_count(&rt_single).await);
}

/// Caller order of tier tokens never changes execution order: L1 then
/// L1.5 then L2 always, regardless of `["l2","l1"]` vs `["l1","l2"]`.
#[tokio::test]
async fn wire_tier_order_is_caller_independent() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_order");
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let reg = registry(rt);

    let db_a = root.path().join("order_a.db");
    let db_b = root.path().join("order_b.db");
    for (db, tiers) in [(&db_a, json!(["l2", "l1"])), (&db_b, json!(["l1", "l2"]))] {
        initialize_explicit_map_db(db);
        dispatch(
            &reg,
            "code.ingest",
            json!({
                "path": root.path().join("pkg_order").to_string_lossy(),
                "db": db.to_string_lossy(),
                "languages": ["rust"],
                "tiers": tiers,
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("ingest with {tiers:?} must succeed: {e}"));
    }

    let rt_a = rt_at(&db_a);
    let rt_b = rt_at(&db_b);
    assert_eq!(entity_count(&rt_a).await, entity_count(&rt_b).await);
    assert_eq!(
        l2_edge_fingerprints(&rt_a).await,
        l2_edge_fingerprints(&rt_b).await
    );
}

/// The wire default (omitted `tiers`) and an explicit direct-API
/// `[l1, l1.5]` call must produce byte-identical report JSON (modulo
/// `db_path`, which is caller-path dependent) — the `l2` field is omitted
/// entirely, rather than present-and-null or present-and-zeroed. None of the
/// report's fields are timestamp-derived, so two independent `Utc::now()`
/// sweeps (one on each path) still compare equal.
#[tokio::test]
async fn default_and_explicit_l1_l1_5_are_report_byte_equivalent() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_bytes");
    let pkg = root.path().join("pkg_bytes");

    let db_wire = root.path().join("bytes_wire.db");
    initialize_explicit_map_db(&db_wire);
    let rt_wire = KhiveRuntime::memory().expect("memory runtime");
    let reg = registry(rt_wire);
    let value_wire = dispatch(
        &reg,
        "code.ingest",
        json!({
            "path": pkg.to_string_lossy(),
            "db": db_wire.to_string_lossy(),
            "languages": ["rust"],
        }),
    )
    .await
    .expect("wire default ingest succeeds");

    let db_direct = root.path().join("bytes_direct.db");
    let rt_direct = rt_at(&db_direct);
    let token_direct = rt_direct.authorize(Namespace::local()).expect("token");
    let report_direct: CodeSourceIngestReport =
        run_code_ingest(&rt_direct, &token_direct, default_opts(&pkg))
            .await
            .expect("direct explicit l1+l1.5 ingest succeeds");

    let mut value_wire = value_wire;
    let mut value_direct = serde_json::to_value(&report_direct).expect("serialize");
    // db_path is the caller-supplied path, intentionally different per call.
    value_wire["db_path"] = Value::Null;
    value_direct["db_path"] = Value::Null;
    assert_eq!(
        serde_json::to_vec(&value_wire).expect("wire report serializes"),
        serde_json::to_vec(&value_direct).expect("direct report serializes"),
        "wire default and direct explicit [l1,l1.5] report JSON bytes must match apart from db_path"
    );
    assert!(!value_wire
        .as_object()
        .expect("object")
        .contains_key("symbols_created"));
    for key in [
        "symbols_created",
        "symbols_updated",
        "symbol_dependencies_unresolved",
        "symbol_edges_stamped",
        "symbol_parse_failures",
    ] {
        assert!(
            !value_wire.as_object().expect("object").contains_key(key),
            "default report must omit additive L2 key {key}"
        );
    }
    assert_eq!(
        map_fingerprint(&rt_at(&db_wire)).await,
        map_fingerprint(&rt_direct).await,
        "wire default and direct explicit [l1,l1.5] must persist the same map rows"
    );
}

#[test]
fn default_report_serialization_preserves_the_pre_l2_key_sequence() {
    let report = CodeSourceIngestReport::default();
    assert_eq!(
        serde_json::to_string(&report).expect("default report serializes"),
        concat!(
            "{\"projects_created\":0,\"projects_updated\":0,",
            "\"modules_created\":0,\"modules_updated\":0,",
            "\"edges_created\":0,\"edges_updated\":0,",
            "\"unresolved_recorded\":0,\"unresolved_resolved\":0,",
            "\"coverage_stamps_missed\":0,",
            "\"files_dropped_without_source_path\":0,",
            "\"files_skipped_without_module_path\":0,\"fts_indexed\":0,",
            "\"languages\":[],\"warnings\":[],\"blocked_count\":0,",
            "\"blocked\":[],\"db_path\":\"\",\"source_revision\":\"\"}"
        )
    );
}

/// L2-only creates project/file-module ownership anchors, but no L1 manifest
/// dependency edges and no L1.5 import-scan facts.
#[tokio::test]
async fn l2_only_creates_no_manifest_or_import_facts() {
    let root = TempDir::new().expect("tempdir");
    // pkg_a depends on pkg_b via Cargo.toml AND `use pkg_b::helper;` — if L1
    // or L1.5 leaked into an L2-only pass, this would produce a depends_on
    // edge between the two projects.
    std::fs::create_dir_all(root.path().join("pkg_a").join("src")).unwrap();
    std::fs::create_dir_all(root.path().join("pkg_b").join("src")).unwrap();
    std::fs::write(
        root.path().join("pkg_a").join("Cargo.toml"),
        "[package]\nname = \"pkg_a\"\n\n[dependencies]\npkg_b = \"0.1\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("pkg_a").join("src/lib.rs"),
        "use pkg_b::helper;\n\npub fn call_it() -> i32 {\n    helper()\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("pkg_b").join("Cargo.toml"),
        "[package]\nname = \"pkg_b\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("pkg_b").join("src/lib.rs"),
        "pub fn helper() -> i32 { 0 }\n",
    )
    .unwrap();

    let db = root.path().join("l2_only.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(root.path()))
        .await
        .expect("l2-only ingest succeeds");

    let l2 = report.l2.as_ref().expect("l2 report");
    assert_eq!(report.projects_created, 2);
    assert_eq!(report.modules_created, 2);
    assert_eq!(report.modules_updated, 0);
    assert_eq!(
        report.unresolved_recorded, 0,
        "L1 manifest edges must not run"
    );
    assert_eq!(report.unresolved_resolved, 0);
    assert_eq!(report.coverage_stamps_missed, 0);
    assert_eq!(l2.symbols_created, 2);
    assert_eq!(l2.symbols_updated, 0);
    assert_eq!(l2.symbol_dependencies_unresolved, 3);
    assert_eq!(l2.symbol_edges_stamped, 0);
    assert_eq!(l2.symbol_parse_failures, 0);

    let pkg_a_functions = concepts_by_type(&rt, "pkg_a", "rust", "function").await;
    let pkg_b_functions = concepts_by_type(&rt, "pkg_b", "rust", "function").await;
    assert_eq!(
        pkg_a_functions
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["call_it"]
    );
    assert_eq!(
        pkg_b_functions
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["helper"]
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    for project in ["pkg_a", "pkg_b"] {
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT p.kind AS project_kind, p.name AS project_name, \
                      p.properties AS project_properties, m.kind AS module_kind, \
                      m.entity_type AS module_type, m.name AS module_name, \
                      m.properties AS module_properties, e.relation AS edge_relation, \
                      e.metadata AS edge_metadata \
                      FROM entities p \
                      JOIN graph_edges e ON e.source_id=p.id AND e.deleted_at IS NULL \
                      JOIN entities m ON m.id=e.target_id AND m.deleted_at IS NULL \
                      WHERE p.deleted_at IS NULL AND p.kind='project' AND p.name=?1 \
                      AND e.relation='contains' AND m.kind='concept' \
                      AND m.entity_type='module' AND m.name='crate' \
                      AND json_extract(m.properties,'$.source_project')=?1 \
                      AND json_extract(m.properties,'$.language')='rust' \
                      AND json_extract(m.properties,'$.module_path')='crate'"
                    .into(),
                params: vec![SqlValue::Text(project.to_string())],
                label: Some("test_l2_only_project_module_anchor".into()),
            })
            .await
            .expect("query exact L2 ownership anchor");
        assert_eq!(
            rows.len(),
            1,
            "{project} must have exactly one project -> crate ownership anchor: {rows:?}"
        );

        let row = &rows[0];
        assert!(matches!(
            row.get("project_kind"),
            Some(SqlValue::Text(value)) if value == "project"
        ));
        assert!(matches!(
            row.get("project_name"),
            Some(SqlValue::Text(value)) if value == project
        ));
        assert!(matches!(
            row.get("module_kind"),
            Some(SqlValue::Text(value)) if value == "concept"
        ));
        assert!(matches!(
            row.get("module_type"),
            Some(SqlValue::Text(value)) if value == "module"
        ));
        assert!(matches!(
            row.get("module_name"),
            Some(SqlValue::Text(value)) if value == "crate"
        ));
        assert!(matches!(
            row.get("edge_relation"),
            Some(SqlValue::Text(value)) if value == "contains"
        ));

        let project_properties = match row.get("project_properties") {
            Some(SqlValue::Text(value)) => {
                serde_json::from_str::<Value>(value).expect("valid project properties")
            }
            value => panic!("unexpected project properties: {value:?}"),
        };
        assert_eq!(project_properties["source_project"], project);
        assert!(project_properties["sweep_clock"]["rust"].is_string());
        assert!(project_properties.get("unresolved_specifiers").is_none());

        let module_properties = match row.get("module_properties") {
            Some(SqlValue::Text(value)) => {
                serde_json::from_str::<Value>(value).expect("valid module properties")
            }
            value => panic!("unexpected module properties: {value:?}"),
        };
        assert_eq!(module_properties["source_project"], project);
        assert_eq!(module_properties["language"], "rust");
        assert_eq!(module_properties["module_path"], "crate");
        assert_eq!(
            module_properties["source_path"],
            format!("{project}/src/lib.rs")
        );
        assert_eq!(module_properties["import_scan_status"], "unscanned");
        assert!(module_properties.get("import_specifier_count").is_none());
        assert!(module_properties.get("unresolved_import_count").is_none());
        assert!(module_properties.get("unresolved_specifiers").is_none());
        assert_eq!(
            module_properties["declaration_ids"]
                .as_array()
                .expect("current L2 declaration ownership")
                .len(),
            1
        );

        let edge_metadata = match row.get("edge_metadata") {
            Some(SqlValue::Text(value)) => {
                serde_json::from_str::<Value>(value).expect("valid edge metadata")
            }
            value => panic!("unexpected edge metadata: {value:?}"),
        };
        assert_eq!(edge_metadata["l2_derived"], true);
        assert_eq!(edge_metadata["language"], "rust");
        assert!(edge_metadata["last_seen_at"].is_string());
    }

    let fingerprints = l2_edge_fingerprints(&rt).await;
    assert_eq!(
        fingerprints.len(),
        4,
        "two project -> module and two module -> function ownership edges are expected"
    );
    assert!(
        fingerprints
            .iter()
            .all(|(relation, _, _, evidence, l2_derived)| {
                relation == "contains" && evidence.is_empty() && *l2_derived
            }),
        "L2-only edges must be L2 ownership facts, never manifest/import facts: {fingerprints:?}"
    );
    assert_eq!(entity_count(&rt).await, 6);
    assert_eq!(edge_count(&rt).await, 4);
}

#[cfg(unix)]
#[tokio::test]
async fn l2_walk_does_not_follow_sources_outside_the_ingest_root() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside tempdir");
    let pkg = root.path().join("pkg_symlink_boundary");
    write_manifest(root.path(), "pkg_symlink_boundary");
    std::fs::write(
        outside.path().join("outside.rs"),
        "pub fn must_not_be_ingested() {}\n",
    )
    .unwrap();
    symlink(
        outside.path().join("outside.rs"),
        pkg.join("src/outside.rs"),
    )
    .unwrap();

    let rt = rt_at(&root.path().join("symlink-boundary.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("bounded ingest succeeds");
    assert_eq!(entity_count(&rt).await, 0);
    assert_eq!(report.files_dropped_without_source_path, 1);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("outside the canonical ingest root")));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_l2_root_keeps_manifest_search_bounded_to_the_requested_tree() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("tempdir");
    let actual = root.path().join("actual_sources");
    let linked = root.path().join("linked_root");
    std::fs::create_dir_all(actual.join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"outer_must_not_govern\"\n",
    )
    .unwrap();
    std::fs::write(actual.join("src/lib.rs"), "pub fn bounded() {}\n").unwrap();
    symlink(&actual, &linked).unwrap();

    let rt = rt_at(&root.path().join("symlink-root.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&linked))
        .await
        .expect("symlink-root ingest succeeds");
    assert!(concepts_by_type(&rt, "linked_root", "rust", "function")
        .await
        .iter()
        .any(|(name, _)| name == "bounded"));
    assert!(
        concepts_by_type(&rt, "outer_must_not_govern", "rust", "function")
            .await
            .is_empty()
    );
}

/// L1 alone materializes manifest dependency edges without requiring the
/// L1.5 import scanner to run.
#[tokio::test]
async fn l1_only_resolves_manifest_dependencies() {
    let root = TempDir::new().expect("tempdir");
    write_manifest(root.path(), "pkg_l1_target");
    std::fs::create_dir_all(root.path().join("pkg_l1_source/src")).unwrap();
    std::fs::write(
        root.path().join("pkg_l1_source/Cargo.toml"),
        "[package]\nname = \"pkg_l1_source\"\n\n[dependencies]\npkg_l1_target = \"0.1\"\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("l1-only.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: false,
            enable_l2: false,
        },
    )
    .await
    .expect("L1-only ingest succeeds");

    assert!(l2_edge_fingerprints(&rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "pkg_l1_source"
            && target == "pkg_l1_target"
    ));
}

/// A re-resolve pass may replay only references owned by a selected tier.
/// Previously recorded import work must remain pending during L1-only, and
/// manifest work must remain pending during L1.5-only.
#[tokio::test]
async fn reresolve_replays_only_selected_tier_dependencies() {
    let root = TempDir::new().expect("tempdir");

    write_manifest(root.path(), "import_source");
    std::fs::write(
        root.path().join("import_source/src/lib.rs"),
        "use import_target::helper;\npub fn caller() { helper(); }\n",
    )
    .unwrap();
    let import_rt = rt_at(&root.path().join("tier-import.db"));
    let import_token = import_rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(
        &import_rt,
        &import_token,
        CodeSourceIngestOptions {
            path: &root.path().join("import_source"),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: false,
            enable_l1_5: true,
            enable_l2: false,
        },
    )
    .await
    .expect("initial L1.5 ingest succeeds");
    write_manifest(root.path(), "import_target");
    run_code_ingest(
        &import_rt,
        &import_token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: false,
            enable_l2: false,
        },
    )
    .await
    .expect("subsequent L1-only ingest succeeds");
    assert!(!l2_edge_fingerprints(&import_rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "import_source"
            && target == "import_target"
    ));
    assert!(unresolved_dependency_kinds(&import_rt)
        .await
        .iter()
        .any(|kind| kind == "import"));
    run_code_ingest(
        &import_rt,
        &import_token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: false,
            enable_l1_5: true,
            enable_l2: false,
        },
    )
    .await
    .expect("owning L1.5 pass resolves the import");
    assert!(l2_edge_fingerprints(&import_rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "import_source"
            && target == "import_target"
    ));
    assert!(!unresolved_dependency_kinds(&import_rt)
        .await
        .iter()
        .any(|kind| kind == "import"));

    write_manifest(root.path(), "manifest_source");
    std::fs::write(
        root.path().join("manifest_source/Cargo.toml"),
        "[package]\nname = \"manifest_source\"\n\n[dependencies]\nmanifest_target = \"0.1\"\n",
    )
    .unwrap();
    let manifest_rt = rt_at(&root.path().join("tier-manifest.db"));
    let manifest_token = manifest_rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(
        &manifest_rt,
        &manifest_token,
        CodeSourceIngestOptions {
            path: &root.path().join("manifest_source"),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: false,
            enable_l2: false,
        },
    )
    .await
    .expect("initial L1 ingest succeeds");
    write_manifest(root.path(), "manifest_target");
    run_code_ingest(
        &manifest_rt,
        &manifest_token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: false,
            enable_l1_5: true,
            enable_l2: false,
        },
    )
    .await
    .expect("subsequent L1.5-only ingest succeeds");
    assert!(!l2_edge_fingerprints(&manifest_rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "manifest_source"
            && target == "manifest_target"
    ));
    assert!(unresolved_dependency_kinds(&manifest_rt)
        .await
        .iter()
        .any(|kind| kind != "import"));
    run_code_ingest(
        &manifest_rt,
        &manifest_token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: false,
            enable_l2: false,
        },
    )
    .await
    .expect("owning L1 pass resolves the manifest dependency");
    assert!(l2_edge_fingerprints(&manifest_rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "manifest_source"
            && target == "manifest_target"
    ));
    assert!(!unresolved_dependency_kinds(&manifest_rt)
        .await
        .iter()
        .any(|kind| kind != "import"));
}

/// L1, L1.5, L2, and all-tier selections compose independently and their
/// effects are additive when run together.
#[tokio::test]
async fn tier_combinations_compose_independently() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_compose");
    let pkg = root.path().join("pkg_compose");

    let db_all = root.path().join("compose_all.db");
    let rt_all = rt_at(&db_all);
    let token_all = rt_all.authorize(Namespace::local()).expect("token");
    let report_all = run_code_ingest(&rt_all, &token_all, all_tiers_opts(&pkg))
        .await
        .expect("all-tier ingest succeeds");
    assert!(report_all.l2.is_some());
    assert!(report_all.l2.as_ref().unwrap().symbols_created > 0);

    let db_l2 = root.path().join("compose_l2.db");
    let rt_l2 = rt_at(&db_l2);
    let token_l2 = rt_l2.authorize(Namespace::local()).expect("token");
    let report_l2_only = run_code_ingest(&rt_l2, &token_l2, l2_only_opts(&pkg))
        .await
        .expect("l2-only ingest succeeds");
    assert_eq!(
        report_all.l2.as_ref().unwrap().symbols_created,
        report_l2_only.l2.as_ref().unwrap().symbols_created,
        "symbol creation count must be identical whether or not L1/L1.5 ran alongside L2"
    );
}

/// Python-only language selection never invokes the Rust scanner even when
/// `enable_l2` is set because L2 scanning is Rust-only.
#[tokio::test]
async fn python_only_selection_never_invokes_rust_l2() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_py");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(pkg.join("pyproject.toml"), "[project]\nname = \"pkg_py\"\n").unwrap();
    std::fs::write(pkg.join("main.py"), "def helper():\n    return 1\n").unwrap();
    // A Rust file is also present; a Python-only selection must not scan it.
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub fn should_not_be_scanned() {}\n",
    )
    .unwrap();

    let db = root.path().join("py_only.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: ["python"].into_iter().collect(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect("python-only ingest succeeds");

    let l2: CodeSourceIngestL2Report = report
        .l2
        .expect("l2 group present because enable_l2 was true");
    assert_eq!(
        l2.symbols_created, 0,
        "python-only language selection must scan zero Rust symbols even with L2 enabled"
    );
    let rust_functions = concepts_by_type(&rt, "pkg_py", "rust", "function").await;
    assert!(rust_functions.is_empty());
}

// ---------------------------------------------------------------------------
// Scanner and identity
// ---------------------------------------------------------------------------

/// function/struct/enum/union/type-alias/trait/inline-module all map to the
/// four canonical stored types; no raw Rust syntax name (`struct`, `enum`,
/// `union`, `trait`, `mod`, ...) leaks into `entity_type`.
#[tokio::test]
async fn canonical_symbol_kinds_map_to_four_entity_types() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_kinds");
    let db = root.path().join("kinds.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&root.path().join("pkg_kinds")))
        .await
        .expect("ingest succeeds");

    let rows = all_symbol_rows(&rt, "pkg_kinds").await;
    assert!(!rows.is_empty(), "fixture must produce symbol rows");
    for (_, entity_type, _) in &rows {
        assert!(
            matches!(
                entity_type.as_str(),
                "function" | "datatype" | "interface" | "module"
            ),
            "raw Rust syntax kind leaked into storage: {entity_type}"
        );
    }

    let functions: BTreeSet<_> = rows
        .iter()
        .filter(|(_, t, _)| t == "function")
        .map(|(n, _, _)| n.clone())
        .collect();
    assert!(
        functions.contains("helper"),
        "free function must map to function"
    );
    assert!(functions.contains("caller"));

    let datatypes: BTreeSet<_> = rows
        .iter()
        .filter(|(_, t, _)| t == "datatype")
        .map(|(n, _, _)| n.clone())
        .collect();
    assert!(datatypes.contains("Widget"), "struct must map to datatype");
    assert!(datatypes.contains("Status"), "enum must map to datatype");
    assert!(datatypes.contains("Raw"), "union must map to datatype");
    assert!(
        datatypes.contains("WidgetAlias"),
        "type alias must map to datatype"
    );

    let interfaces: BTreeSet<_> = rows
        .iter()
        .filter(|(_, t, _)| t == "interface")
        .map(|(n, _, _)| n.clone())
        .collect();
    assert!(interfaces.contains("Greet"), "trait must map to interface");

    let modules: BTreeSet<_> = rows
        .iter()
        .filter(|(_, t, _)| t == "module")
        .map(|(n, _, _)| n.clone())
        .collect();
    assert!(
        modules.iter().any(|n| n.contains("inner")),
        "inline module must map to module: {modules:?}"
    );

    // Trait method, inherent method, and trait-impl method are all functions
    // with qualified (not bare "greet"/"method") names — the exact qualifier
    // format is intentionally not part of the public contract, so this checks substring + count
    // rather than an exact literal.
    let qualified_method_functions = functions
        .iter()
        .filter(|n| n.contains("greet") || n.contains("method") || n.contains("calls_method"))
        .count();
    assert!(
        qualified_method_functions >= 3,
        "trait method, inherent method, and calls_method must all be present as functions: {functions:?}"
    );
}

/// Inline modules use declaration identity based on their containing module
/// and declaration name; they are not conflated with file-module anchors.
#[tokio::test]
async fn inline_module_identity_and_containment_are_declaration_based() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_inline_identity");
    write_manifest(root.path(), "pkg_inline_identity");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub mod inner { pub fn nested() {} }\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("inline-identity.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let first_sweep = Utc::now();
    let second_sweep = first_sweep + chrono::Duration::seconds(1);
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: first_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("ingest succeeds");
    let first_containment_times =
        edge_time_fingerprint(&rt, "pkg_inline_identity", "contains", "crate", "inner").await;
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: second_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("unchanged ingest succeeds");

    let actual = symbol_id(&rt, "pkg_inline_identity", "inner").await;
    let preimage = json!({
        "kind": "code-source-symbol",
        "source_project": "pkg_inline_identity",
        "language": "rust",
        "module_path": "crate",
        "name": "inner",
        "symbol_kind": "module",
    });
    let expected = Uuid::new_v5(
        &CODE_INGEST_NAMESPACE,
        &serde_json::to_vec(&preimage).expect("UUID preimage serializes"),
    );
    assert_eq!(actual, expected);
    assert_eq!(
        entity_properties_by_id(&rt, expected).await["module_path"],
        "crate"
    );

    let nested = symbol_id(&rt, "pkg_inline_identity", "nested").await;
    assert_eq!(
        entity_properties_by_id(&rt, nested).await["module_path"],
        "crate::inner"
    );
    let crate_anchor = Uuid::new_v5(
        &CODE_INGEST_NAMESPACE,
        &serde_json::to_vec(&json!({
            "kind": "code-source-symbol",
            "source_project": "pkg_inline_identity",
            "language": "rust",
            "module_path": "crate",
            "name": "crate",
            "symbol_kind": "module",
        }))
        .expect("UUID preimage serializes"),
    );
    assert!(edge_exists_by_id(&rt, "contains", crate_anchor, expected).await);
    assert!(edge_exists_by_id(&rt, "contains", expected, nested).await);
    let containment_metadata =
        l2_edge_metadata(&rt, "pkg_inline_identity", "contains", "crate", "inner").await;
    assert_eq!(
        containment_metadata["last_seen_at"],
        second_sweep.to_rfc3339()
    );
    let second_containment_times =
        edge_time_fingerprint(&rt, "pkg_inline_identity", "contains", "crate", "inner").await;
    assert_eq!(first_containment_times.0, second_containment_times.0);
    assert_ne!(first_containment_times.1, second_containment_times.1);

    let edges = l2_edge_fingerprints(&rt).await;
    assert!(edges.iter().any(|(relation, source, target, _, _)| {
        relation == "contains" && source == "crate" && target == "inner"
    }));
    assert!(edges.iter().any(|(relation, source, target, _, _)| {
        relation == "contains" && source == "inner" && target == "nested"
    }));
}

/// File-backed modules retain the target's flat project-owned scaffold;
/// module-owned containment is reserved for inline modules/declarations.
#[tokio::test]
async fn file_modules_remain_directly_project_owned() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_file_modules");
    write_manifest(root.path(), "pkg_file_modules");
    std::fs::create_dir_all(pkg.join("src/a")).unwrap();
    std::fs::write(pkg.join("src/lib.rs"), "pub mod a;\n").unwrap();
    std::fs::write(pkg.join("src/a/mod.rs"), "pub mod b;\n").unwrap();
    std::fs::write(pkg.join("src/a/b.rs"), "pub fn leaf() {}\n").unwrap();

    let rt = rt_at(&root.path().join("file-modules.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let sweep_time = Utc::now();
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("ingest succeeds");

    let edges = l2_edge_fingerprints(&rt).await;
    for child in ["crate", "a", "a::b"] {
        assert!(
            edges.iter().any(|(relation, source, target, _, _)| {
                relation == "contains" && source == "pkg_file_modules" && target == child
            }),
            "missing project->{child} containment: {edges:?}"
        );
        let metadata = l2_edge_metadata(
            &rt,
            "pkg_file_modules",
            "contains",
            "pkg_file_modules",
            child,
        )
        .await;
        assert_eq!(metadata["l2_derived"], true);
        assert_eq!(metadata["language"], "rust");
        assert_eq!(metadata["last_seen_at"], sweep_time.to_rfc3339());
    }
    assert!(!edges.iter().any(|(relation, source, target, _, _)| {
        relation == "contains"
            && matches!(source.as_str(), "crate" | "a")
            && matches!(target.as_str(), "a" | "a::b")
    }));
}

#[tokio::test]
async fn all_tier_l2_preserves_existing_file_scaffold_metadata() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_scaffold_metadata");
    write_manifest(root.path(), "pkg_scaffold_metadata");
    std::fs::write(pkg.join("src/lib.rs"), "pub fn symbol() {}\n").unwrap();

    let rt = rt_at(&root.path().join("scaffold-metadata.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, all_tiers_opts(&pkg))
        .await
        .expect("all-tier ingest succeeds");
    let metadata = l2_edge_metadata(
        &rt,
        "pkg_scaffold_metadata",
        "contains",
        "pkg_scaffold_metadata",
        "crate",
    )
    .await;
    assert_eq!(metadata, json!({}));
}

/// Type-reference evidence may bind only datatype/interface declarations,
/// never a same-named function candidate.
#[tokio::test]
async fn type_references_do_not_bind_function_symbols() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_type_target");
    write_manifest(root.path(), "pkg_type_target");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub fn Target() {}\npub struct Holder { pub value: Target }\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("type-target.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest succeeds");

    assert_eq!(
        report.l2.expect("L2 report").symbol_dependencies_unresolved,
        1
    );
    assert!(!l2_edge_fingerprints(&rt).await.iter().any(
        |(relation, source, target, evidence, _)| relation == "depends_on"
            && source == "Holder"
            && target == "Target"
            && evidence.contains(&"type_reference".to_string())
    ));
}

/// Documentation is stored verbatim (untrimmed, `LitStr::value`-decoded) and
/// is gate-checked alongside the entity name and properties.
#[tokio::test]
async fn documentation_is_verbatim_and_gate_checked() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_docs");
    let db = root.path().join("docs.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&root.path().join("pkg_docs")))
        .await
        .expect("ingest succeeds");

    let functions = concepts_by_type(&rt, "pkg_docs", "rust", "function").await;
    let helper = functions
        .iter()
        .find(|(name, _)| name == "helper")
        .expect("helper function symbol exists");

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT description FROM entities WHERE deleted_at IS NULL AND kind='concept' \
                  AND entity_type='function' AND name=?1"
                .into(),
            params: vec![SqlValue::Text("helper".to_string())],
            label: Some("test_l2_helper_description".into()),
        })
        .await
        .expect("query")
        .expect("row");
    let description = match row.get("description") {
        Some(SqlValue::Text(s)) => s.clone(),
        _ => String::new(),
    };
    assert_eq!(
        description, " Adds two integers together.",
        "doc comment must be decoded verbatim via LitStr::value (leading space from `/// `)"
    );
    let _ = helper;
}

/// `ExprCall` path calls resolve to `depends_on` with `l2_evidence:["call"]`;
/// method calls (`Expr::MethodCall`) are never claimed as call-floor edges.
#[tokio::test]
async fn call_floor_resolves_path_calls_and_omits_method_calls() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_calls");
    let db = root.path().join("calls.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&root.path().join("pkg_calls")))
        .await
        .expect("ingest succeeds");

    let fingerprints = l2_edge_fingerprints(&rt).await;
    assert!(
        fingerprints.iter().any(|(rel, src, tgt, evidence, _)| {
            rel == "depends_on" && src == "caller" && tgt == "helper" && evidence.contains(&"call".to_string())
        }),
        "caller() -> helper() ExprCall must resolve to a depends_on edge with call evidence: {fingerprints:?}"
    );
    assert!(
        !fingerprints
            .iter()
            .any(|(_, src, tgt, _, _)| src == "calls_method" && tgt == "method"),
        "self.method() is an Expr::MethodCall and must not be claimed by the call floor: {fingerprints:?}"
    );
}

/// Supported type-reference sites (field types) resolve through `depends_on`
/// with `l2_evidence` containing `"type_reference"`.
#[tokio::test]
async fn type_reference_floor_resolves_field_types() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_types");
    let db = root.path().join("types.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&root.path().join("pkg_types")))
        .await
        .expect("ingest succeeds");

    let fingerprints = l2_edge_fingerprints(&rt).await;
    assert!(
        fingerprints.iter().any(|(rel, src, tgt, evidence, _)| {
            rel == "depends_on"
                && src == "Widget"
                && tgt == "Payload"
                && evidence.contains(&"type_reference".to_string())
        }),
        "Widget.inner: Payload field type must resolve with type_reference evidence: {fingerprints:?}"
    );
}

/// Bound and supertrait paths are type references: a supertrait
/// (`trait Child: Parent`), a generic bound (`fn bounded<T: Parent>`), and a
/// `dyn` bound in a field type must each resolve to `depends_on` with
/// `type_reference` evidence (ADR-085 D3 rules 3, 5, and 6).
#[tokio::test]
async fn bound_and_supertrait_paths_are_type_references() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_bounds");
    write_manifest(root.path(), "pkg_bounds");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub trait Parent {}\n\
         pub trait Child: Parent {}\n\
         pub fn bounded<T: Parent>(_x: T) -> i32 { 1 }\n\
         pub struct Holder {\n\
             pub h: Box<dyn Parent>,\n\
         }\n",
    )
    .unwrap();

    let db = root.path().join("bounds.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest succeeds");

    let fingerprints = l2_edge_fingerprints(&rt).await;
    for (src, tgt, site) in [
        ("Child", "Parent", "supertrait"),
        ("bounded", "Parent", "generic bound"),
        ("Holder", "Parent", "dyn field bound"),
    ] {
        assert!(
            fingerprints.iter().any(|(rel, s, t, evidence, _)| {
                rel == "depends_on"
                    && s == src
                    && t == tgt
                    && evidence.contains(&"type_reference".to_string())
            }),
            "{site}: {src} -> {tgt} must resolve with type_reference evidence: {fingerprints:?}"
        );
    }
}

/// Positive `impl Trait for Type` emits a datatype -> interface `implements`
/// edge; inherent and negative impls emit none.
#[tokio::test]
async fn positive_inherent_and_negative_impl_behavior_is_exact() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_impls");
    let db = root.path().join("impls.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let pkg = root.path().join("pkg_impls");
    let first_sweep = Utc::now();
    let second_sweep = first_sweep + chrono::Duration::seconds(1);
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: first_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("ingest succeeds");
    let first_times =
        edge_time_fingerprint(&rt, "pkg_impls", "implements", "Widget", "Greet").await;
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: second_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("unchanged ingest succeeds");

    let fingerprints = l2_edge_fingerprints(&rt).await;
    let implements: Vec<_> = fingerprints
        .iter()
        .filter(|(rel, ..)| rel == "implements")
        .collect();
    assert_eq!(
        implements.len(),
        1,
        "exactly one implements edge: the positive impl Greet for Widget: {implements:?}"
    );
    assert!(implements
        .iter()
        .any(|(_, src, tgt, _, _)| src == "Widget" && tgt == "Greet"));
    // No implements edge whose source/target implies the inherent `impl
    // Widget` block or the negative `impl !Send for Widget` — the only
    // possible false-positive targets here are "Widget" (inherent) and
    // "Send" (negative, an external/unresolved trait so it cannot appear as
    // a stored target anyway); the count assertion above is the primary
    // proof.
    assert!(!implements.iter().any(|(_, _, tgt, _, _)| tgt == "Send"));
    let metadata = l2_edge_metadata(&rt, "pkg_impls", "implements", "Widget", "Greet").await;
    assert_eq!(metadata["last_seen_at"], second_sweep.to_rfc3339());
    let second_times =
        edge_time_fingerprint(&rt, "pkg_impls", "implements", "Widget", "Greet").await;
    assert_eq!(first_times.0, second_times.0);
    assert_ne!(first_times.1, second_times.1);
}

/// UUID5 identity is stable across a byte-identical re-ingest, survives a
/// content change (same name/kind/module/project/language), and a rename
/// creates a new identity while the old row remains as history.
#[tokio::test]
async fn uuid5_identity_stable_on_reingest_and_content_change_new_on_rename() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_identity");
    write_manifest(root.path(), "pkg_identity");
    std::fs::write(pkg.join("src/lib.rs"), valid_lib_rs("stable_name", 1)).unwrap();

    let db = root.path().join("identity.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("first ingest succeeds");
    let first_pass = concepts_by_type(&rt, "pkg_identity", "rust", "function").await;
    let (_, first_props) = first_pass
        .iter()
        .find(|(n, _)| n == "stable_name")
        .expect("stable_name symbol exists after first ingest");
    let first_hash = first_props["content_hash"].as_str().unwrap().to_string();

    // Re-ingest unchanged: identical entity set (byte-identical fixture).
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("second ingest succeeds");
    let second_pass = concepts_by_type(&rt, "pkg_identity", "rust", "function").await;
    assert_eq!(
        first_pass.len(),
        second_pass.len(),
        "unchanged re-ingest must not create duplicate rows for the same declaration"
    );

    // Content change (same name): identity (row count + name) is stable,
    // content_hash changes.
    std::fs::write(pkg.join("src/lib.rs"), valid_lib_rs("stable_name", 2)).unwrap();
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("content-changed ingest succeeds");
    let third_pass = concepts_by_type(&rt, "pkg_identity", "rust", "function").await;
    assert_eq!(
        third_pass.len(),
        1,
        "content change alone must not create a second row for the same name"
    );
    let (_, third_props) = &third_pass[0];
    let third_hash = third_props["content_hash"].as_str().unwrap().to_string();
    assert_ne!(
        first_hash, third_hash,
        "content_hash must change when the declaration body changes"
    );

    // Rename: a new identity appears; the old name's row remains (history).
    std::fs::write(pkg.join("src/lib.rs"), valid_lib_rs("renamed_name", 2)).unwrap();
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("rename ingest succeeds");
    let fourth_pass = concepts_by_type(&rt, "pkg_identity", "rust", "function").await;
    let names: BTreeSet<_> = fourth_pass.iter().map(|(n, _)| n.clone()).collect();
    assert!(
        names.contains("renamed_name"),
        "rename must produce a row under the new name: {names:?}"
    );
    assert!(
        names.contains("stable_name"),
        "the old name's row must remain as history, not be silently deleted: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Storage and staleness behavior
// ---------------------------------------------------------------------------

/// A gate refusal (secret-shaped documentation) quarantines only that
/// record; the rest of the sweep continues.
#[tokio::test]
async fn gate_blocks_in_docs_quarantine_only_that_record() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_gate");
    write_manifest(root.path(), "pkg_gate");
    // A secret-shaped doc comment on one declaration; a clean declaration
    // alongside it must still be ingested.
    std::fs::write(
        pkg.join("src/lib.rs"),
        format!(
            "/// {} is embedded in this doc comment.\n\
             pub fn tainted() -> i32 {{ 1 }}\n\n\
             pub fn clean() -> i32 {{ 2 }}\n",
            secret_shaped_token()
        ),
    )
    .unwrap();

    let db = root.path().join("gate.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest completes despite a gate refusal");

    let functions = concepts_by_type(&rt, "pkg_gate", "rust", "function").await;
    let names: BTreeSet<_> = functions.iter().map(|(n, _)| n.clone()).collect();
    assert!(
        names.contains("clean"),
        "the clean declaration must still be ingested"
    );
    assert!(
        !names.contains("tainted"),
        "the gate-refused declaration must not be written"
    );
    assert!(
        report.blocked_count >= 1,
        "the refusal must be recorded in the report"
    );
}

#[tokio::test]
async fn gate_refused_inline_module_suppresses_descendants_and_incident_edges() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_gate_module");
    write_manifest(root.path(), "pkg_gate_module");
    std::fs::write(
        pkg.join("src/lib.rs"),
        format!(
            "/// {} must be quarantined.\n\
             pub mod refused {{ pub fn nested_clean() {{}} }}\n\
             pub fn sibling_clean() {{}}\n",
            secret_shaped_token()
        ),
    )
    .unwrap();

    let rt = rt_at(&root.path().join("gate-module.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest completes despite refusal");
    let names: BTreeSet<String> = all_symbol_rows(&rt, "pkg_gate_module")
        .await
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();
    assert!(names.contains("sibling_clean"));
    assert!(!names.contains("refused"));
    assert!(!names.contains("nested_clean"));
    assert!(!l2_edge_fingerprints(&rt)
        .await
        .iter()
        .any(|(_, source, target, _, _)| {
            matches!(source.as_str(), "refused" | "nested_clean")
                || matches!(target.as_str(), "refused" | "nested_clean")
        }));
    assert!(report.blocked_count >= 1);
}

/// Secret-shaped names and provenance properties are quarantined without
/// preventing clean declarations in the same sweep from being indexed.
#[tokio::test]
async fn gate_checks_symbol_names_and_properties() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_gate_fields");
    write_manifest(root.path(), "pkg_gate_fields");
    std::fs::write(
        pkg.join("src/lib.rs"),
        format!(
            "pub fn {}() {{}}\npub fn clean() {{}}\n",
            secret_shaped_token()
        ),
    )
    .unwrap();
    std::fs::write(
        pkg.join(format!("src/{}.rs", secret_shaped_token())),
        "pub fn property_tainted() {}\n",
    )
    .unwrap();

    let db = root.path().join("gate-fields.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("gate refusals remain per-record");

    let functions = concepts_by_type(&rt, "pkg_gate_fields", "rust", "function").await;
    let names: BTreeSet<_> = functions.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains("clean"));
    assert!(!names.contains(secret_shaped_token().as_str()));
    assert!(!names.contains("property_tainted"));
    assert!(
        report.blocked_count >= 2,
        "both the secret-shaped name and source-path properties must be refused: {report:?}"
    );
}

/// Symbol FTS writes increment `fts_indexed`; every successful symbol
/// upsert is searchable.
#[tokio::test]
async fn symbol_upserts_increment_fts_indexed() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_fts");
    let db = root.path().join("fts.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&root.path().join("pkg_fts")))
        .await
        .expect("ingest succeeds");

    let l2 = report.l2.expect("l2 group present");
    assert!(
        report.fts_indexed >= l2.symbols_created,
        "every created symbol must have a corresponding FTS write: fts_indexed={} symbols_created={}",
        report.fts_indexed,
        l2.symbols_created
    );

    let search = registry(rt.clone())
        .dispatch("search", json!({"kind": "entity", "query": "helper"}))
        .await
        .expect("generic KG search succeeds");
    let hits = search.as_array().expect("search result array");
    assert!(
        hits.iter().any(|hit| hit["name"] == "helper"),
        "the L2 symbol must be present in the generic FTS-backed search result: {hits:?}"
    );
}

/// Repeated observations of the same dependency fold their evidence into one
/// natural edge, and re-ingest does not duplicate that edge.
#[tokio::test]
async fn dependency_evidence_folds_without_duplicate_edges() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_evidence");
    write_manifest(root.path(), "pkg_evidence");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub struct Target(pub i32);\npub fn source() -> Target { Target(1) }\n",
    )
    .unwrap();

    let db = root.path().join("evidence.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let first_sweep = Utc::now();
    let second_sweep = first_sweep + chrono::Duration::seconds(1);
    let first = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: first_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("first ingest succeeds");
    assert_eq!(first.edges_created, 4);
    assert_eq!(first.edges_updated, 1);
    assert_eq!(
        first
            .l2
            .as_ref()
            .expect("first L2 report")
            .symbol_edges_stamped,
        1,
        "call and type evidence on one natural edge count once"
    );
    let first_edge_times =
        edge_time_fingerprint(&rt, "pkg_evidence", "depends_on", "source", "Target").await;
    let second = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: second_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("second ingest succeeds");

    assert_eq!(
        second
            .l2
            .as_ref()
            .expect("second L2 report")
            .symbol_edges_stamped,
        1,
        "unchanged current edges are refreshed and counted once"
    );
    assert_eq!(second.edges_created, 0);
    assert_eq!(second.edges_updated, 4);

    let matching: Vec<_> = l2_edge_fingerprints(&rt)
        .await
        .into_iter()
        .filter(|(relation, source, target, _, _)| {
            relation == "depends_on" && source == "source" && target == "Target"
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "one natural dependency edge: {matching:?}"
    );
    assert_eq!(
        matching[0].3,
        vec!["call".to_string(), "type_reference".to_string()]
    );
    assert_eq!(
        l2_edge_metadata(&rt, "pkg_evidence", "depends_on", "source", "Target").await
            ["last_seen_at"],
        second_sweep.to_rfc3339()
    );
    let second_edge_times =
        edge_time_fingerprint(&rt, "pkg_evidence", "depends_on", "source", "Target").await;
    assert_eq!(first_edge_times.0, second_edge_times.0);
    assert_ne!(first_edge_times.1, second_edge_times.1);
}

/// Removed references leave their old edge as history, but its edge-level
/// last-seen stamp does not advance to the new project sweep.
#[tokio::test]
async fn removed_dependency_edges_are_not_current_after_reingest() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_edge_currency");
    write_manifest(root.path(), "pkg_edge_currency");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub fn helper() {}\npub fn caller() { helper(); }\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("edge-currency.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let first_sweep = Utc::now();
    let second_sweep = first_sweep + chrono::Duration::seconds(1);
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: first_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("first ingest succeeds");
    assert_eq!(
        l2_edge_metadata(&rt, "pkg_edge_currency", "depends_on", "caller", "helper").await
            ["last_seen_at"],
        first_sweep.to_rfc3339()
    );

    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub fn helper() {}\npub fn caller() {}\n",
    )
    .unwrap();
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: second_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("changed ingest succeeds");

    let metadata =
        l2_edge_metadata(&rt, "pkg_edge_currency", "depends_on", "caller", "helper").await;
    assert_eq!(metadata["last_seen_at"], first_sweep.to_rfc3339());
    assert_ne!(metadata["last_seen_at"], second_sweep.to_rfc3339());
}

/// A valid, empty Rust file stamps `declaration_ids=[]`; a complete
/// zero-declaration scan is a successful pass.
#[tokio::test]
async fn valid_empty_rust_file_stamps_empty_declaration_ids() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_empty_file");
    write_manifest(root.path(), "pkg_empty_file");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "// only a comment, zero declarations\n",
    )
    .unwrap();

    let db = root.path().join("empty_file.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest of a syntactically valid, declaration-free file succeeds");

    let l2 = report.l2.expect("l2 group present");
    assert_eq!(
        l2.symbol_parse_failures, 0,
        "a valid empty file is not a parse failure"
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT properties FROM entities WHERE deleted_at IS NULL \
                  AND entity_type='module' \
                  AND json_extract(properties,'$.source_project')=?1"
                .into(),
            params: vec![SqlValue::Text("pkg_empty_file".to_string())],
            label: Some("test_l2_empty_file_module".into()),
        })
        .await
        .expect("query")
        .expect("module row exists for the scanned file");
    let properties: Value = match row.get("properties") {
        Some(SqlValue::Text(s)) => serde_json::from_str(s).expect("valid json"),
        _ => Value::Null,
    };
    let declaration_ids = properties
        .get("declaration_ids")
        .and_then(Value::as_array)
        .expect("declaration_ids present on the module stamp");
    assert!(
        declaration_ids.is_empty(),
        "a valid empty file's module must stamp declaration_ids=[] : {declaration_ids:?}"
    );
}

/// Changed-and-invalid Rust clears the module's ownership stamp so a future
/// exporter cannot present stale declarations as current under new bytes;
/// old symbol rows remain only as untouched history and the pass continues.
#[tokio::test]
async fn changed_invalid_rust_clears_ownership_and_preserves_history() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_reparse");
    write_manifest(root.path(), "pkg_reparse");
    std::fs::write(pkg.join("src/lib.rs"), valid_lib_rs("still_here", 1)).unwrap();

    let db = root.path().join("reparse.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("first (valid) ingest succeeds");
    let before = concepts_by_type(&rt, "pkg_reparse", "rust", "function").await;
    assert!(before.iter().any(|(n, _)| n == "still_here"));

    // Break the same file's syntax.
    std::fs::write(pkg.join("src/lib.rs"), "pub fn broken( {\n").unwrap();
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest continues past a parse failure rather than aborting");
    let l2 = report.l2.expect("l2 group present");
    assert_eq!(l2.symbol_parse_failures, 1);
    assert!(
        !report.warnings.is_empty(),
        "a parse failure must append a warning"
    );

    // The old symbol row is untouched history, not deleted or updated.
    let after = concepts_by_type(&rt, "pkg_reparse", "rust", "function").await;
    assert!(
        after.iter().any(|(n, _)| n == "still_here"),
        "the old declaration row must remain as history after a parse failure"
    );

    // The module's ownership stamp must not carry a current declaration_ids
    // array for the failed file — no exporter could safely trust it now.
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT properties FROM entities WHERE deleted_at IS NULL \
                  AND entity_type='module' \
                  AND json_extract(properties,'$.source_project')=?1"
                .into(),
            params: vec![SqlValue::Text("pkg_reparse".to_string())],
            label: Some("test_l2_reparse_module".into()),
        })
        .await
        .expect("query")
        .expect("module row exists");
    let properties: Value = match row.get("properties") {
        Some(SqlValue::Text(s)) => serde_json::from_str(s).expect("valid json"),
        _ => Value::Null,
    };
    assert!(
        properties.get("declaration_ids").is_none()
            || properties["declaration_ids"].as_array().is_none(),
        "a failed reparse must leave no current declaration_ids ownership stamp: {properties}"
    );
}

/// A parse failure in one file does not abort the sweep; sibling files in
/// the same project still ingest successfully.
#[tokio::test]
async fn parse_failure_in_one_file_does_not_abort_the_sweep() {
    let root = TempDir::new().expect("tempdir");
    write_partial_parse_failure_fixture(root.path(), "pkg_partial");
    let db = root.path().join("partial.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&root.path().join("pkg_partial")))
        .await
        .expect("ingest completes despite one broken file");

    let l2 = report.l2.expect("l2 group present");
    assert_eq!(
        l2.symbol_parse_failures, 1,
        "exactly the one broken file fails to parse"
    );

    let functions = concepts_by_type(&rt, "pkg_partial", "rust", "function").await;
    assert!(
        functions.iter().any(|(n, _)| n == "still_parses"),
        "the syntactically valid sibling file must still be ingested: {functions:?}"
    );
}

#[tokio::test]
async fn resolved_forward_reference_reports_zero_unresolved() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_forward");
    write_manifest(root.path(), "pkg_forward");
    std::fs::write(
        pkg.join("src/a.rs"),
        "pub fn caller() { crate::z::target(); }\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/z.rs"), "pub fn target() {}\n").unwrap();

    let rt = rt_at(&root.path().join("forward.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest succeeds");

    let l2 = report.l2.expect("l2 report");
    assert_eq!(l2.symbol_dependencies_unresolved, 0);
    assert!(l2_edge_fingerprints(&rt).await.iter().any(
        |(relation, source, target, evidence, _)| relation == "depends_on"
            && source == "caller"
            && target == "target"
            && evidence.contains(&"call".to_string())
    ));
}

#[tokio::test]
async fn l1_5_preserves_pending_l2_impls() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_pending_impl");
    write_manifest(root.path(), "pkg_pending_impl");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "impl MissingTrait for MissingType {}\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("pending-impl.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("L2 ingest succeeds");
    let before = module_properties(&rt, "pkg_pending_impl", "crate").await;
    assert!(before
        .get("l2_pending_impls")
        .and_then(Value::as_array)
        .is_some_and(|pending| pending.len() == 1));

    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: false,
            enable_l1_5: true,
            enable_l2: false,
        },
    )
    .await
    .expect("L1.5 ingest succeeds");

    let after = module_properties(&rt, "pkg_pending_impl", "crate").await;
    assert_eq!(
        after.get("l2_pending_impls"),
        before.get("l2_pending_impls")
    );
}

#[tokio::test]
async fn default_ingest_preserves_l2_ownership_stamps() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_preserve_ownership");
    write_manifest(root.path(), "pkg_preserve_ownership");
    std::fs::write(pkg.join("src/lib.rs"), "pub fn owned() {}\n").unwrap();

    let rt = rt_at(&root.path().join("preserve-ownership.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("L2 ingest succeeds");
    let before = module_properties(&rt, "pkg_preserve_ownership", "crate").await;
    let declaration_ids = before
        .get("declaration_ids")
        .and_then(Value::as_array)
        .expect("L2 ownership stamp")
        .clone();
    assert!(!declaration_ids.is_empty());

    run_code_ingest(&rt, &token, default_opts(&pkg))
        .await
        .expect("default L1+L1.5 ingest succeeds");
    let after = module_properties(&rt, "pkg_preserve_ownership", "crate").await;
    assert_eq!(
        after.get("declaration_ids"),
        Some(&Value::Array(declaration_ids))
    );
}

#[tokio::test]
async fn all_tier_reingest_reparses_changed_source_before_refreshing_symbols() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_all_tier_change");
    write_manifest(root.path(), "pkg_all_tier_change");
    std::fs::write(pkg.join("src/lib.rs"), "pub fn before() {}\n").unwrap();

    let rt = rt_at(&root.path().join("all-tier-change.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, all_tiers_opts(&pkg))
        .await
        .expect("first all-tier ingest succeeds");

    std::fs::write(pkg.join("src/lib.rs"), "pub fn after() {}\n").unwrap();
    run_code_ingest(&rt, &token, all_tiers_opts(&pkg))
        .await
        .expect("changed all-tier ingest succeeds");

    let module = module_properties(&rt, "pkg_all_tier_change", "crate").await;
    let declaration_ids = module["declaration_ids"]
        .as_array()
        .expect("current ownership array");
    assert_eq!(declaration_ids.len(), 1);
    let current_id = Uuid::parse_str(
        declaration_ids[0]
            .as_str()
            .expect("declaration UUID string"),
    )
    .expect("valid declaration UUID");
    let current = rt
        .get_entity(&token, current_id)
        .await
        .expect("current declaration exists");
    assert_eq!(current.name, "after");
}

#[tokio::test]
async fn unchanged_file_refreshes_symbol_revision_and_timestamp() {
    let root = TempDir::new().expect("tempdir");
    write_manifest(root.path(), "pkg_revision");
    let pkg = root.path().join("pkg_revision");
    std::fs::write(pkg.join("src/lib.rs"), "pub fn stable() {}\n").unwrap();
    git(&pkg, &["init", "-q"]);
    git(&pkg, &["add", "."]);
    git(&pkg, &["commit", "-q", "-m", "initial"]);

    let rt = rt_at(&root.path().join("revision.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let first_sweep = Utc::now();
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: first_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("first ingest succeeds");

    git(&pkg, &["commit", "-q", "--allow-empty", "-m", "next"]);
    let second_revision = git(&pkg, &["rev-parse", "HEAD"]);
    let second_sweep = first_sweep + chrono::Duration::seconds(1);
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: second_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("second ingest succeeds");

    let module = module_properties(&rt, "pkg_revision", "crate").await;
    let symbols = concepts_by_type(&rt, "pkg_revision", "rust", "function").await;
    let symbol = &symbols
        .iter()
        .find(|(name, _)| name == "stable")
        .expect("stable symbol")
        .1;
    assert_eq!(module["source_revision"], second_revision);
    assert_eq!(symbol["source_revision"], second_revision);
    assert_eq!(symbol["last_seen_at"], second_sweep.to_rfc3339());
}

#[tokio::test]
async fn historical_symbol_rows_do_not_resolve_current_references() {
    let root = TempDir::new().expect("tempdir");
    write_manifest(root.path(), "pkg_history");
    let pkg = root.path().join("pkg_history");
    std::fs::write(pkg.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();

    let rt = rt_at(&root.path().join("history.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("first ingest succeeds");

    std::fs::write(pkg.join("src/lib.rs"), "pub fn caller() { helper(); }\n").unwrap();
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("second ingest succeeds");

    assert_eq!(
        report.l2.expect("l2 report").symbol_dependencies_unresolved,
        1
    );
    assert!(!l2_edge_fingerprints(&rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "caller"
            && target == "helper"
    ));
}

#[tokio::test]
async fn earlier_caller_does_not_refresh_edge_to_later_parse_failed_target() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_ordered_failure");
    write_manifest(root.path(), "pkg_ordered_failure");
    std::fs::write(
        pkg.join("src/a.rs"),
        "pub fn caller() { crate::z::target(); }\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/z.rs"), "pub fn target() {}\n").unwrap();

    let rt = rt_at(&root.path().join("ordered-failure.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let first_sweep = Utc::now();
    let second_sweep = first_sweep + chrono::Duration::seconds(1);
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: first_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("first ingest succeeds");
    assert_eq!(
        l2_edge_metadata(&rt, "pkg_ordered_failure", "depends_on", "caller", "target").await
            ["last_seen_at"],
        first_sweep.to_rfc3339()
    );

    std::fs::write(
        pkg.join("src/a.rs"),
        "pub fn caller() { crate::z::target(); } // changed\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/z.rs"), "pub fn target( {\n").unwrap();
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: rust_only(),
            sweep_time: second_sweep,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("partial parse failure is isolated");

    assert!(module_properties(&rt, "pkg_ordered_failure", "z")
        .await
        .get("declaration_ids")
        .is_none());
    let metadata =
        l2_edge_metadata(&rt, "pkg_ordered_failure", "depends_on", "caller", "target").await;
    assert_eq!(metadata["last_seen_at"], first_sweep.to_rfc3339());
    assert_ne!(metadata["last_seen_at"], second_sweep.to_rfc3339());
}

#[tokio::test]
async fn historical_pending_source_cannot_create_a_current_edge() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_historical_pending");
    write_manifest(root.path(), "pkg_historical_pending");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub fn historical() { crate::target::arrives_later(); }\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("historical-pending.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("first ingest succeeds");

    std::fs::write(pkg.join("src/lib.rs"), "pub fn replacement() {}\n").unwrap();
    std::fs::write(pkg.join("src/target.rs"), "pub fn arrives_later() {}\n").unwrap();
    let report = run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("second ingest succeeds");

    assert!(!l2_edge_fingerprints(&rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "historical"
            && target == "arrives_later"
    ));
    assert_eq!(
        report.l2.expect("L2 report").symbol_dependencies_unresolved,
        0,
        "historical pending work is outside the current sweep"
    );
}

#[tokio::test]
async fn qualified_external_call_does_not_bind_same_module_symbol() {
    let root = TempDir::new().expect("tempdir");
    write_manifest(root.path(), "pkg_qualified");
    let pkg = root.path().join("pkg_qualified");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub fn helper() {}\npub fn caller() { external::helper(); }\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("qualified.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest succeeds");

    assert!(!l2_edge_fingerprints(&rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "caller"
            && target == "helper"
    ));
}

#[tokio::test]
async fn rust_reference_qualifiers_and_self_loops_follow_the_syntax_floor() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_qualifiers");
    write_manifest(root.path(), "pkg_qualifiers");
    std::fs::write(
        pkg.join("src/lib.rs"),
        r#"
pub fn helper() {}
pub fn recurse() { recurse(); }
pub struct Node { pub next: Node }
pub mod inner {
    pub fn inner_helper() {}
    pub fn via_self() { self::inner_helper(); }
    pub fn via_super() { super::helper(); }
    pub fn underflow() { super::super::helper(); }
    pub mod deep {
        pub fn via_two_super() { super::super::helper(); }
    }
}
pub mod external { pub fn hidden() {} }
pub fn unknown_qualifier() { external::hidden(); }
"#,
    )
    .unwrap();

    let rt = rt_at(&root.path().join("qualifiers.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest succeeds");
    let edges = l2_edge_fingerprints(&rt).await;
    for (source, target) in [
        ("recurse", "recurse"),
        ("via_self", "inner_helper"),
        ("via_super", "helper"),
        ("via_two_super", "helper"),
    ] {
        assert!(
            edges
                .iter()
                .any(|(relation, actual_source, actual_target, _, _)| {
                    relation == "depends_on" && actual_source == source && actual_target == target
                }),
            "missing {source}->{target}: {edges:?}"
        );
    }
    assert!(!edges.iter().any(|(relation, source, target, _, _)| {
        relation == "depends_on"
            && ((source == "underflow" && target == "helper")
                || (source == "unknown_qualifier" && target == "hidden")
                || (source == "Node" && target == "Node"))
    }));
    let node = symbol_id(&rt, "pkg_qualifiers", "Node").await;
    assert!(entity_properties_by_id(&rt, node)
        .await
        .get("l2_unresolved_references")
        .is_none());
}

/// Stored symbol dependencies support bounded reverse blast-radius, cycle,
/// and dead-symbol queries through the ordinary graph API.
#[tokio::test]
async fn symbol_dependencies_are_queryable_with_generic_graph_operations() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_graph_queries");
    write_manifest(root.path(), "pkg_graph_queries");
    std::fs::write(
        pkg.join("src/lib.rs"),
        "pub fn a() { b(); }\npub fn b() { a(); }\npub fn leaf() {}\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("graph-queries.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(&rt, &token, l2_only_opts(&pkg))
        .await
        .expect("ingest succeeds");

    let a = symbol_id(&rt, "pkg_graph_queries", "a").await;
    let b = symbol_id(&rt, "pkg_graph_queries", "b").await;
    let leaf = symbol_id(&rt, "pkg_graph_queries", "leaf").await;
    let relations = Some(vec![EdgeRelation::DependsOn]);

    let reverse_blast = rt
        .neighbors(&token, b, Direction::In, Some(16), relations.clone())
        .await
        .expect("reverse neighbor query");
    assert!(reverse_blast.iter().any(|hit| hit.node_id == a));

    let a_out = rt
        .neighbors(&token, a, Direction::Out, Some(16), relations.clone())
        .await
        .expect("a outgoing query");
    let b_out = rt
        .neighbors(&token, b, Direction::Out, Some(16), relations.clone())
        .await
        .expect("b outgoing query");
    assert!(a_out.iter().any(|hit| hit.node_id == b));
    assert!(b_out.iter().any(|hit| hit.node_id == a));

    let dead = rt
        .neighbors(&token, leaf, Direction::Both, Some(16), relations)
        .await
        .expect("dead-symbol neighbor query");
    assert!(
        dead.is_empty(),
        "leaf must have no dependency incidents: {dead:?}"
    );
}

/// Two related projects converge to the identical final edge set regardless
/// of ingest order (ADR-085 B8 property 2, extended to L2 evidence) — the
/// same property `tests/source_ingest.rs` already proves for L1/L1.5.
#[tokio::test]
async fn l2_two_project_convergence_is_ingest_order_independent() {
    let root = TempDir::new().expect("tempdir");
    write_l2_symbol_fixture(root.path(), "pkg_conv_a");
    write_l2_symbol_fixture(root.path(), "pkg_conv_b");

    let db1 = root.path().join("conv_order1.db");
    let rt1 = rt_at(&db1);
    let token1 = rt1.authorize(Namespace::local()).expect("token");
    for pkg in ["pkg_conv_a", "pkg_conv_b"] {
        run_code_ingest(&rt1, &token1, l2_only_opts(&root.path().join(pkg)))
            .await
            .unwrap_or_else(|e| panic!("ingest {pkg} (order 1) must succeed: {e}"));
    }

    let db2 = root.path().join("conv_order2.db");
    let rt2 = rt_at(&db2);
    let token2 = rt2.authorize(Namespace::local()).expect("token");
    for pkg in ["pkg_conv_b", "pkg_conv_a"] {
        run_code_ingest(&rt2, &token2, l2_only_opts(&root.path().join(pkg)))
            .await
            .unwrap_or_else(|e| panic!("ingest {pkg} (order 2) must succeed: {e}"));
    }

    let fp1 = l2_edge_fingerprints(&rt1).await;
    let fp2 = l2_edge_fingerprints(&rt2).await;
    assert!(!fp1.is_empty());
    assert_eq!(
        fp1, fp2,
        "L2 edges must converge to the identical set regardless of ingest order"
    );
}

/// All L2 symbol edges stay within one `source_project`/`language`.
/// Separate temporary databases also prove that identity and resolution do
/// not leak through ambient process state.
#[tokio::test]
async fn cross_project_same_named_symbols_remain_isolated() {
    let root_a = TempDir::new().expect("tempdir a");
    let root_b = TempDir::new().expect("tempdir b");
    write_manifest(root_a.path(), "pkg_same_name");
    write_manifest(root_b.path(), "pkg_same_name");
    std::fs::write(
        root_a.path().join("pkg_same_name/src/lib.rs"),
        "pub fn helper() -> i32 { 1 }\npub fn caller() -> i32 { helper() }\n",
    )
    .unwrap();
    std::fs::write(
        root_b.path().join("pkg_same_name/src/lib.rs"),
        "pub fn helper() -> i32 { 2 }\npub fn caller() -> i32 { helper() }\n",
    )
    .unwrap();

    let db_a = root_a.path().join("iso_a.db");
    let db_b = root_b.path().join("iso_b.db");
    let rt_a = rt_at(&db_a);
    let rt_b = rt_at(&db_b);
    let token_a = rt_a.authorize(Namespace::local()).expect("token");
    let token_b = rt_b.authorize(Namespace::local()).expect("token");

    run_code_ingest(
        &rt_a,
        &token_a,
        l2_only_opts(&root_a.path().join("pkg_same_name")),
    )
    .await
    .expect("project a ingest succeeds");
    run_code_ingest(
        &rt_b,
        &token_b,
        l2_only_opts(&root_b.path().join("pkg_same_name")),
    )
    .await
    .expect("project b ingest succeeds");

    let helpers_a = concepts_by_type(&rt_a, "pkg_same_name", "rust", "function").await;
    let helpers_b = concepts_by_type(&rt_b, "pkg_same_name", "rust", "function").await;
    let id_a = helpers_a
        .iter()
        .find(|(n, _)| n == "helper")
        .map(|(_, p)| p.clone());
    let id_b = helpers_b
        .iter()
        .find(|(n, _)| n == "helper")
        .map(|(_, p)| p.clone());
    assert!(id_a.is_some() && id_b.is_some());

    // Different databases entirely — the strongest form of isolation. Both
    // must independently resolve their own local call edge.
    let fp_a = l2_edge_fingerprints(&rt_a).await;
    let fp_b = l2_edge_fingerprints(&rt_b).await;
    assert!(fp_a.iter().any(|(rel, src, tgt, ev, _)| rel == "depends_on"
        && src == "caller"
        && tgt == "helper"
        && ev.contains(&"call".to_string())));
    assert!(fp_b.iter().any(|(rel, src, tgt, ev, _)| rel == "depends_on"
        && src == "caller"
        && tgt == "helper"
        && ev.contains(&"call".to_string())));
}

/// A shared database may contain multiple projects, but a qualified call in
/// one project never resolves to a declaration owned by another project.
#[tokio::test]
async fn cross_project_references_remain_unresolved_in_one_database() {
    let root = TempDir::new().expect("tempdir");
    write_manifest(root.path(), "pkg_a_shared");
    write_manifest(root.path(), "pkg_b_shared");
    std::fs::write(
        root.path().join("pkg_a_shared/src/lib.rs"),
        "pub fn caller() { pkg_b_shared::helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("pkg_b_shared/src/lib.rs"),
        "pub fn helper() {}\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("shared-projects.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(&rt, &token, l2_only_opts(root.path()))
        .await
        .expect("multi-project ingest succeeds");

    assert!(report
        .l2
        .as_ref()
        .is_some_and(|l2| l2.symbol_dependencies_unresolved >= 1));
    assert!(!l2_edge_fingerprints(&rt).await.iter().any(
        |(relation, source, target, _, _)| relation == "depends_on"
            && source == "caller"
            && target == "helper"
    ));
}

#[tokio::test]
async fn default_polyglot_ingest_preserves_legacy_project_cache_behavior() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_default_polyglot");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("Cargo.toml"),
        "[package]\nname = \"pkg_default_polyglot\"\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/lib.rs"), "pub fn rust_item() {}\n").unwrap();
    std::fs::write(pkg.join("helper.py"), "def python_item():\n    return 1\n").unwrap();

    let rt = rt_at(&root.path().join("default-polyglot.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: ["rust", "python"].into_iter().collect(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: false,
        },
    )
    .await
    .expect("default polyglot ingest succeeds");
    assert_eq!(report.projects_created, 1);
    assert_eq!(report.projects_updated, 0);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT properties FROM entities WHERE deleted_at IS NULL \
                  AND kind='project' AND name='pkg_default_polyglot'"
                .into(),
            params: vec![],
            label: Some("test_default_polyglot_project".into()),
        })
        .await
        .expect("query project");
    assert_eq!(rows.len(), 1);
    let properties = match rows[0].get("properties") {
        Some(SqlValue::Text(value)) => serde_json::from_str::<Value>(value).expect("valid json"),
        value => panic!("unexpected properties value: {value:?}"),
    };
    let sweep_clock = properties["sweep_clock"].as_object().expect("sweep clock");
    assert!(sweep_clock.contains_key("rust"));
    assert!(!sweep_clock.contains_key("python"));
}

/// Same-named declarations across languages stay disjoint because the L2
/// symbol identity includes `language`, following the same pattern as the
/// existing L1.5 module identity. L2 itself is Rust-only, so this exercises the
/// shared-database case: an L2 Rust symbol and an L1.5 Python module sharing
/// a name/project must not collide, and each language's sweep clock is
/// independent.
#[tokio::test]
async fn cross_language_same_name_entities_remain_disjoint() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("pkg_polyglot");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("Cargo.toml"),
        "[package]\nname = \"pkg_polyglot\"\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/lib.rs"), "pub fn helper() -> i32 { 1 }\n").unwrap();
    std::fs::write(pkg.join("helper.py"), "def helper():\n    return 1\n").unwrap();

    let db = root.path().join("polyglot.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &pkg,
            languages: ["rust", "python"].into_iter().collect(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect("polyglot ingest succeeds");

    let rust_functions = concepts_by_type(&rt, "pkg_polyglot", "rust", "function").await;
    assert!(
        rust_functions.iter().any(|(n, _)| n == "helper"),
        "the rust L2 symbol must be present"
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let n_project_rows = reader
        .query_row(SqlStatement {
            sql: "SELECT COUNT(DISTINCT id) AS n FROM entities WHERE deleted_at IS NULL \
                  AND kind='project' AND name='pkg_polyglot'"
                .into(),
            params: vec![],
            label: Some("test_l2_polyglot_project_rows".into()),
        })
        .await
        .expect("query")
        .expect("row");
    let n = match n_project_rows.get("n") {
        Some(SqlValue::Integer(n)) => *n,
        _ => -1,
    };
    assert_eq!(
        n, 1,
        "one project name across two languages remains one project entity (existing L1/L1.5 identity), \
         while the per-language sweep_clock on it stays independent"
    );

    let project_row = reader
        .query_row(SqlStatement {
            sql: "SELECT properties FROM entities WHERE deleted_at IS NULL \
                  AND kind='project' AND name='pkg_polyglot'"
                .into(),
            params: vec![],
            label: Some("test_l2_polyglot_project_props".into()),
        })
        .await
        .expect("query")
        .expect("row");
    let properties: Value = match project_row.get("properties") {
        Some(SqlValue::Text(s)) => serde_json::from_str(s).expect("valid json"),
        _ => Value::Null,
    };
    let sweep_clock = properties
        .get("sweep_clock")
        .and_then(Value::as_object)
        .expect("sweep_clock present");
    assert!(
        sweep_clock.contains_key("rust") && sweep_clock.contains_key("python"),
        "each language's sweep clock must be recorded independently: {sweep_clock:?}"
    );
}
