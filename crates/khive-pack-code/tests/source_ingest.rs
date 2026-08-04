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
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::json;
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

/// Issue #1590: `code.ingest` writes an ordinary khive map database, so a
/// successful ingest must populate the FTS documents that the generic KG
/// `search` verb reads. Entity rows without these documents made exact-name
/// searches return an indistinguishable empty result.
#[tokio::test]
async fn ingested_map_entities_are_visible_to_generic_search() {
    let root = TempDir::new().expect("tempdir");
    write_two_package_fixture(root.path());
    let db = root.path().join("searchable.db");
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
    .expect("source ingest succeeds");
    assert!(
        report.fts_indexed > 0,
        "successful report must expose populated FTS work: {report:?}"
    );

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt));
    let registry = builder.build().expect("kg registry builds");
    let result = registry
        .dispatch("search", json!({"kind": "entity", "query": "pkg_a"}))
        .await
        .expect("generic search against map succeeds");
    let hits = result.as_array().expect("search returns an array");
    assert!(
        hits.iter().any(|hit| hit["name"] == "pkg_a"),
        "exact ingested project name must be searchable; got {hits:?}"
    );
}

/// `src/lib.rs` importing `crate::foo::Thing`, an item declared in
/// `src/foo.rs`, must resolve to a `crate -> foo` `depends_on` edge with
/// `dependency_kinds=["import"]` — not stay unresolved because the raw
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
        edges.iter().any(|(rel, src, tgt, kinds)| rel == "depends_on"
            && src == "crate"
            && tgt == "foo"
            && kinds == "import"),
        "expected one crate -> foo depends_on edge with dependency_kinds=[\"import\"], got: {edges:?}"
    );
}

/// Relative imports declared in a package `__init__.py` must resolve inside
/// that package, not its parent (#1662): `module_path_for_file` collapses
/// `pkg/x/__init__.py` to `pkg.x`, so the single leading dot already names
/// the package itself. Before the fix, `from .x import A` in
/// `pkg/x/__init__.py` resolved to `pkg.x` — a self-loop — and every other
/// `__init__` relative import landed one package too high.
fn write_python_init_reexport_fixture(root: &Path) {
    let pkg_x = root.join("pkg/x");
    std::fs::create_dir_all(&pkg_x).unwrap();
    std::fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"pyproj\"\n",
    )
    .unwrap();
    std::fs::write(root.join("pkg/__init__.py"), "").unwrap();
    std::fs::write(
        pkg_x.join("__init__.py"),
        "from .x import A\nfrom .y import B\n",
    )
    .unwrap();
    std::fs::write(pkg_x.join("x.py"), "A = 1\n").unwrap();
    std::fs::write(pkg_x.join("y.py"), "B = 2\n").unwrap();
    std::fs::write(pkg_x.join("z.py"), "from .y import B\n").unwrap();
}

#[tokio::test]
async fn python_init_reexport_resolves_in_package_without_self_loop() {
    let root = TempDir::new().expect("tempdir");
    write_python_init_reexport_fixture(root.path());
    let db = root.path().join("init_reexport.db");
    let rt = rt_at(&db);
    let token = rt.authorize(Namespace::local()).expect("token");

    let opts = || CodeSourceIngestOptions {
        path: root.path(),
        languages: all_languages(),
        sweep_time: Utc::now(),
    };
    run_code_ingest(&rt, &token, opts())
        .await
        .expect("first ingest succeeds");
    run_code_ingest(&rt, &token, opts())
        .await
        .expect("second ingest succeeds");

    let edges = edge_fingerprints(&rt).await;
    for (src, tgt) in [
        ("pkg.x", "pkg.x.x"),
        ("pkg.x", "pkg.x.y"),
        ("pkg.x.z", "pkg.x.y"),
    ] {
        assert!(
            edges.iter().any(|(rel, s, t, kinds)| rel == "depends_on"
                && s == src
                && t == tgt
                && kinds == "import"),
            "expected {src} -> {tgt} depends_on import edge, got: {edges:?}"
        );
    }
    let self_loops: Vec<_> = edges.iter().filter(|(_, s, t, _)| s == t).collect();
    assert!(
        self_loops.is_empty(),
        "same-name submodule re-export must not produce self-loop edges, got: {self_loops:?}"
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
            .any(|(rel, src, _tgt, _kinds)| rel == "contains" && src == "bare_rust_project"),
        "expected the basename-fallback project 'bare_rust_project' to contain its module, got: {edges:?}"
    );
    assert!(
        edges.iter().any(|(rel, src, tgt, kinds)| rel == "depends_on"
            && src == "crate"
            && tgt == "util"
            && kinds == "import"),
        "expected one crate -> util depends_on edge with dependency_kinds=[\"import\"], got: {edges:?}"
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
