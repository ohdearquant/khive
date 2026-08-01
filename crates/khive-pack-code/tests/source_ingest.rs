//! `code.ingest` L1 + L1.5 pipeline tests (ADR-085 Amendments 2 and 5).
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

/// Normalized `(relation, source name, target name, dependency kinds,
/// dependency scopes)`
/// triples for every non-deleted edge in the target db — comparable across
/// two independently-ingested databases regardless of internal UUID values
/// (which differ only if content differs, but we compare by name to make the
/// assertion legible independent of that). The final two fields are sorted,
/// comma-joined metadata arrays — `graph_edges`'s
/// `(namespace, source_id, target_id, relation)` natural key means only one
/// `depends_on` edge can exist per pair, so multiple provenances (manifest +
/// import scan) fold onto one row's kind list rather than separate rows.
async fn edge_fingerprints(rt: &KhiveRuntime) -> Vec<(String, String, String, String, String)> {
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
            let mut scopes: Vec<String> = serde_json::from_str::<serde_json::Value>(&metadata)
                .ok()
                .and_then(|v| v.get("dependency_scopes").cloned())
                .and_then(|v| v.as_array().cloned())
                .map(|arr| {
                    arr.into_iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            scopes.sort();
            (relation, src, tgt, kinds.join(","), scopes.join(","))
        })
        .collect()
}

