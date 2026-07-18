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

fn all_languages() -> BTreeSet<&'static str> {
    ["rust", "python", "typescript"].into_iter().collect()
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
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect("l2 ingest succeeds");

    let entities = entity_rows(&rt).await;
    let find = |name: &str| entities.iter().find(|(n, ..)| n == name);

    // Verbatim transcription (ADR-069 D5): the space between `///` and the
    // comment text is part of the doc string, not trimmed away.
    let (_, kind, doc, props) = find("helper").expect("helper entity created");
    assert_eq!(kind, "function");
    assert_eq!(doc.as_deref(), Some(" Does the real work."));
    let props: serde_json::Value = serde_json::from_str(props.as_deref().unwrap()).unwrap();
    assert!(props["content_hash"].as_str().is_some());
    assert_eq!(props["language"], "rust");

    let (_, kind, doc, _) = find("Hello").expect("Hello entity created");
    assert_eq!(kind, "datatype");
    assert_eq!(doc.as_deref(), Some(" A friendly struct."));

    let (_, kind, doc, _) = find("Greeter").expect("Greeter entity created");
    assert_eq!(kind, "interface");
    assert_eq!(doc.as_deref(), Some(" Greets someone."));
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
            enable_l1: true,
            enable_l1_5: true,
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
            enable_l1: true,
            enable_l1_5: true,
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
        enable_l1: true,
        enable_l1_5: true,
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
    assert!(
        first.symbols_created > 0,
        "first pass must create the fixture's declarations"
    );
    assert_eq!(
        second.symbols_created, 0,
        "second pass creates zero new symbols"
    );
    assert_eq!(
        second.symbols_updated, 0,
        "every declaration's content_hash matches the prior sweep unchanged, so the second \
         pass reports zero symbol updates — a last_seen_at-only touch, not a rewrite"
    );
}

/// #1087 item 5 regression: enabling L2 on a sweep where the file's content
/// is unchanged from a prior L1.5-only ingest must still scan it. Before the
/// fix, `module_changed = false` routed straight to loading the module's
/// (nonexistent, because L2 never ran) `declaration_ids`, silently
/// producing zero L2 symbols for the file forever.
#[tokio::test]
async fn l2_scans_on_tier_upgrade_even_when_module_content_is_unchanged() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());
    let db = root.path().join("l2.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let l1_5_only = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: false,
        },
    )
    .await
    .expect("L1.5-only ingest succeeds");
    assert_eq!(
        l1_5_only.symbols_created, 0,
        "L2 disabled: no symbol entities from the first pass"
    );
    assert!(
        entity_rows(&rt).await.is_empty(),
        "no function/datatype/interface entities exist before L2 ever runs"
    );

    let l2_upgrade = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect("L2-upgrade ingest succeeds even though file content is unchanged");

    assert!(
        l2_upgrade.symbols_created > 0,
        "enabling L2 on an unchanged file must still scan and create its declarations, \
         got symbols_created = {}",
        l2_upgrade.symbols_created
    );
    assert!(
        !entity_rows(&rt).await.is_empty(),
        "function/datatype/interface entities must exist after the L2 upgrade pass"
    );
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
            enable_l1: true,
            enable_l1_5: true,
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

/// A doc comment carrying a credential-shaped string must not
/// reach storage — L2 entity writes go through the same secret gate
/// `KhiveRuntime::create_entity` applies (ADR-085 D6), not a raw
/// `EntityStore::upsert_entity` call that bypasses it.
#[tokio::test]
async fn l2_rejects_declaration_whose_doc_comment_carries_a_secret() {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"secretcrate\"\n",
    )
    .unwrap();
    let leaky_source = "/// token: ghp_abcdefghijklmnopqrstuvwxyz1234567890\npub fn leaky() {}\n"; // gitleaks:allow
    std::fs::write(root.path().join("src/lib.rs"), leaky_source).unwrap();
    let db = root.path().join("secret.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let err = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect_err("a doc comment carrying a credential-shaped string must be rejected");
    assert!(
        err.to_string().to_lowercase().contains("secret"),
        "expected a secret-gate rejection, got: {err}"
    );

    let entities = entity_rows(&rt).await;
    assert!(
        entities.is_empty(),
        "the leaking declaration must never reach storage, got: {entities:?}"
    );
}

