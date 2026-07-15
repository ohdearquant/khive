//! `code.ingest` L1 + L1.5 pipeline tests (ADR-085 Amendment 2 B3-B8).
//!
//! Exercises `khive_pack_code::source_ingest::run_code_ingest` directly
//! against on-disk fixtures — no MCP/VerbRegistry wiring needed since the
//! pipeline writes through `KhiveRuntime`'s low-level entity/graph store
//! accessors, not the verb dispatch surface (B7: the target database is
//! never the shared production runtime).

use std::collections::BTreeSet;
use std::path::Path;

use chrono::Utc;
use khive_pack_code::source_ingest::{run_code_ingest, CodeSourceIngestOptions};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::types::{SqlStatement, SqlValue};
use tempfile::TempDir;

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

/// `pkg_a` depends on `pkg_b` in its `Cargo.toml` AND imports it via
/// `use pkg_b::helper;` in `src/lib.rs` — exercises both L1 (manifest edge)
/// and L1.5 (import-scan edge) for the same project pair.
fn write_two_package_fixture(root: &Path) {
    let pkg_a = root.join("pkg_a");
    let pkg_b = root.join("pkg_b");
    std::fs::create_dir_all(pkg_a.join("src")).unwrap();
    std::fs::create_dir_all(pkg_b.join("src")).unwrap();

    std::fs::write(
        pkg_a.join("Cargo.toml"),
        "[package]\nname = \"pkg_a\"\n\n[dependencies]\npkg_b = \"0.1\"\n",
    )
    .unwrap();
    std::fs::write(
        pkg_a.join("src/lib.rs"),
        "use pkg_b::helper;\n\npub fn call_it() {\n    helper();\n}\n",
    )
    .unwrap();

    std::fs::write(pkg_b.join("Cargo.toml"), "[package]\nname = \"pkg_b\"\n").unwrap();
    std::fs::write(pkg_b.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
}

/// Normalized `(source project/module name, relation, dependency_kinds)`
/// triples for every non-deleted edge in the target db — comparable across
/// two independently-ingested databases regardless of internal UUID values
/// (which differ only if content differs, but we compare by name to make the
/// assertion legible independent of that). `dependency_kinds` is the sorted,
/// comma-joined `metadata.dependency_kinds` array — `graph_edges`'s
/// `(namespace, source_id, target_id, relation)` natural key means only one
/// `depends_on` edge can exist per pair, so multiple provenances (manifest +
/// import scan) fold onto one row's kind list rather than separate rows.
async fn edge_fingerprints(rt: &KhiveRuntime) -> Vec<(String, String, String, String)> {
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
            label: Some("test_edge_fingerprints".into()),
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
            let metadata = match r.get("metadata") {
                Some(SqlValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let mut kinds: Vec<String> = serde_json::from_str::<serde_json::Value>(&metadata)
                .ok()
                .and_then(|v| v.get("dependency_kinds").cloned())
                .and_then(|v| v.as_array().cloned())
                .map(|arr| {
                    arr.into_iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            kinds.sort();
            (relation, src, tgt, kinds.join(","))
        })
        .collect()
}

async fn entity_count(rt: &KhiveRuntime) -> i64 {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) AS n FROM entities WHERE deleted_at IS NULL".into(),
            params: vec![],
            label: Some("test_entity_count".into()),
        })
        .await
        .expect("query")
        .expect("row");
    match row.get("n") {
        Some(SqlValue::Integer(n)) => *n,
        _ => -1,
    }
}