async fn entity_names(rt: &KhiveRuntime) -> Vec<String> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT name FROM entities WHERE deleted_at IS NULL".into(),
            params: vec![],
            label: Some("test_entity_names".into()),
        })
        .await
        .expect("query entity names");
    rows.into_iter()
        .filter_map(|r| match r.get("name") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
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

async fn module_properties_for_path(
    rt: &KhiveRuntime,
    source_project: &str,
    source_path: &str,
) -> Vec<serde_json::Value> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT properties FROM entities \
                  WHERE deleted_at IS NULL AND entity_type='module' \
                  AND json_extract(properties,'$.source_project')=?1 \
                  AND json_extract(properties,'$.source_path')=?2"
                .into(),
            params: vec![
                SqlValue::Text(source_project.to_string()),
                SqlValue::Text(source_path.to_string()),
            ],
            label: Some("test_module_properties_for_path".into()),
        })
        .await
        .expect("query modules by source path");
    rows.into_iter()
        .filter_map(|row| match row.get("properties") {
            Some(SqlValue::Text(properties)) => serde_json::from_str(properties).ok(),
            _ => None,
        })
        .collect()
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        // Keep the fixture independent of machine-wide hooks (for example,
        // a global leak guard) when it creates its two local commits.
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git must be available for source-revision test");
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
    // per ordered pair) whose evidence and scope arrays record both
    // provenances, plus both contains edges.
    assert!(fp1
        .iter()
        .any(|(rel, src, tgt, kinds, scopes)| rel == "depends_on"
            && src == "pkg_a"
            && tgt == "pkg_b"
            && kinds == "dependencies,import"
            && scopes == "normal"));
    assert!(fp1
        .iter()
        .any(|(rel, src, _tgt, _kinds, _scopes)| rel == "contains" && src == "pkg_a"));
    assert!(fp1
        .iter()
        .any(|(rel, src, _tgt, _kinds, _scopes)| rel == "contains" && src == "pkg_b"));
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
async fn manifest_scopes_keep_dev_back_edges_out_of_the_production_graph() {
    let root = TempDir::new().expect("tempdir");
    for package in ["pkg_a", "pkg_b"] {
        std::fs::create_dir_all(root.path().join(package).join("src")).unwrap();
        std::fs::write(
            root.path().join(package).join("src/lib.rs"),
            format!("pub fn {package}() {{}}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        root.path().join("pkg_a/Cargo.toml"),
        "[package]\nname = \"pkg-a\"\n\n[dependencies]\npkg-b = \"0.1\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("pkg_b/Cargo.toml"),
        "[package]\nname = \"pkg-b\"\n\n[dev-dependencies]\npkg-a = \"0.1\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("pkg_b/src/lib.rs"),
        "use pkg_a::test_helper;\n\npub fn pkg_b() { test_helper(); }\n",
    )
    .unwrap();

    let rt = rt_at(&root.path().join("scopes.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: all_languages(),
            sweep_time: Utc::now(),
        },
    )
    .await
    .expect("scope fixture ingests");

    let edges = edge_fingerprints(&rt).await;
    assert!(edges
        .iter()
        .any(|(relation, source, target, kinds, scopes)| {
            relation == "depends_on"
                && source == "pkg-a"
                && target == "pkg-b"
                && kinds == "dependencies"
                && scopes == "normal"
        }));
    assert!(edges
        .iter()
        .any(|(relation, source, target, kinds, scopes)| {
            relation == "depends_on"
                && source == "pkg-b"
                && target == "pkg-a"
                && kinds == "dev-dependencies,import"
                && scopes == "dev"
        }));

    let production_edges: BTreeSet<(&str, &str)> = edges
        .iter()
        .filter(|(relation, _, _, _, scopes)| relation == "depends_on" && scopes != "dev")
        .map(|(_, source, target, _, _)| (source.as_str(), target.as_str()))
        .collect();
    assert!(production_edges.contains(&("pkg-a", "pkg-b")));
    assert!(
        !production_edges.contains(&("pkg-b", "pkg-a")),
        "the reciprocal manifest entry is dev-only and must not create a production cycle"
    );
    let pkg_b_module = module_properties_for_path(&rt, "pkg-b", "pkg_b/src/lib.rs").await;
    assert_eq!(pkg_b_module.len(), 1);
    assert_eq!(pkg_b_module[0]["import_scan_status"], "scanned");
    assert_eq!(pkg_b_module[0]["unresolved_import_count"], 0);
}

#[tokio::test]
async fn module_paths_revisions_coverage_and_contains_ownership_are_queryable() {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"coverage_fixture\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "pub fn independent() {}\n").unwrap();
    std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        root.path().join("src/partial.rs"),
        "use crate::missing::Thing;\n\npub fn needs_missing(_: Thing) {}\n",
    )
    .unwrap();

    git(root.path(), &["init", "-q"]);
    git(root.path(), &["add", "Cargo.toml", "src"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=khive-test",
            "-c",
            "user.email=khive-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "initial",
        ],
    );
    let first_revision = git(root.path(), &["rev-parse", "HEAD"]);

    let rt = rt_at(&root.path().join("coverage.db"));
    let token = rt.authorize(Namespace::local()).expect("token");
    let first = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: all_languages(),
            sweep_time: Utc::now(),
        },
    )
    .await
    .expect("coverage fixture ingests");
    assert_eq!(first.source_revision, first_revision);

    let lib = module_properties_for_path(&rt, "coverage_fixture", "src/lib.rs").await;
    assert_eq!(lib.len(), 1, "path lookup must resolve to one module");
    assert_eq!(
        lib[0]["source_revision"].as_str(),
        Some(first_revision.as_str())
    );
    assert_eq!(lib[0]["import_scan_status"], "scanned");
    assert_eq!(lib[0]["import_specifier_count"], 0);
    assert_eq!(lib[0]["unresolved_import_count"], 0);

    let main = module_properties_for_path(&rt, "coverage_fixture", "src/main.rs").await;
    assert_eq!(main.len(), 1, "binary-root path must resolve independently");
    assert_eq!(main[0]["module_path"], "crate::main");
    assert_eq!(main[0]["import_scan_status"], "scanned");

    let partial = module_properties_for_path(&rt, "coverage_fixture", "src/partial.rs").await;
    assert_eq!(partial.len(), 1, "path lookup must resolve to one module");
    assert_eq!(
        partial[0]["source_revision"].as_str(),
        Some(first_revision.as_str())
    );
    assert_eq!(partial[0]["import_scan_status"], "partially_resolved");
    assert_eq!(partial[0]["import_specifier_count"], 1);
    assert_eq!(partial[0]["unresolved_import_count"], 1);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let ownership_rows = reader
        .query_all(SqlStatement {
            sql: "SELECT json_extract(parent.properties,'$.source_project') AS parent_project, \
                         json_extract(child.properties,'$.source_project') AS child_project \
                  FROM graph_edges edge \
                  JOIN entities parent ON parent.id=edge.source_id \
                  JOIN entities child ON child.id=edge.target_id \
                  WHERE edge.relation='contains' AND edge.deleted_at IS NULL"
                .into(),
            params: vec![],
            label: Some("test_contains_ownership".into()),
        })
        .await
        .expect("query contains ownership");
    assert!(!ownership_rows.is_empty());
    for row in ownership_rows {
        let parent_project = match row.get("parent_project") {
            Some(SqlValue::Text(project)) => project,
            value => panic!("expected text parent source_project, got {value:?}"),
        };
        let child_project = match row.get("child_project") {
            Some(SqlValue::Text(project)) => project,
            value => panic!("expected text child source_project, got {value:?}"),
        };
        assert_eq!(parent_project, "coverage_fixture");
        assert_eq!(child_project, "coverage_fixture");
    }
    drop(reader);

    std::fs::write(root.path().join("src/missing.rs"), "pub struct Thing;\n").unwrap();
    git(root.path(), &["add", "src/missing.rs"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=khive-test",
            "-c",
            "user.email=khive-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "add missing module",
        ],
    );
    let second_revision = git(root.path(), &["rev-parse", "HEAD"]);
    assert_ne!(first_revision, second_revision);

    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: all_languages(),
            sweep_time: Utc::now(),
        },
    )
    .await
    .expect("coverage fixture re-ingests");
    let resolved = module_properties_for_path(&rt, "coverage_fixture", "src/partial.rs").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0]["source_revision"].as_str(),
        Some(second_revision.as_str())
    );
    assert_eq!(resolved[0]["import_scan_status"], "scanned");
    assert_eq!(resolved[0]["unresolved_import_count"], 0);
}