/// The same-named function in two different modules must resolve
/// its `helper()` call only within its own module — the symbol index is
/// keyed by `(project, module_path, name, kind)`, not `(project, name,
/// kind)`, so the two `helper` declarations never collapse onto one entry.
#[tokio::test]
async fn l2_same_named_function_in_different_modules_resolves_within_own_module_only() {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"qualcrate\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "mod a;\nmod b;\n").unwrap();
    std::fs::write(
        root.path().join("src/a.rs"),
        "pub fn helper() -> u32 { 1 }\npub fn caller() -> u32 { helper() }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/b.rs"),
        "pub fn helper() -> u32 { 2 }\n",
    )
    .unwrap();
    let db = root.path().join("qual.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect("l2 ingest succeeds");

    let edges = edge_triples(&rt).await;
    let depends_on: Vec<_> = edges
        .iter()
        .filter(|(rel, ..)| rel == "depends_on")
        .collect();
    assert_eq!(
        depends_on.len(),
        1,
        "expected exactly one depends_on edge (a::caller -> a::helper), got: {edges:?}"
    );
    assert!(
        depends_on
            .iter()
            .any(|(_, s, t)| s == "caller" && t == "helper"),
        "expected caller depends_on helper, got: {edges:?}"
    );
}

/// Two calls to the same helper from one function must collapse
/// onto a single `depends_on` edge, not one edge operation per call site.
#[tokio::test]
async fn l2_dedups_repeated_calls_to_the_same_helper_into_one_edge() {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"dedupcrate\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/lib.rs"),
        "pub fn helper() -> u32 { 0 }\npub fn caller() -> u32 { helper() + helper() + helper() }\n",
    )
    .unwrap();
    let db = root.path().join("dedup.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect("l2 ingest succeeds");

    let edges = edge_triples(&rt).await;
    let depends_on: Vec<_> = edges
        .iter()
        .filter(|(rel, s, t)| rel == "depends_on" && s == "caller" && t == "helper")
        .collect();
    assert_eq!(
        depends_on.len(),
        1,
        "three calls to the same helper must yield exactly one depends_on edge, got: {edges:?}"
    );
}

/// `tiers=["l1"]`/`["l1.5"]`/`["l2"]` each run only their own
/// tier, and combinations compose.
#[tokio::test]
async fn tiers_are_independently_selectable_and_compose() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());

    // l1 alone: no manifest dependencies in this fixture, so l1 produces the
    // project entity but no modules and no symbols.
    {
        let db = root.path().join("l1_only.db");
        let rt = rt_at(&db);
        let token = rt.authorize(Namespace::local()).expect("token");
        let report = run_code_ingest(
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
        .expect("l1-only ingest succeeds");
        assert_eq!(report.projects_created, 1);
        assert_eq!(report.modules_created, 0, "l1 alone must not walk files");
        assert_eq!(report.symbols_created, 0);
    }

    // l1.5 alone: modules created, no symbol-tier entities.
    {
        let db = root.path().join("l1_5_only.db");
        let rt = rt_at(&db);
        let token = rt.authorize(Namespace::local()).expect("token");
        let report = run_code_ingest(
            &rt,
            &token,
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
        .expect("l1.5-only ingest succeeds");
        assert!(report.modules_created > 0, "l1.5 alone must walk files");
        assert_eq!(
            report.symbols_created, 0,
            "l1.5 alone must not run the symbol tier"
        );
    }

    // l2 alone: symbol-tier entities created, no import-edge unresolved specs.
    {
        let db = root.path().join("l2_only.db");
        let rt = rt_at(&db);
        let token = rt.authorize(Namespace::local()).expect("token");
        let report = run_code_ingest(
            &rt,
            &token,
            CodeSourceIngestOptions {
                path: root.path(),
                languages: rust_only(),
                sweep_time: Utc::now(),
                enable_l1: false,
                enable_l1_5: false,
                enable_l2: true,
            },
        )
        .await
        .expect("l2-only ingest succeeds");
        assert!(
            report.symbols_created > 0,
            "l2 alone must run the symbol tier"
        );
    }
}

fn write_polyglot_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("rustpkg/src")).unwrap();
    std::fs::write(
        root.join("rustpkg/Cargo.toml"),
        "[package]\nname = \"rustpkg\"\n",
    )
    .unwrap();
    std::fs::write(root.join("rustpkg/src/lib.rs"), "pub fn f() {}\n").unwrap();

    std::fs::create_dir_all(root.join("pypkg")).unwrap();
    std::fs::write(
        root.join("pypkg/pyproject.toml"),
        "[project]\nname = \"pypkg\"\n",
    )
    .unwrap();
    std::fs::write(root.join("pypkg/mod.py"), "def g():\n    pass\n").unwrap();

    std::fs::create_dir_all(root.join("tspkg")).unwrap();
    std::fs::write(root.join("tspkg/package.json"), "{\"name\": \"tspkg\"}").unwrap();
    std::fs::write(root.join("tspkg/index.ts"), "export function h() {}\n").unwrap();
}

