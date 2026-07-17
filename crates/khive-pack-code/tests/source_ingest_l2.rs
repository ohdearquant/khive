//! L2 (Rust Scanner/Extractor symbol tier) pipeline tests (ADR-085 Amendment
//! 2 B2-B4, B8). Exercises `run_code_ingest` with `enable_l2: true` directly
//! against on-disk fixtures, mirroring `tests/source_ingest.rs`'s setup.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::Utc;
use khive_pack_code::source_ingest::{run_code_ingest, CodeSourceIngestOptions};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::types::{SqlStatement, SqlValue};
use tempfile::TempDir;

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

/// One crate with: a trait `Greeter`, a struct `Hello` implementing it, a
/// `helper` function, and a `caller` function that calls `helper` — covers
/// D3 rule 13 (`implements`), D3 rule 1 (`function depends_on function`),
/// and D3 rules 17-19 (`module contains {function,datatype,interface}`) in
/// one fixture.
fn write_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"symcrate\"\n").unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        r#"
/// Greets someone.
pub trait Greeter {
    fn greet(&self);
}

/// A friendly struct.
pub struct Hello;

impl Greeter for Hello {}

/// Does the real work.
pub fn helper() -> u32 {
    42
}

/// Calls helper.
pub fn caller() -> u32 {
    helper()
}

/// Never called by anything in this crate.
pub fn orphan() -> u32 {
    0
}
"#,
    )
    .unwrap();
}

async fn entity_rows(rt: &KhiveRuntime) -> Vec<(String, String, Option<String>, Option<String>)> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT name, entity_type, description, properties FROM entities \
                  WHERE deleted_at IS NULL \
                  AND entity_type IN ('function', 'datatype', 'interface') \
                  ORDER BY name"
                .into(),
            params: vec![],
            label: Some("test_l2_entity_rows".into()),
        })
        .await
        .expect("query entities");
    rows.into_iter()
        .map(|r| {
            let name = match r.get("name") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let entity_type = match r.get("entity_type") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let description = match r.get("description") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            };
            let properties = match r.get("properties") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            };
            (name, entity_type, description, properties)
        })
        .collect()
}

async fn edge_triples(rt: &KhiveRuntime) -> Vec<(String, String, String)> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT e.relation, s.name AS src_name, t.name AS tgt_name \
                  FROM graph_edges e \
                  JOIN entities s ON s.id = e.source_id \
                  JOIN entities t ON t.id = e.target_id \
                  WHERE e.deleted_at IS NULL \
                  ORDER BY e.relation, src_name, tgt_name"
                .into(),
            params: vec![],
            label: Some("test_l2_edge_triples".into()),
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
            (relation, src, tgt)
        })
        .collect()
}

#[tokio::test]
async fn l2_creates_declaration_entities_with_verbatim_docs() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());
    let db = root.path().join("l2.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l2: true,
        },
    )
    .await
    .expect("l2 ingest succeeds");

    let entities = entity_rows(&rt).await;
    let find = |name: &str| entities.iter().find(|(n, ..)| n == name);

    let (_, kind, doc, props) = find("helper").expect("helper entity created");
    assert_eq!(kind, "function");
    assert_eq!(doc.as_deref(), Some("Does the real work."));
    let props: serde_json::Value = serde_json::from_str(props.as_deref().unwrap()).unwrap();
    assert!(props["content_hash"].as_str().is_some());
    assert_eq!(props["language"], "rust");

    let (_, kind, doc, _) = find("Hello").expect("Hello entity created");
    assert_eq!(kind, "datatype");
    assert_eq!(doc.as_deref(), Some("A friendly struct."));

    let (_, kind, doc, _) = find("Greeter").expect("Greeter entity created");
    assert_eq!(kind, "interface");
    assert_eq!(doc.as_deref(), Some("Greets someone."));
}

/// D3 rule 13 (`datatype implements interface`) and rules 17-19 (`module
/// contains {function,datatype,interface}`).
#[tokio::test]
async fn l2_creates_implements_and_containment_edges() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());
    let db = root.path().join("l2.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l2: true,
        },
    )
    .await
    .expect("l2 ingest succeeds");

    let edges = edge_triples(&rt).await;
    assert!(
        edges
            .iter()
            .any(|(rel, s, t)| rel == "implements" && s == "Hello" && t == "Greeter"),
        "expected Hello implements Greeter, got: {edges:?}"
    );
    for decl in ["helper", "caller", "orphan", "Hello", "Greeter"] {
        assert!(
            edges
                .iter()
                .any(|(rel, s, t)| rel == "contains" && s == "crate" && t == decl),
            "expected module crate to contain {decl}, got: {edges:?}"
        );
    }
}

/// ADR-085 Amendment 2 B8 property 1 (codeflow parity), scoped to what this
/// slice's coverage-floor call extraction resolves:
/// - blast radius: a reverse `depends_on` traversal from `helper` finds
///   `caller` (its only caller in the fixture).
/// - dead symbols: `orphan` has zero incoming `depends_on` edges.
#[tokio::test]
async fn l2_function_depends_on_edges_support_blast_radius_and_dead_symbol_queries() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());
    let db = root.path().join("l2.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l2: true,
        },
    )
    .await
    .expect("l2 ingest succeeds");

    let edges = edge_triples(&rt).await;

    // Blast radius: reverse depends_on from `helper` -> its callers.
    let callers_of_helper: Vec<&str> = edges
        .iter()
        .filter(|(rel, _, t)| rel == "depends_on" && t == "helper")
        .map(|(_, s, _)| s.as_str())
        .collect();
    assert_eq!(
        callers_of_helper,
        vec!["caller"],
        "expected caller -> helper depends_on edge, got edges: {edges:?}"
    );

    // Dead symbols: `orphan` has zero incoming depends_on edges.
    let incoming_to_orphan = edges
        .iter()
        .filter(|(rel, _, t)| rel == "depends_on" && t == "orphan")
        .count();
    assert_eq!(incoming_to_orphan, 0);
}

#[tokio::test]
async fn l2_reingest_is_idempotent_no_duplicate_symbol_entities() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());
    let db = root.path().join("l2.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let opts = || CodeSourceIngestOptions {
        path: root.path(),
        languages: rust_only(),
        sweep_time: Utc::now(),
        enable_l2: true,
    };

    let first = run_code_ingest(&rt, &token, opts())
        .await
        .expect("first l2 ingest succeeds");
    let entities_after_first = entity_rows(&rt).await.len();

    let second = run_code_ingest(&rt, &token, opts())
        .await
        .expect("second l2 ingest succeeds");
    let entities_after_second = entity_rows(&rt).await.len();

    assert_eq!(
        entities_after_first, entities_after_second,
        "re-ingest must not create duplicate declaration entities"
    );
    assert_eq!(
        second.symbols_created, 0,
        "second pass creates zero new symbols"
    );
    assert_eq!(first.symbols_created, second.symbols_updated);
}

/// When `enable_l2` is false (the default), no symbol-tier entities are
/// created — the L2 tier changes nothing for existing L1/L1.5-only callers.
#[tokio::test]
async fn l2_disabled_by_default_creates_no_symbol_entities() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());
    let db = root.path().join("no_l2.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l2: false,
        },
    )
    .await
    .expect("l1+l1.5-only ingest succeeds");

    assert_eq!(report.symbols_created, 0);
    let entities = entity_rows(&rt).await;
    assert!(
        entities.is_empty(),
        "no entity_type-carrying entities expected without l2, got: {entities:?}"
    );
}