/// `src/lib.rs` importing `crate::foo::Thing`, an item declared in
/// `src/foo.rs`, must resolve to a `crate -> foo` `depends_on` edge with
/// build-scope `dependency_kinds=["import"]` — not stay unresolved because the raw
/// import target (`foo::Thing`) names an item inside `foo`, not a nested
/// module `foo::Thing` (see #1039).
fn write_item_import_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"itemimport\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "mod foo;\n\nuse crate::foo::Thing;\n\npub fn use_it() -> Thing {\n    Thing\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/foo.rs"),
        "pub struct Thing;\n\nimpl Thing {}\n",
    )
    .unwrap();
}

#[tokio::test]
async fn rust_item_import_resolves_to_containing_module_after_reingest() {
    let root = TempDir::new().expect("tempdir");
    write_item_import_fixture(root.path());
    let db = root.path().join("item_import.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let opts = || CodeSourceIngestOptions {
        path: root.path(),
        languages: all_languages(),
        sweep_time: Utc::now(),
    };

    // First pass records the item import as unresolved (module `foo` did
    // not exist yet when `lib.rs` was scanned, depending on file-walk
    // order); the synchronous re-resolve pass inside the same call already
    // covers this, but re-ingesting once more is the documented idempotency
    // contract (B4/B6) and removes any file-walk-order sensitivity.
    run_code_ingest(&rt, &token, opts())
        .await
        .expect("first ingest succeeds");
    run_code_ingest(&rt, &token, opts())
        .await
        .expect("second ingest succeeds");

    let edges = edge_fingerprints(&rt).await;
    assert!(
        edges
            .iter()
            .any(|(rel, src, tgt, kinds, scopes)| rel == "depends_on"
                && src == "crate"
                && tgt == "foo"
                && kinds == "import"
                && scopes == "build"),
        "expected one crate -> foo build-scope import edge, got: {edges:?}"
    );
}