/// An L2-only ingest over a polyglot tree must not walk, read,
/// hash, or upsert modules for languages L2 doesn't support (python,
/// typescript) — only the rust project/module gets touched.
#[tokio::test]
async fn l2_only_polyglot_ingest_touches_only_rust() {
    let root = TempDir::new().expect("tempdir");
    write_polyglot_fixture(root.path());
    let db = root.path().join("polyglot_l2.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: all_languages(),
            sweep_time: Utc::now(),
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
    .expect("l2-only polyglot ingest succeeds");

    assert_eq!(
        report.projects_created, 1,
        "only the rust project should be touched: {report:?}"
    );
    assert_eq!(
        report.modules_created, 1,
        "only the rust module should be walked: {report:?}"
    );
    assert!(report.symbols_created > 0);
}

/// An ingest with every tier disabled performs zero writes.
#[tokio::test]
async fn all_tiers_disabled_performs_zero_writes() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());
    let db = root.path().join("zero_tier.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: false,
        },
    )
    .await
    .expect("all-tiers-disabled ingest succeeds");

    assert_eq!(report.projects_created, 0);
    assert_eq!(report.modules_created, 0);
    assert_eq!(report.symbols_created, 0);
    assert_eq!(report.edges_created, 0);
    assert_eq!(report.unresolved_recorded, 0);
}

/// Reads back an edge's `last_seen_at` metadata stamp for the given relation
/// and endpoint names, or `None` if no such edge exists.
async fn edge_last_seen(
    rt: &KhiveRuntime,
    relation: &str,
    src_name: &str,
    tgt_name: &str,
) -> Option<String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT e.metadata AS metadata \
                  FROM graph_edges e \
                  JOIN entities s ON s.id = e.source_id \
                  JOIN entities t ON t.id = e.target_id \
                  WHERE e.deleted_at IS NULL AND e.relation = ?1 \
                  AND s.name = ?2 AND t.name = ?3"
                .into(),
            params: vec![
                SqlValue::Text(relation.to_string()),
                SqlValue::Text(src_name.to_string()),
                SqlValue::Text(tgt_name.to_string()),
            ],
            label: Some("test_l2_edge_last_seen".into()),
        })
        .await
        .expect("query edge metadata");
    let row = rows.into_iter().next()?;
    let metadata: serde_json::Value = match row.get("metadata") {
        Some(SqlValue::Text(s)) => serde_json::from_str(s).ok()?,
        _ => return None,
    };
    metadata
        .get("last_seen_at")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// ADR-085 B5 extended to L2 edges: a declaration re-extracted this scan