#[tokio::test]
async fn two_package_fixture_converges_regardless_of_ingest_order() {
    let root = TempDir::new().expect("tempdir");
    write_two_package_fixture(root.path());

    // Order 1: pkg_a first, then pkg_b.
    let db1 = root.path().join("order1.db");
    let rt1 = rt_at(&db1);
    let token1 = rt1.authorize(Namespace::local()).expect("token");
    for pkg in ["pkg_a", "pkg_b"] {
        run_code_ingest(
            &rt1,
            &token1,
            CodeSourceIngestOptions {
                path: &root.path().join(pkg),
                languages: all_languages(),
                sweep_time: Utc::now(),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("ingest {pkg} (order 1) must succeed: {e}"));
    }

    // Order 2: pkg_b first, then pkg_a.
    let db2 = root.path().join("order2.db");
    let rt2 = rt_at(&db2);
    let token2 = rt2.authorize(Namespace::local()).expect("token");
    for pkg in ["pkg_b", "pkg_a"] {
        run_code_ingest(
            &rt2,
            &token2,
            CodeSourceIngestOptions {
                path: &root.path().join(pkg),
                languages: all_languages(),
                sweep_time: Utc::now(),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("ingest {pkg} (order 2) must succeed: {e}"));
    }

    let fp1 = edge_fingerprints(&rt1).await;
    let fp2 = edge_fingerprints(&rt2).await;
    assert!(!fp1.is_empty(), "order 1 must produce at least one edge");
    assert_eq!(
        fp1, fp2,
        "the two-package fixture must converge to the identical edge set regardless of \
         which package is ingested first (ADR-085 Amendment 2 B8 property 2)"
    );

    // Sanity: the manifest depends_on and the import depends_on fold onto
    // ONE edge (graph_edges' natural key allows only one `depends_on` row
    // per ordered pair) whose `dependency_kinds` records both provenances,
    // plus both contains edges.
    assert!(fp1.iter().any(|(rel, src, tgt, kinds)| rel == "depends_on"
        && src == "pkg_a"
        && tgt == "pkg_b"
        && kinds == "dependencies,import"));
    assert!(fp1
        .iter()
        .any(|(rel, src, _tgt, _kind)| rel == "contains" && src == "pkg_a"));
    assert!(fp1
        .iter()
        .any(|(rel, src, _tgt, _kind)| rel == "contains" && src == "pkg_b"));
}

#[tokio::test]
async fn reingesting_same_fixture_is_idempotent() {
    let root = TempDir::new().expect("tempdir");
    write_two_package_fixture(root.path());
    let db = root.path().join("idempotent.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let opts = || CodeSourceIngestOptions {
        path: root.path(),
        languages: all_languages(),
        sweep_time: Utc::now(),
    };

    let first = run_code_ingest(&rt, &token, opts())
        .await
        .expect("first ingest succeeds");
    let entities_after_first = entity_count(&rt).await;
    let edges_after_first = edge_fingerprints(&rt).await;

    let second = run_code_ingest(&rt, &token, opts())
        .await
        .expect("second ingest succeeds");
    let entities_after_second = entity_count(&rt).await;
    let edges_after_second = edge_fingerprints(&rt).await;

    assert_eq!(
        entities_after_first, entities_after_second,
        "re-ingesting the same fixture must not create duplicate entity rows"
    );
    assert_eq!(
        edges_after_first, edges_after_second,
        "re-ingesting the same fixture must not create duplicate or divergent edges"
    );
    assert_eq!(
        first.projects_created + first.modules_created,
        second.projects_updated + second.modules_updated,
        "everything created on the first pass must be reported as updated on the second"
    );
    assert_eq!(
        second.projects_created, 0,
        "second pass must create zero new projects"
    );
    assert_eq!(
        second.modules_created, 0,
        "second pass must create zero new modules"
    );
}

#[tokio::test]
async fn rejects_nonexistent_path() {
    let root = TempDir::new().expect("tempdir");
    let db = root.path().join("reject.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let err = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &root.path().join("does-not-exist"),
            languages: all_languages(),
            sweep_time: Utc::now(),
        },
    )
    .await
    .expect_err("nonexistent path must fail loud");
    assert!(matches!(
        err,
        khive_pack_code::CodeSourceIngestError::InvalidPath(_)
    ));
}