/// A manifestless folder (no `Cargo.toml`/`pyproject.toml`/`package.json`
/// anywhere above its source files) must still produce project/module
/// entities and import edges under the basename-fallback identity rule
/// (ADR-085 Amendment 2 B4), not be silently skipped for lack of a manifest
/// (see #1039).
#[tokio::test]
async fn manifestless_rust_folder_uses_basename_fallback() {
    let root = TempDir::new().expect("tempdir");
    let proj = root.path().join("bare_rust_project");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(
        proj.join("src/lib.rs"),
        "mod util;\n\nuse crate::util::helper;\n\npub fn call() {\n    helper();\n}\n",
    )
    .unwrap();
    std::fs::write(proj.join("src/util.rs"), "pub fn helper() {}\n").unwrap();

    let db = root.path().join("manifestless.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &proj,
            languages: all_languages(),
            sweep_time: Utc::now(),
        },
    )
    .await
    .expect("manifestless ingest succeeds");
    // Re-ingest so the synchronous re-resolve pass materializes the
    // module -> module edge regardless of file-walk order.
    run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: &proj,
            languages: all_languages(),
            sweep_time: Utc::now(),
        },
    )
    .await
    .expect("manifestless re-ingest succeeds");

    assert!(
        report.modules_created > 0 || report.projects_created > 0,
        "a manifestless folder with source files must still create project/module entities"
    );

    let edges = edge_fingerprints(&rt).await;
    assert!(
        edges
            .iter()
            .any(|(rel, src, _tgt, _kinds, _scopes)| rel == "contains" && src == "bare_rust_project"),
        "expected the basename-fallback project 'bare_rust_project' to contain its module, got: {edges:?}"
    );
    assert!(
        edges
            .iter()
            .any(|(rel, src, tgt, kinds, scopes)| rel == "depends_on"
                && src == "crate"
                && tgt == "util"
                && kinds == "import"
                && scopes == "build"),
        "expected one crate -> util build-scope import edge, got: {edges:?}"
    );
}