/// whose freshly extracted call list no longer includes a previously
/// recorded target must NOT have that edge deleted or mutated — it simply
/// keeps its old `last_seen_at` stamp, while the newly resolved edge gets the
/// new sweep's stamp. Currency is a view-layer filter, never a data-layer
/// deletion.
#[tokio::test]
async fn stale_call_edge_keeps_old_stamp_when_declaration_changes() {
    let root = TempDir::new().expect("tempdir");
    write_fixture(root.path());
    let db = root.path().join("b5_edges.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let first_sweep = Utc::now();
    let opts = |sweep_time| CodeSourceIngestOptions {
        path: root.path(),
        languages: rust_only(),
        sweep_time,
        enable_l1: true,
        enable_l1_5: true,
        enable_l2: true,
    };
    let first = run_code_ingest(&rt, &token, opts(first_sweep))
        .await
        .expect("first ingest");
    assert!(first.symbol_edges_stamped > 0);
    let edges = edge_triples(&rt).await;
    assert!(
        edges
            .iter()
            .any(|(rel, src, tgt)| rel == "depends_on" && src == "caller" && tgt == "helper"),
        "expected initial caller->helper edge, got: {edges:?}"
    );
    let helper_stamp_first = edge_last_seen(&rt, "depends_on", "caller", "helper")
        .await
        .expect("caller->helper edge carries a last_seen_at stamp");

    // `caller` now calls `other` instead of `helper`.
    std::fs::write(
        root.path().join("src/lib.rs"),
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

/// Something else entirely.
pub fn other() -> u32 {
    7
}

/// Calls other now, not helper.
pub fn caller() -> u32 {
    other()
}

/// Never called by anything in this crate.
pub fn orphan() -> u32 {
    0
}
"#,
    )
    .unwrap();

    let second_sweep = first_sweep + chrono::Duration::seconds(1);
    let second = run_code_ingest(&rt, &token, opts(second_sweep))
        .await
        .expect("second ingest");
    assert!(
        second.symbol_edges_stamped > 0,
        "the re-resolved caller->other edge must be stamped this sweep"
    );

    let edges = edge_triples(&rt).await;
    // B5: the stale caller->helper edge is NEVER deleted — its declaration
    // (caller) was re-scanned, but B5's no-automatic-deletion rule applies to
    // L2 edges exactly as it does to entities.
    assert!(
        edges
            .iter()
            .any(|(rel, src, tgt)| rel == "depends_on" && src == "caller" && tgt == "helper"),
        "stale caller->helper edge must still exist after re-ingest (B5: no deletion), got: {edges:?}"
    );
    let helper_stamp_second = edge_last_seen(&rt, "depends_on", "caller", "helper")
        .await
        .expect("caller->helper edge still exists");
    assert_eq!(
        helper_stamp_first, helper_stamp_second,
        "an edge this scan did not re-resolve keeps its OLD last_seen_at stamp untouched"
    );

    assert!(
        edges
            .iter()
            .any(|(rel, src, tgt)| rel == "depends_on" && src == "caller" && tgt == "other"),
        "new caller->other edge must exist, got: {edges:?}"
    );
    let other_stamp = edge_last_seen(&rt, "depends_on", "caller", "other")
        .await
        .expect("caller->other edge carries a last_seen_at stamp");
    assert!(
        other_stamp > helper_stamp_second,
        "the re-resolved edge must carry the NEW sweep's stamp, later than the untouched stale edge"
    );

    // implements edge (Hello implements Greeter) is untouched — unrelated
    // to caller's own outgoing edge set.
    assert!(edges
        .iter()
        .any(|(rel, src, tgt)| rel == "implements" && src == "Hello" && tgt == "Greeter"));
}

/// findings 6/9: a file with more declarations than SQLite's
/// `SQLITE_MAX_VARIABLE_NUMBER` (999) must still ingest successfully (the
/// `get_entities_by_ids` content-hash lookup) and re-ingest successfully
/// unchanged (the `touch_last_seen_at` batched UPDATE).
#[tokio::test]
async fn ingest_with_over_900_declarations_does_not_exceed_sql_param_limits() {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"bigcrate\"\n",
    )
    .unwrap();
    let mut src = String::new();
    for i in 0..950 {
        src.push_str(&format!("pub fn f{i}() {{}}\n"));
    }
    std::fs::write(root.path().join("src/lib.rs"), src).unwrap();

    let db = root.path().join("big.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    let opts = || CodeSourceIngestOptions {
        path: root.path(),
        languages: rust_only(),
        sweep_time: Utc::now(),
        enable_l1: true,
        enable_l1_5: true,
        enable_l2: true,
    };

    let report = run_code_ingest(&rt, &token, opts())
        .await
        .expect("large ingest succeeds without exceeding SQL param limits");
    assert_eq!(report.symbols_created, 950);

    let report2 = run_code_ingest(&rt, &token, opts())
        .await
        .expect("unchanged re-ingest of large fixture succeeds (chunked last_seen_at touch)");
    assert_eq!(report2.symbols_created, 0);
    assert_eq!(report2.symbols_updated, 0);
}