/// `pkg_a` declares a dependency whose name is itself a secret-shaped string
/// (`scheme://user:pass@host` — the exact url-userinfo pattern the runtime
/// secret gate blocks, ADR-085 D6 #4). `pkg_b` is an ordinary sibling
/// project with no such dependency.
fn write_gate_blocked_dependency_fixture(root: &Path) {
    let pkg_a = root.join("pkg_a");
    let pkg_b = root.join("pkg_b");
    std::fs::create_dir_all(pkg_a.join("src")).unwrap();
    std::fs::create_dir_all(pkg_b.join("src")).unwrap();

    std::fs::write(
        pkg_a.join("Cargo.toml"),
        "[package]\nname = \"pkg_a\"\n\n[dependencies]\n\"scheme://user:pass@host\" = \"0.1\"\n",
    )
    .unwrap();
    std::fs::write(pkg_a.join("src/lib.rs"), "pub fn call_it() {}\n").unwrap();

    std::fs::write(pkg_b.join("Cargo.toml"), "[package]\nname = \"pkg_b\"\n").unwrap();
    std::fs::write(pkg_b.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
}

/// issue #1594: a single secret-gate refusal during `code.ingest` must
/// quarantine the refused item, not abort the whole pass. `pkg_a`'s
/// gate-blocked dependency name is recorded in `report.blocked` and skipped;
/// both `pkg_a`'s own project entity and the unrelated sibling `pkg_b` are
/// still ingested normally.
#[tokio::test]
async fn gate_blocked_write_is_quarantined_and_siblings_ingest() {
    let root = TempDir::new().expect("tempdir");
    write_gate_blocked_dependency_fixture(root.path());
    let db = root.path().join("gate_blocked.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: all_languages(),
            sweep_time: Utc::now(),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("ingest must complete despite one gate-blocked write: {e}"));

    assert_eq!(
        report.blocked.len(),
        1,
        "expected exactly one quarantined write, got: {:?}",
        report.blocked
    );
    assert_eq!(
        report.blocked_count, 1,
        "blocked_count must match the single entry in blocked"
    );
    for entry in &report.blocked {
        assert_eq!(entry.detector, "url-userinfo");
        assert!(
            !entry.masked_excerpt.contains("user:pass"),
            "masked excerpt must not echo the credential-shaped span: {}",
            entry.masked_excerpt
        );
    }

    assert!(
        report.projects_created >= 2,
        "both pkg_a and the unrelated sibling pkg_b must be created, got {} \
         (report: {report:?})",
        report.projects_created
    );

    let names = entity_names(&rt).await;
    assert!(
        names.iter().any(|n| n == "pkg_a"),
        "pkg_a's own project must still be ingested despite its blocked dependency: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "pkg_b"),
        "unrelated sibling pkg_b must be ingested: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("user:pass")),
        "the gate-blocked dependency name must never be written as an entity: {names:?}"
    );
}

/// `pkg_secret`'s own manifest-declared project name is itself a
/// secret-shaped string (`scheme://user:pass@host`) — the value the runtime
/// secret gate refuses is the entity's `name`, not one of its dependencies.
/// `pkg_ok` is an ordinary sibling project.
fn write_gate_blocked_project_name_fixture(root: &Path) {
    let pkg_secret = root.join("pkg_secret");
    let pkg_ok = root.join("pkg_ok");
    std::fs::create_dir_all(pkg_secret.join("src")).unwrap();
    std::fs::create_dir_all(pkg_ok.join("src")).unwrap();

    std::fs::write(
        pkg_secret.join("Cargo.toml"),
        "[package]\nname = \"scheme://user:pass@host\"\n",
    )
    .unwrap();
    std::fs::write(pkg_secret.join("src/lib.rs"), "pub fn call_it() {}\n").unwrap();

    std::fs::write(pkg_ok.join("Cargo.toml"), "[package]\nname = \"pkg_ok\"\n").unwrap();
    std::fs::write(pkg_ok.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
}

/// issue #1594 / gate-report label leak: when the *project name itself* is
/// secret-shaped, the quarantine report's `blocked[].file` must carry a
/// trusted on-disk file location (the governing manifest for the manifest
/// tier, the triggering source file for the import-scan fallback), never the
/// refused name — reusing content-derived identity as a diagnostic label
/// would re-exfiltrate exactly what the gate just refused. Mirrors `khive-pack-git`'s full-report masking assertion
/// (`crates/khive-pack-git/tests/acceptance.rs`, `writes_refused` case).
#[tokio::test]
async fn gate_blocked_project_name_reports_safe_manifest_path() {
    let root = TempDir::new().expect("tempdir");
    write_gate_blocked_project_name_fixture(root.path());
    let db = root.path().join("gate_blocked_name.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let report = run_code_ingest(
        &rt,
        &token,
        CodeSourceIngestOptions {
            path: root.path(),
            languages: all_languages(),
            sweep_time: Utc::now(),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("ingest must complete despite one gate-blocked write: {e}"));

    // The manifest-tier upsert and the import-scan-tier upsert each attempt
    // (and independently refuse) the same secret-shaped project name, so
    // both routes are quarantined, each labeled by its own trusted on-disk
    // location: the governing manifest file for the manifest tier, the
    // triggering source file for the import-scan fallback. Asserting the
    // exact pair proves BOTH routes carry a real file path, never the
    // refused name and never a bare directory.
    assert_eq!(
        report.blocked_count as usize,
        report.blocked.len(),
        "blocked_count must match the number of entries in blocked"
    );
    let expected_manifest = root
        .path()
        .join("pkg_secret")
        .join("Cargo.toml")
        .display()
        .to_string();
    let expected_source = root
        .path()
        .join("pkg_secret")
        .join("src")
        .join("lib.rs")
        .display()
        .to_string();
    let mut blocked_files: Vec<&str> = report.blocked.iter().map(|b| b.file.as_str()).collect();
    blocked_files.sort_unstable();
    let mut expected_files = vec![expected_manifest.as_str(), expected_source.as_str()];
    expected_files.sort_unstable();
    assert_eq!(
        blocked_files, expected_files,
        "blocked[].file must be exactly the manifest file (manifest tier) and the \
         triggering source file (import-scan fallback): {:?}",
        report.blocked
    );
    for entry in &report.blocked {
        assert_eq!(entry.detector, "url-userinfo");
        assert!(
            !entry.masked_excerpt.is_empty(),
            "masked_excerpt must be present"
        );
        assert!(
            !entry.masked_excerpt.contains("user:pass"),
            "masked excerpt must not echo the credential-shaped span: {}",
            entry.masked_excerpt
        );
    }

    let serialized = serde_json::to_string(&report).expect("CodeSourceIngestReport serializes");
    assert!(
        !serialized.contains("user:pass"),
        "the complete serialized report must never contain the refused credential-shaped \
         project name: {serialized}"
    );

    assert!(
        report.projects_created >= 1,
        "the unrelated sibling pkg_ok must still be created, got {} (report: {report:?})",
        report.projects_created
    );
    let names = entity_names(&rt).await;
    assert!(
        names.iter().any(|n| n == "pkg_ok"),
        "unrelated sibling pkg_ok must be ingested: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("user:pass")),
        "the gate-blocked project name must never be written as an entity: {names:?}"
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