/// Inline modules, inherent/trait impl methods, and trait
/// default-body and signature-only methods all produce declarations end to end, and a call
/// inside a nested module resolves against that module's own path.
#[tokio::test]
async fn l2_extracts_methods_and_inline_modules_end_to_end() {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"modcrate\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/lib.rs"),
        r#"
pub struct S;
impl S {
    pub fn m() {}
}

pub trait T {
    fn required(&self);
    fn provided(&self) {}
}
impl T for S {
    fn required(&self) {}
}

pub mod inner {
    pub fn helper() {}
    pub fn f() {
        helper();
    }
}
"#,
    )
    .unwrap();

    let db = root.path().join("mods.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect("ingest succeeds");

    let entities = entity_rows(&rt).await;
    let names: Vec<&str> = entities.iter().map(|(n, ..)| n.as_str()).collect();
    assert!(names.contains(&"S::m"), "impl method: {names:?}");
    assert!(
        names.contains(&"T::provided"),
        "trait default method: {names:?}"
    );
    assert!(
        names.contains(&"<S as T>::required"),
        "trait impl method: {names:?}"
    );
    // `inner`'s own module-kind entity is proven indirectly below (the
    // `inner -> helper` contains edge requires it to exist as an endpoint);
    // `entity_rows` only selects function/datatype/interface entity_types.
    assert!(
        names.contains(&"helper"),
        "fn nested in inline module: {names:?}"
    );
    assert!(
        names.contains(&"f"),
        "fn nested in inline module: {names:?}"
    );

    let edges = edge_triples(&rt).await;
    assert!(
        edges
            .iter()
            .any(|(rel, src, tgt)| rel == "depends_on" && src == "f" && tgt == "helper"),
        "call inside inner::f must resolve against inner's own module path, got: {edges:?}"
    );
    assert!(
        edges
            .iter()
            .any(|(rel, src, tgt)| rel == "contains" && src == "inner" && tgt == "helper"),
        "inner module must contain its nested fn via `contains`, got: {edges:?}"
    );
}

/// A qualified `impl crate::traits::T for crate::types::S {}` at the crate
/// root, where the trait and type each live in their own sibling module, must
/// still produce the `implements` edge: both paths resolve through the same
/// `crate`/`self`/`super`-aware module resolution used for call targets,
/// rather than the impl's own module alone.
#[tokio::test]
async fn qualified_impl_across_sibling_modules_produces_implements_edge() {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"qualcrate\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/lib.rs"),
        r#"
pub mod traits {
    pub trait T {}
}

pub mod types {
    pub struct S;
}

impl crate::traits::T for crate::types::S {}
"#,
    )
    .unwrap();

    let db = root.path().join("qualified_impl.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: rust_only(),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        },
    )
    .await
    .expect("ingest succeeds");

    let edges = edge_triples(&rt).await;
    assert!(
        edges
            .iter()
            .any(|(rel, src, tgt)| rel == "implements" && src == "S" && tgt == "T"),
        "qualified impl across sibling modules must resolve to an implements edge, got: {edges:?}"
    );
}
