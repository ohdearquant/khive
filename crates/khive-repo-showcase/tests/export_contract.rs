use std::fs;
use std::path::Path;

use khive_repo_showcase::{
    canonical_bytes, export, Availability, BoundKind, CodeIngestL2Provenance, CodeIngestProvenance,
    DisclosureStatus, ExportBounds, ExportError, ExportRequest, GitDigestProvenance, Granularity,
    GraphEdge, HistorySourceCoverage, JoinTag, Page, PipelineProvenance, RepoBundle, SchemaVersion,
    ScorecardKey, ScorecardValue, SourceCoverage, SymbolPage,
};
use rusqlite::Connection;
use tempfile::TempDir;

fn schema(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE entities (
             id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
             name TEXT NOT NULL, description TEXT, properties TEXT, tags TEXT NOT NULL,
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER,
             entity_type TEXT, merged_into TEXT, merge_event_id TEXT
         );
         CREATE TABLE graph_edges (
             namespace TEXT NOT NULL, id TEXT NOT NULL, source_id TEXT NOT NULL,
             target_id TEXT NOT NULL, relation TEXT NOT NULL, weight REAL NOT NULL,
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER,
             metadata TEXT, target_backend TEXT,
             PRIMARY KEY(namespace,id)
         );
         CREATE TABLE notes (
             id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
             status TEXT NOT NULL, name TEXT, content TEXT NOT NULL, salience REAL,
             decay_factor REAL, expires_at INTEGER, properties TEXT,
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER
         );
         CREATE TABLE events (
             id TEXT PRIMARY KEY, namespace TEXT NOT NULL, verb TEXT NOT NULL,
             substrate TEXT NOT NULL, actor TEXT NOT NULL, outcome TEXT NOT NULL,
             data TEXT, duration_us INTEGER NOT NULL, target_id TEXT, created_at INTEGER NOT NULL,
             kind TEXT NOT NULL, payload TEXT NOT NULL, payload_schema_version INTEGER NOT NULL,
             profile_state_version INTEGER, session_id TEXT, aggregate_kind TEXT,
             aggregate_id TEXT
         );",
    )
    .unwrap();
}

fn insert_entity(
    conn: &Connection,
    id: &str,
    kind: &str,
    entity_type: Option<&str>,
    name: &str,
    properties: serde_json::Value,
) {
    conn.execute(
        "INSERT INTO entities
         (id,namespace,kind,name,properties,tags,created_at,updated_at,entity_type)
         VALUES (?1,'local',?2,?3,?4,'[]',1,1,?5)",
        rusqlite::params![id, kind, name, properties.to_string(), entity_type],
    )
    .unwrap();
}

fn insert_note(conn: &Connection, id: &str, kind: &str, name: &str, properties: serde_json::Value) {
    conn.execute(
        "INSERT INTO notes
         (id,namespace,kind,status,name,content,properties,created_at,updated_at)
         VALUES (?1,'local',?2,'active',?3,?3,?4,1,1)",
        rusqlite::params![id, kind, name, properties.to_string()],
    )
    .unwrap();
}

fn insert_edge(conn: &Connection, id: &str, source: &str, target: &str, relation: &str) {
    conn.execute(
        "INSERT INTO graph_edges
         (namespace,id,source_id,target_id,relation,weight,created_at,updated_at,metadata)
         VALUES ('local',?1,?2,?3,?4,1.0,1,1,'{}')",
        rusqlite::params![id, source, target, relation],
    )
    .unwrap();
}

fn insert_l2_edge(
    conn: &Connection,
    id: &str,
    source: &str,
    target: &str,
    relation: &str,
    metadata: serde_json::Value,
) {
    conn.execute(
        "INSERT INTO graph_edges
         (namespace,id,source_id,target_id,relation,weight,created_at,updated_at,metadata)
         VALUES ('local',?1,?2,?3,?4,1.0,1,1,?5)",
        rusqlite::params![id, source, target, relation, metadata.to_string()],
    )
    .unwrap();
}

fn entity_properties(conn: &Connection, id: &str) -> serde_json::Value {
    let raw: String = conn
        .query_row("SELECT properties FROM entities WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn set_entity_properties(conn: &Connection, id: &str, properties: serde_json::Value) {
    conn.execute(
        "UPDATE entities SET properties=?2 WHERE id=?1",
        rusqlite::params![id, properties.to_string()],
    )
    .unwrap();
}

fn stamp_module(request: &ExportRequest, module_id: &str, declaration_ids: serde_json::Value) {
    let conn = Connection::open(&request.map_db).unwrap();
    let mut properties = entity_properties(&conn, module_id);
    properties["declaration_ids"] = declaration_ids;
    set_entity_properties(&conn, module_id, properties);
}

fn request_head(request: &ExportRequest) -> String {
    match &request.provenance.code_ingest {
        Availability::Available { value } => value.source_revision.clone(),
        Availability::Unavailable { .. } => panic!("fixture code provenance is available"),
    }
}

fn attest_l2(request: &mut ExportRequest) {
    let head = request_head(request);
    let Availability::Available { value } = &mut request.provenance.code_ingest else {
        panic!("fixture code provenance is available");
    };
    value.l2 = Some(CodeIngestL2Provenance {
        source_revision: head,
        symbols_created: 0,
        symbols_updated: 0,
        symbol_dependencies_unresolved: 0,
        symbol_edges_stamped: 0,
        symbol_parse_failures: 0,
    });
}

fn attest_populated_l2(request: &mut ExportRequest, symbols_created: u64, edges_stamped: u64) {
    attest_l2(request);
    let Availability::Available { value } = &mut request.provenance.code_ingest else {
        unreachable!();
    };
    let l2 = value.l2.as_mut().unwrap();
    l2.symbols_created = symbols_created;
    l2.symbol_edges_stamped = edges_stamped;
}

fn insert_symbol(
    request: &ExportRequest,
    id: &str,
    entity_type: &str,
    name: &str,
    module_path: &str,
    source_revision: &str,
) {
    insert_symbol_at_path(
        request,
        id,
        entity_type,
        name,
        module_path,
        "src/lib.rs",
        source_revision,
    );
}

fn insert_symbol_at_path(
    request: &ExportRequest,
    id: &str,
    entity_type: &str,
    name: &str,
    module_path: &str,
    source_path: &str,
    source_revision: &str,
) {
    let conn = Connection::open(&request.map_db).unwrap();
    insert_entity(
        &conn,
        id,
        "concept",
        Some(entity_type),
        name,
        serde_json::json!({
            "source_project": "fixture-crate",
            "language": "rust",
            "module_path": module_path,
            "source_path": source_path,
            "source_revision": source_revision,
            "content_hash": format!("{entity_type}-{name}"),
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
}

fn assert_legacy_symbol_pages(bundle: &RepoBundle) {
    for page in [
        &bundle.graph.functions,
        &bundle.graph.datatypes,
        &bundle.graph.interfaces,
    ] {
        assert_eq!(page, &SymbolPage::empty());
        assert_eq!(page.bound.max_items, 0);
        assert_eq!(page.bound.order, "symbol_id");
    }
}

fn assert_available_zero(page: &SymbolPage, limit: u32) {
    assert!(page.items.is_empty());
    assert_eq!(page.total_count, Availability::available(0));
    assert_eq!(page.bound.kind, BoundKind::All);
    assert_eq!(page.bound.max_items, limit);
    assert_eq!(page.bound.order, "module_path,name,symbol_id");
    assert_eq!(page.next_cursor, None);
    assert!(!page.truncated);
    assert_eq!(page.disclosure.status, DisclosureStatus::Complete);
    assert_eq!(page.disclosure.reason, None);
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

fn fixture() -> (TempDir, ExportRequest) {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "fixture@example.com"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/lib.rs"), "pub mod api;\n").unwrap();
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let head = String::from_utf8(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();

    let history_db = dir.path().join("history.db");
    let history = Connection::open(&history_db).unwrap();
    schema(&history);
    insert_entity(
        &history,
        "10000000-0000-4000-8000-000000000001",
        "project",
        None,
        "owner/repo",
        serde_json::json!({"repo_slug":"github.com/owner/repo", "repo_url":"https://github.com/owner/repo"}),
    );
    insert_note(
        &history,
        "20000000-0000-4000-8000-000000000001",
        "commit",
        "initial",
        serde_json::json!({
            "sha": head,
            "short_sha": &head[..7],
            "author": "Fixture",
            "committed_at": "2026-08-07T00:00:00Z",
            "parents": [],
            "changed_paths": ["src/lib.rs", "src/main.rs"]
        }),
    );
    insert_edge(
        &history,
        "20000000-0000-4000-8000-000000000002",
        "20000000-0000-4000-8000-000000000001",
        "10000000-0000-4000-8000-000000000001",
        "annotates",
    );

    let map_db = dir.path().join("map.db");
    let map = Connection::open(&map_db).unwrap();
    schema(&map);
    insert_entity(
        &map,
        "30000000-0000-4000-8000-000000000001",
        "project",
        None,
        "fixture-crate",
        serde_json::json!({"source_project":"fixture-crate"}),
    );
    insert_entity(
        &map,
        "30000000-0000-4000-8000-000000000002",
        "concept",
        Some("module"),
        "crate",
        serde_json::json!({
            "source_project":"fixture-crate", "language":"rust", "module_path":"crate",
            "source_path":"src/lib.rs", "source_revision":head, "content_hash":"lib",
            "import_scan_status":"scanned"
        }),
    );
    insert_entity(
        &map,
        "30000000-0000-4000-8000-000000000003",
        "concept",
        Some("module"),
        "crate::main",
        serde_json::json!({
            "source_project":"fixture-crate", "language":"rust", "module_path":"crate::main",
            "source_path":"src/main.rs", "source_revision":head, "content_hash":"main",
            "import_scan_status":"scanned"
        }),
    );
    insert_edge(
        &map,
        "40000000-0000-4000-8000-000000000001",
        "30000000-0000-4000-8000-000000000001",
        "30000000-0000-4000-8000-000000000002",
        "contains",
    );
    insert_edge(
        &map,
        "40000000-0000-4000-8000-000000000002",
        "30000000-0000-4000-8000-000000000001",
        "30000000-0000-4000-8000-000000000003",
        "contains",
    );

    let request = ExportRequest {
        repo_path: repo,
        history_db,
        map_db,
        generated_at: "2099-01-01T00:00:00Z".to_string(),
        repository_url: "https://github.com/owner/repo".to_string(),
        bounds: ExportBounds::default(),
        provenance: PipelineProvenance {
            git_digest: Availability::available(GitDigestProvenance {
                calls: 1,
                history_exhausted: true,
                cursor_stalled: false,
                writes_refused: 0,
                changed_paths_filtered_noncanonical: 0,
                sources: HistorySourceCoverage {
                    commits: SourceCoverage::Completed,
                    issues: SourceCoverage::Skipped {
                        reason: "fixture has no forge source".into(),
                    },
                    pull_requests: SourceCoverage::Skipped {
                        reason: "fixture has no forge source".into(),
                    },
                },
            }),
            code_ingest: Availability::available(CodeIngestProvenance {
                source_revision: head,
                languages: vec!["rust".into()],
                blocked_count: 0,
                files_dropped_without_source_path: 0,
                files_skipped_without_module_path: 0,
                coverage_stamps_missed: 0,
                warnings_count: 0,
                l2: None,
            }),
            clone_tags: SourceCoverage::Completed,
        },
        default_branch: Availability::available("main".into()),
    };
    (dir, request)
}

fn fixture_with_l2_symbols() -> (TempDir, ExportRequest, RepoBundle) {
    let (dir, mut request) = fixture();
    let legacy = export(&request).unwrap();
    let head = request_head(&request);
    let function = "51000000-0000-4000-8000-000000000001";
    let datatype = "51000000-0000-4000-8000-000000000002";
    let interface = "51000000-0000-4000-8000-000000000003";

    insert_symbol_at_path(
        &request,
        function,
        "function",
        "load_record",
        "crate",
        "src/lib.rs",
        &head,
    );
    insert_symbol_at_path(
        &request,
        datatype,
        "datatype",
        "Record",
        "crate",
        "src/lib.rs",
        &head,
    );
    insert_symbol_at_path(
        &request,
        interface,
        "interface",
        "Loadable",
        "crate::main",
        "src/main.rs",
        &head,
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([function, datatype]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([interface]),
    );

    let conn = Connection::open(&request.map_db).unwrap();
    insert_l2_edge(
        &conn,
        "61000000-0000-4000-8000-000000000001",
        function,
        datatype,
        "depends_on",
        serde_json::json!({
            "l2_derived": true,
            "l2_evidence": ["call", "type_reference"],
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    insert_l2_edge(
        &conn,
        "61000000-0000-4000-8000-000000000002",
        datatype,
        interface,
        "implements",
        serde_json::json!({
            "l2_derived": true,
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    insert_l2_edge(
        &conn,
        "61000000-0000-4000-8000-000000000003",
        function,
        "51000000-0000-4000-8000-000000000099",
        "depends_on",
        serde_json::json!({
            "l2_derived": true,
            "l2_evidence": ["call"],
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    drop(conn);
    attest_populated_l2(&mut request, 3, 3);

    (dir, request, legacy)
}

#[test]
fn exact_source_path_join_keeps_lib_and_main_distinct() {
    let (_dir, request) = fixture();
    let bundle = export(&request).unwrap();
    assert_eq!(bundle.schema_version, SchemaVersion::KhiveRepoV1);
    assert_eq!(bundle.graph.commit_module_edges.items.len(), 2);
    let paths: Vec<_> = bundle
        .graph
        .modules
        .items
        .iter()
        .map(|module| (module.source_path.as_str(), module.module_path.as_str()))
        .collect();
    assert!(paths.contains(&("src/lib.rs", "crate")));
    assert!(paths.contains(&("src/main.rs", "crate::main")));
    assert!(bundle
        .graph
        .commit_module_edges
        .items
        .iter()
        .all(|edge| edge.origin.as_str() == "derived"));
}

#[test]
fn symbol_pages_and_unavailable_issue_facets_are_explicit() {
    let (_dir, request) = fixture();
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!(["missing-current-symbol"]),
    );
    let bundle = export(&request).unwrap();
    assert_legacy_symbol_pages(&bundle);
    // A deferred tier is not a measured zero. Every symbol page reports the same
    // way an unrequested history facet does, on both fields: an unavailable count
    // and an unavailable disclosure. Asserting only one of the two is how the
    // earlier `complete` + `available(0)` pairing survived this test.
    for page in [
        &bundle.graph.functions,
        &bundle.graph.datatypes,
        &bundle.graph.interfaces,
    ] {
        assert_eq!(page.disclosure.status, DisclosureStatus::Unavailable);
        assert!(matches!(page.total_count, Availability::Unavailable { .. }));
    }
    assert!(matches!(
        bundle.graph.issues.total_count,
        Availability::Unavailable { .. }
    ));
    assert!(bundle
        .capability
        .views
        .history_structure_navigation
        .status
        .is_available());
    assert!(matches!(
        bundle
            .capability
            .views
            .history_structure_navigation
            .issue_module_facet,
        Availability::Unavailable { .. }
    ));
}

#[test]
fn attested_empty_l2_is_available_and_complete() {
    let (_dir, mut request) = fixture();
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    attest_l2(&mut request);

    let bundle = export(&request).unwrap();

    for page in [
        &bundle.graph.functions,
        &bundle.graph.datatypes,
        &bundle.graph.interfaces,
    ] {
        assert_available_zero(page, 2_000);
    }
}

#[test]
fn populated_l2_fixture_keeps_module_graph_and_aggregates_stable() {
    let (_dir, request, legacy) = fixture_with_l2_symbols();
    let populated = export(&request).unwrap();

    assert_eq!(populated.graph.functions.items.len(), 1);
    assert_eq!(populated.graph.datatypes.items.len(), 1);
    assert_eq!(populated.graph.interfaces.items.len(), 1);
    let function = &populated.graph.functions.items[0];
    let datatype = &populated.graph.datatypes.items[0];
    let interface = &populated.graph.interfaces.items[0];
    assert_eq!(function.kind, "function");
    assert_eq!(datatype.kind, "datatype");
    assert_eq!(interface.kind, "interface");
    assert_eq!(function.module_id, datatype.module_id);
    assert_ne!(function.module_id, interface.module_id);
    assert_eq!(function.outgoing_call_edge_count, 1);
    assert_eq!(function.outgoing_type_reference_edge_count, 1);
    assert_eq!(interface.incoming_implements_edge_count, 1);

    for page in [
        &populated.graph.functions,
        &populated.graph.datatypes,
        &populated.graph.interfaces,
    ] {
        assert_eq!(page.total_count, Availability::available(1));
        assert_eq!(page.bound.kind, BoundKind::All);
        assert_eq!(page.bound.max_items, 2_000);
        assert_eq!(page.bound.order, "module_path,name,symbol_id");
        assert!(!page.truncated);
        assert_eq!(page.disclosure.status, DisclosureStatus::Complete);
    }

    let map = Connection::open(&request.map_db).unwrap();
    assert_eq!(
        entity_properties(&map, "30000000-0000-4000-8000-000000000002")["declaration_ids"],
        serde_json::json!([
            "51000000-0000-4000-8000-000000000001",
            "51000000-0000-4000-8000-000000000002"
        ])
    );
    assert_eq!(
        entity_properties(&map, "30000000-0000-4000-8000-000000000003")["declaration_ids"],
        serde_json::json!(["51000000-0000-4000-8000-000000000003"])
    );

    assert_eq!(
        serde_json::to_vec(&populated.graph.structure_edges).unwrap(),
        serde_json::to_vec(&legacy.graph.structure_edges).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&populated.aggregates).unwrap(),
        serde_json::to_vec(&legacy.aggregates).unwrap()
    );
    assert_eq!(
        populated.capability.views.structure_graph.granularity,
        Granularity::ModuleSymbolDeferred
    );
    let symbol_count = populated
        .aggregates
        .scorecard
        .fields
        .iter()
        .find(|field| field.key == ScorecardKey::SymbolCount)
        .unwrap();
    assert_eq!(symbol_count.granularity, Granularity::ModuleSymbolDeferred);
    assert!(matches!(
        &symbol_count.value,
        Availability::Unavailable { reason }
            if reason == "symbol-tier ingest is deferred"
    ));
}

#[test]
fn unresolved_symbol_dependencies_are_nonfatal() {
    let (_dir, mut request) = fixture();
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    attest_l2(&mut request);
    let Availability::Available { value } = &mut request.provenance.code_ingest else {
        unreachable!();
    };
    value.l2.as_mut().unwrap().symbol_dependencies_unresolved = 1;

    let bundle = export(&request).unwrap();

    for page in [
        &bundle.graph.functions,
        &bundle.graph.datatypes,
        &bundle.graph.interfaces,
    ] {
        assert_available_zero(page, 2_000);
    }
}

#[test]
fn nested_l2_revision_must_equal_head() {
    let (_dir, mut request) = fixture();
    attest_l2(&mut request);
    let Availability::Available { value } = &mut request.provenance.code_ingest else {
        unreachable!();
    };
    value.l2.as_mut().unwrap().source_revision = "0".repeat(40);
    let head = value.source_revision.clone();

    let error = export(&request).unwrap_err();

    assert!(matches!(
        error,
        ExportError::InvalidData(message)
            if message == format!("code.ingest l2 source_revision {} does not equal HEAD {head}", "0".repeat(40))
    ));
}

#[test]
fn attested_l2_requires_complete_ingest_coverage() {
    let (_dir, mut request) = fixture();
    attest_l2(&mut request);
    let Availability::Available { value } = &mut request.provenance.code_ingest else {
        unreachable!();
    };
    value.blocked_count = 1;
    value.files_dropped_without_source_path = 2;
    value.files_skipped_without_module_path = 3;
    value.coverage_stamps_missed = 4;
    value.l2.as_mut().unwrap().symbol_parse_failures = 5;

    let error = export(&request).unwrap_err();

    assert!(matches!(
        error,
        ExportError::InvalidData(message)
            if message == "code.ingest l2 coverage is incomplete: blocked=1, dropped_without_source_path=2, skipped_without_module_path=3, coverage_stamps_missed=4, symbol_parse_failures=5"
    ));
}

#[test]
fn malformed_declaration_ownership_is_rejected_exactly() {
    for (declaration_ids, expected) in [
        (
            None,
            "module 30000000-0000-4000-8000-000000000002 has no declaration_ids array for attested l2",
        ),
        (
            Some(serde_json::json!({})),
            "module 30000000-0000-4000-8000-000000000002 declaration_ids must be an array",
        ),
        (
            Some(serde_json::json!([null])),
            "module 30000000-0000-4000-8000-000000000002 declaration_ids[0] must be a non-empty string",
        ),
        (
            Some(serde_json::json!(["duplicate", "duplicate"])),
            "module 30000000-0000-4000-8000-000000000002 declaration_ids contains duplicate symbol duplicate",
        ),
    ] {
        let (_dir, mut request) = fixture();
        if let Some(declaration_ids) = declaration_ids {
            stamp_module(
                &request,
                "30000000-0000-4000-8000-000000000002",
                declaration_ids,
            );
        }
        stamp_module(
            &request,
            "30000000-0000-4000-8000-000000000003",
            serde_json::json!([]),
        );
        attest_l2(&mut request);

        let error = export(&request).unwrap_err();
        let ExportError::InvalidData(message) = error else {
            panic!("expected invalid data, got {error:?}");
        };

        assert_eq!(message, expected);
    }
}

#[test]
fn a_symbol_cannot_be_claimed_by_multiple_modules() {
    let (_dir, mut request) = fixture();
    for module_id in [
        "30000000-0000-4000-8000-000000000002",
        "30000000-0000-4000-8000-000000000003",
    ] {
        stamp_module(&request, module_id, serde_json::json!(["shared-symbol"]));
    }
    attest_l2(&mut request);

    let error = export(&request).unwrap_err();

    assert!(matches!(
        error,
        ExportError::InvalidData(message)
            if message == "symbol shared-symbol is owned by modules 30000000-0000-4000-8000-000000000002 and 30000000-0000-4000-8000-000000000003"
    ));
}

#[test]
fn missing_owned_symbol_rows_report_the_exact_module_error() {
    let (_dir, mut request) = fixture();
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!(["missing-a", "missing-b"]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    attest_l2(&mut request);

    let error = export(&request).unwrap_err();

    assert!(matches!(
        error,
        ExportError::InvalidData(message)
            if message == "module 30000000-0000-4000-8000-000000000002 declaration_ids reference 2 missing symbol row(s)"
    ));
}

#[test]
fn deleted_owned_symbol_rows_are_reported_as_missing() {
    let (_dir, mut request) = fixture();
    let symbol_id = "50000000-0000-4000-8000-000000000001";
    let head = request_head(&request);
    insert_symbol(
        &request,
        symbol_id,
        "function",
        "deleted_symbol",
        "crate",
        &head,
    );
    let conn = Connection::open(&request.map_db).unwrap();
    conn.execute("UPDATE entities SET deleted_at=2 WHERE id=?1", [symbol_id])
        .unwrap();
    drop(conn);
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([symbol_id]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    attest_populated_l2(&mut request, 1, 0);

    let error = export(&request).unwrap_err();

    assert!(matches!(
        error,
        ExportError::InvalidData(message)
            if message == "module 30000000-0000-4000-8000-000000000002 declaration_ids reference 1 missing symbol row(s)"
    ));
}

#[test]
fn owned_symbol_revision_mismatch_reports_the_exact_module_error() {
    let (_dir, mut request) = fixture();
    let symbol_id = "50000000-0000-4000-8000-000000000001";
    let old_revision = "0".repeat(40);
    insert_symbol(
        &request,
        symbol_id,
        "function",
        "old_symbol",
        "crate",
        &old_revision,
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([symbol_id]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    attest_l2(&mut request);
    let expected = request_head(&request);

    let error = export(&request).unwrap_err();

    assert!(matches!(
        error,
        ExportError::InvalidData(message)
            if message == format!(
                "module 30000000-0000-4000-8000-000000000002 symbol {symbol_id} has source_revision {old_revision}, expected {expected}"
            )
    ));
}

#[test]
fn owned_symbol_identity_must_match_its_module() {
    let (_dir, mut request) = fixture();
    let symbol_id = "50000000-0000-4000-8000-000000000001";
    let head = request_head(&request);
    insert_symbol(
        &request,
        symbol_id,
        "function",
        "misowned_symbol",
        "crate",
        &head,
    );
    let conn = Connection::open(&request.map_db).unwrap();
    let mut properties = entity_properties(&conn, symbol_id);
    properties["source_project"] = serde_json::json!("other-project");
    set_entity_properties(&conn, symbol_id, properties);
    drop(conn);
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([symbol_id]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    attest_l2(&mut request);

    let error = export(&request).unwrap_err();

    assert!(matches!(
        error,
        ExportError::InvalidData(message)
            if message == format!(
                "module 30000000-0000-4000-8000-000000000002 symbol {symbol_id} has source_project \"other-project\", expected \"fixture-crate\""
            )
    ));
}

#[test]
fn current_symbol_edges_require_recognized_evidence() {
    let (_dir, mut request) = fixture();
    let head = request_head(&request);
    let source = "50000000-0000-4000-8000-000000000001";
    let target = "50000000-0000-4000-8000-000000000002";
    insert_symbol(&request, source, "function", "source", "crate", &head);
    insert_symbol(&request, target, "function", "target", "crate", &head);
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([source, target]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    let conn = Connection::open(&request.map_db).unwrap();
    let edge_id = "60000000-0000-4000-8000-000000000001";
    insert_l2_edge(
        &conn,
        edge_id,
        source,
        target,
        "depends_on",
        serde_json::json!({
            "l2_derived": true,
            "l2_evidence": ["unknown"],
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    drop(conn);
    attest_populated_l2(&mut request, 2, 1);

    let error = export(&request).unwrap_err();

    assert!(matches!(
        error,
        ExportError::InvalidData(message)
            if message == format!(
                "current depends_on edge {edge_id} requires a nonempty recognized l2_evidence array"
            )
    ));
}

#[test]
fn stale_unowned_symbols_do_not_change_attested_zero_pages() {
    let (_dir, mut request) = fixture();
    insert_symbol(
        &request,
        "50000000-0000-4000-8000-000000000099",
        "function",
        "historical_symbol",
        "crate",
        &"0".repeat(40),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    attest_l2(&mut request);

    let bundle = export(&request).unwrap();

    for page in [
        &bundle.graph.functions,
        &bundle.graph.datatypes,
        &bundle.graph.interfaces,
    ] {
        assert_available_zero(page, 2_000);
    }
}

#[test]
fn populated_symbol_pages_sort_bound_and_count_private_edges() {
    let (_dir, mut request) = fixture();
    let head = request_head(&request);
    let function_outer = "50000000-0000-4000-8000-000000000001";
    let function_inner = "50000000-0000-4000-8000-000000000002";
    let datatype = "50000000-0000-4000-8000-000000000003";
    let interface = "50000000-0000-4000-8000-000000000004";
    let inline_module = "50000000-0000-4000-8000-000000000005";
    for (id, entity_type, name, module_path) in [
        (function_outer, "function", "zeta", "crate"),
        (function_inner, "function", "alpha", "crate::inner"),
        (datatype, "datatype", "Record", "crate"),
        (interface, "interface", "Contract", "crate"),
        (inline_module, "module", "inner", "crate"),
    ] {
        insert_symbol(&request, id, entity_type, name, module_path, &head);
    }
    insert_symbol(
        &request,
        "50000000-0000-4000-8000-000000000099",
        "function",
        "historical_symbol",
        "crate",
        &"0".repeat(40),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000002",
        serde_json::json!([
            function_outer,
            function_inner,
            datatype,
            interface,
            inline_module
        ]),
    );
    stamp_module(
        &request,
        "30000000-0000-4000-8000-000000000003",
        serde_json::json!([]),
    );
    let conn = Connection::open(&request.map_db).unwrap();
    insert_l2_edge(
        &conn,
        "60000000-0000-4000-8000-000000000001",
        function_outer,
        function_inner,
        "depends_on",
        serde_json::json!({
            "l2_derived": true,
            "l2_evidence": ["call", "type_reference"],
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    insert_l2_edge(
        &conn,
        "60000000-0000-4000-8000-000000000004",
        function_outer,
        function_inner,
        "depends_on",
        serde_json::json!({
            "l2_derived": true,
            "l2_evidence": ["call"],
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    insert_l2_edge(
        &conn,
        "60000000-0000-4000-8000-000000000005",
        function_outer,
        function_inner,
        "depends_on",
        serde_json::json!({
            "l2_derived": true,
            "l2_evidence": ["type_reference"],
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    insert_l2_edge(
        &conn,
        "60000000-0000-4000-8000-000000000006",
        function_outer,
        function_inner,
        "depends_on",
        serde_json::json!({
            "l2_derived": true,
            "l2_evidence": ["unknown"],
            "language": "rust",
            "last_seen_at": "2026-08-06T00:00:00Z"
        }),
    );
    insert_l2_edge(
        &conn,
        "60000000-0000-4000-8000-000000000002",
        datatype,
        interface,
        "implements",
        serde_json::json!({
            "l2_derived": true,
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    insert_l2_edge(
        &conn,
        "60000000-0000-4000-8000-000000000003",
        function_outer,
        "50000000-0000-4000-8000-000000000099",
        "depends_on",
        serde_json::json!({
            "l2_derived": true,
            "l2_evidence": ["call"],
            "language": "rust",
            "last_seen_at": "2026-08-07T00:00:00Z"
        }),
    );
    drop(conn);
    attest_populated_l2(&mut request, 5, 6);
    request.bounds.symbols_per_kind = 1;

    let first = export(&request).unwrap();
    let second = export(&request).unwrap();

    assert_eq!(
        canonical_bytes(&first).unwrap(),
        canonical_bytes(&second).unwrap()
    );
    let functions = &first.graph.functions;
    assert_eq!(functions.items.len(), 1);
    assert_eq!(functions.items[0].name, "zeta");
    assert_eq!(functions.items[0].module_path, "crate");
    assert_eq!(functions.items[0].kind, "function");
    assert_eq!(functions.items[0].outgoing_call_edge_count, 2);
    assert_eq!(functions.items[0].outgoing_type_reference_edge_count, 2);
    assert_eq!(functions.total_count, Availability::available(2));
    assert_eq!(functions.bound.kind, BoundKind::TopN);
    assert_eq!(functions.bound.max_items, 1);
    assert_eq!(functions.bound.order, "module_path,name,symbol_id");
    assert_eq!(functions.next_cursor.as_deref(), Some("offset:1"));
    assert!(functions.truncated);
    assert_eq!(functions.disclosure.status, DisclosureStatus::Truncated);
    assert_eq!(
        functions.disclosure.reason.as_deref(),
        Some("section limited to 1 items")
    );

    let datatypes = &first.graph.datatypes;
    assert_eq!(datatypes.total_count, Availability::available(1));
    assert_eq!(datatypes.items[0].name, "Record");
    assert_eq!(datatypes.bound.kind, BoundKind::All);
    assert_eq!(datatypes.disclosure.status, DisclosureStatus::Complete);

    let interfaces = &first.graph.interfaces;
    assert_eq!(interfaces.total_count, Availability::available(1));
    assert_eq!(interfaces.items[0].name, "Contract");
    assert_eq!(interfaces.items[0].incoming_implements_edge_count, 1);
    assert_eq!(interfaces.bound.kind, BoundKind::All);
    assert_eq!(interfaces.disclosure.status, DisclosureStatus::Complete);
    assert_eq!(first.graph.modules.total_count, Availability::available(2));
    assert!(first
        .graph
        .modules
        .items
        .iter()
        .all(|module| module.module_path != "inner"));

    assert!(first
        .graph
        .structure_edges
        .items
        .iter()
        .all(|edge| edge.relation == "contains"));
    let symbol_count = first
        .aggregates
        .scorecard
        .fields
        .iter()
        .find(|field| field.key == ScorecardKey::SymbolCount)
        .unwrap();
    assert!(matches!(
        &symbol_count.value,
        Availability::Unavailable { reason }
            if reason == "symbol-tier ingest is deferred"
    ));
    assert!(!serde_json::to_string(&first)
        .unwrap()
        .contains("historical_symbol"));
}

#[test]
fn view_tags_bind_all_ten_views() {
    let (_dir, request) = fixture();
    let bundle = export(&request).unwrap();
    assert_eq!(
        bundle.capability.views.cadence_timeline.granularity,
        Granularity::Repository
    );
    assert_eq!(
        bundle.capability.views.cadence_timeline.join,
        JoinTag::HistoryOnly
    );
    assert_eq!(bundle.capability.views.hotspot_quadrant.join, JoinTag::Join);
    assert_eq!(
        bundle.capability.views.structure_graph.join,
        JoinTag::StructureOnly
    );
    assert_eq!(bundle.capability.views.scorecard.join, JoinTag::FieldTagged);
}

#[test]
fn canonical_bytes_are_stable_and_round_trip_strictly() {
    let (_dir, request) = fixture();
    let first = export(&request).unwrap();
    let second = export(&request).unwrap();
    let a = canonical_bytes(&first).unwrap();
    let b = canonical_bytes(&second).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.last(), Some(&b'\n'));
    let mut expected = serde_json::to_vec(&first).unwrap();
    expected.push(b'\n');
    assert_eq!(a, expected, "canonical JSON is compact plus one newline");
    let decoded: RepoBundle = serde_json::from_slice(&a).unwrap();
    assert_eq!(decoded, first);
    let validator = jsonschema::validator_for(&khive_repo_showcase::json_schema()).unwrap();
    let instance = serde_json::to_value(&first).unwrap();
    let errors = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema errors: {errors:#?}");

    let mut value: serde_json::Value = serde_json::from_slice(&a).unwrap();
    value["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<RepoBundle>(value).is_err());

    let mut value: serde_json::Value = serde_json::from_slice(&a).unwrap();
    value["schema_version"] = serde_json::json!("khive.repo.v2");
    assert!(serde_json::from_value::<RepoBundle>(value).is_err());

    let mut value: serde_json::Value = serde_json::from_slice(&a).unwrap();
    value["meta"]["snapshot"]["ingested_at"] = serde_json::json!("not-a-timestamp");
    assert!(serde_json::from_value::<RepoBundle>(value).is_err());
}

#[test]
fn default_aggregate_bound_and_scorecard_module_ids_are_bounded() {
    assert_eq!(ExportBounds::default().aggregate_rows, 1_000);
    let (_dir, request) = fixture();
    let bundle = export(&request).unwrap();
    for key in [ScorecardKey::TopHotspots, ScorecardKey::OwnershipWarnings] {
        let field = bundle
            .aggregates
            .scorecard
            .fields
            .iter()
            .find(|field| field.key == key)
            .unwrap();
        let Availability::Available {
            value: ScorecardValue::ModuleIds { value },
        } = &field.value
        else {
            panic!("expected available bounded module ids for {key:?}");
        };
        assert!(value.items.len() <= value.bound.max_items as usize);
        assert!(matches!(value.total_count, Availability::Available { .. }));
    }
}

#[test]
fn edge_provenance_invariant_is_enforced_by_rust_and_schema() {
    let invalid = serde_json::json!({
        "id": "edge",
        "source": "source",
        "target": "target",
        "relation": "contains",
        "weight": 1.0,
        "origin": "derived",
        "derivation": null
    });
    assert!(serde_json::from_value::<GraphEdge>(invalid.clone()).is_err());
    let mut missing_nullable_field = invalid.clone();
    missing_nullable_field["origin"] = serde_json::json!("ingested");
    missing_nullable_field
        .as_object_mut()
        .unwrap()
        .remove("derivation");
    assert!(serde_json::from_value::<GraphEdge>(missing_nullable_field).is_err());

    let schema = khive_repo_showcase::json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let (_dir, request) = fixture();
    let mut bundle = serde_json::to_value(export(&request).unwrap()).unwrap();
    bundle["graph"]["structure_edges"]["items"][0]["origin"] = serde_json::json!("derived");
    bundle["graph"]["structure_edges"]["items"][0]["derivation"] = serde_json::Value::Null;
    assert!(!validator.is_valid(&bundle));
}

#[test]
fn completed_empty_is_distinct_from_skipped() {
    let (_dir, mut request) = fixture();
    if let Availability::Available { value } = &mut request.provenance.git_digest {
        value.sources.issues = SourceCoverage::Completed;
    } else {
        panic!("fixture supplies git.digest provenance");
    }
    let completed = export(&request).unwrap();
    assert!(matches!(
        completed.graph.issues.total_count,
        Availability::Available { value: 0 }
    ));
    assert_eq!(
        completed.graph.issues.disclosure.status,
        DisclosureStatus::Complete
    );
    assert!(completed
        .aggregates
        .cadence_timeline
        .issues_opened
        .items
        .is_empty());
    assert!(matches!(
        completed
            .aggregates
            .cadence_timeline
            .issues_opened
            .total_count,
        Availability::Available { value: 0 }
    ));

    if let Availability::Available { value } = &mut request.provenance.git_digest {
        value.sources.issues = SourceCoverage::Skipped {
            reason: "forge source disabled".into(),
        };
    }
    let skipped = export(&request).unwrap();
    assert!(matches!(
        skipped.graph.issues.total_count,
        Availability::Unavailable { .. }
    ));
    assert_eq!(
        skipped.graph.issues.disclosure.status,
        DisclosureStatus::Unavailable
    );
    assert_eq!(
        skipped
            .aggregates
            .cadence_timeline
            .issues_opened
            .disclosure
            .status,
        DisclosureStatus::Unavailable
    );
}

#[test]
fn completed_source_with_missing_required_timestamp_marks_only_that_series_unavailable() {
    let (_dir, mut request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    insert_note(
        &history,
        "20000000-0000-4000-8000-000000000010",
        "issue",
        "timestamp missing",
        serde_json::json!({"number": 10, "title": "timestamp missing"}),
    );
    insert_edge(
        &history,
        "20000000-0000-4000-8000-000000000011",
        "20000000-0000-4000-8000-000000000010",
        "10000000-0000-4000-8000-000000000001",
        "annotates",
    );
    if let Availability::Available { value } = &mut request.provenance.git_digest {
        value.sources.issues = SourceCoverage::Completed;
    }

    let bundle = export(&request).unwrap();
    assert_eq!(
        bundle
            .aggregates
            .cadence_timeline
            .issues_opened
            .disclosure
            .status,
        DisclosureStatus::Unavailable
    );
    assert_eq!(
        bundle
            .aggregates
            .cadence_timeline
            .issues_closed
            .disclosure
            .status,
        DisclosureStatus::Complete
    );
}

#[test]
fn zero_history_ownership_metrics_are_unavailable_not_zero() {
    let (_dir, request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    history.execute("DELETE FROM graph_edges", []).unwrap();
    history.execute("DELETE FROM notes", []).unwrap();

    let bundle = export(&request).unwrap();
    assert!(matches!(
        bundle.aggregates.ownership.repository_author_concentration,
        Availability::Unavailable { .. }
    ));
    assert!(matches!(
        bundle.aggregates.ownership.repository_bus_factor,
        Availability::Unavailable { .. }
    ));
    assert!(bundle
        .aggregates
        .ownership
        .modules
        .items
        .iter()
        .all(
            |row| matches!(row.author_concentration, Availability::Unavailable { .. })
                && matches!(row.bus_factor, Availability::Unavailable { .. })
        ));
}

#[test]
fn duplicate_history_identity_and_short_sha_mismatch_are_rejected() {
    let (_dir, request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    let properties: String = history
        .query_row(
            "SELECT properties FROM notes WHERE kind='commit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut properties: serde_json::Value = serde_json::from_str(&properties).unwrap();
    properties["short_sha"] = serde_json::json!("deadbee");
    history
        .execute(
            "UPDATE notes SET properties=?1 WHERE kind='commit'",
            [properties.to_string()],
        )
        .unwrap();
    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("short_sha"), "{error}");

    properties["short_sha"] =
        serde_json::json!(properties["sha"].as_str().unwrap().get(..7).unwrap());
    history
        .execute(
            "UPDATE notes SET properties=?1 WHERE kind='commit'",
            [properties.to_string()],
        )
        .unwrap();
    insert_note(
        &history,
        "20000000-0000-4000-8000-000000000020",
        "commit",
        "duplicate",
        properties,
    );
    insert_edge(
        &history,
        "20000000-0000-4000-8000-000000000021",
        "20000000-0000-4000-8000-000000000020",
        "10000000-0000-4000-8000-000000000001",
        "annotates",
    );
    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("duplicate commit SHA"), "{error}");
}

#[test]
fn ambiguous_exact_history_project_is_rejected() {
    let (_dir, request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    insert_entity(
        &history,
        "10000000-0000-4000-8000-000000000099",
        "project",
        None,
        "owner/repo-copy",
        serde_json::json!({"repo_slug":"github.com/owner/repo"}),
    );
    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("matched 2 history projects"), "{error}");
}

#[test]
fn negative_pull_request_lead_time_is_rejected() {
    let (_dir, mut request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    insert_note(
        &history,
        "20000000-0000-4000-8000-000000000030",
        "pull_request",
        "time travel",
        serde_json::json!({
            "number": 30,
            "created_at": "2026-08-08T00:00:00Z",
            "merged_at": "2026-08-07T00:00:00Z"
        }),
    );
    insert_edge(
        &history,
        "20000000-0000-4000-8000-000000000031",
        "20000000-0000-4000-8000-000000000030",
        "10000000-0000-4000-8000-000000000001",
        "annotates",
    );
    if let Availability::Available { value } = &mut request.provenance.git_digest {
        value.sources.pull_requests = SourceCoverage::Completed;
    }
    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("precedes creation time"), "{error}");
}

#[test]
fn release_tag_that_does_not_resolve_to_a_commit_is_rejected() {
    let (_dir, request) = fixture();
    fs::write(request.repo_path.join("tag-target.txt"), "not a commit\n").unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&request.repo_path)
        .args(["hash-object", "-w", "tag-target.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let object = String::from_utf8(output.stdout).unwrap();
    git(
        &request.repo_path,
        &["tag", "invalid-target", object.trim()],
    );

    let error = export(&request).unwrap_err().to_string();
    assert!(error.contains("rather than a commit"), "{error}");
}

#[test]
fn release_tag_uses_the_target_commit_identity_and_timestamp() {
    let (_dir, request) = fixture();
    git(&request.repo_path, &["tag", "v1", "HEAD"]);
    let head = String::from_utf8(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&request.repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();

    let bundle = export(&request).unwrap();
    assert_eq!(
        bundle.aggregates.cadence_timeline.release_tags.items.len(),
        1
    );
    let tag = &bundle.aggregates.cadence_timeline.release_tags.items[0];
    assert_eq!(tag.target_sha, head.trim());
    assert!(matches!(tag.committed_at, Availability::Available { .. }));
}

#[test]
fn checked_in_schema_matches_rust_model() {
    let schema_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/schemas/khive-repo-v1.schema.json");
    let checked_in = fs::read(schema_path).unwrap();
    let mut generated = serde_json::to_vec_pretty(&khive_repo_showcase::json_schema()).unwrap();
    generated.push(b'\n');
    assert_eq!(checked_in, generated);
}

#[test]
fn unknown_provenance_marks_structure_and_join_views_unavailable() {
    let (_dir, mut request) = fixture();
    request.provenance = PipelineProvenance::unknown("fixture revision");
    let bundle = export(&request).unwrap();

    assert_legacy_symbol_pages(&bundle);

    assert_eq!(
        bundle.graph.modules.disclosure.status,
        DisclosureStatus::Unavailable
    );
    assert_eq!(
        bundle.graph.commit_module_edges.disclosure.status,
        DisclosureStatus::Unavailable
    );
    assert!(!bundle.capability.languages.rust.measured);
    assert!(!bundle.capability.languages.rust.module_join);
    assert!(!bundle
        .capability
        .views
        .structure_graph
        .status
        .is_available());
    assert!(!bundle
        .capability
        .views
        .hotspot_quadrant
        .status
        .is_available());
    assert!(!bundle
        .capability
        .views
        .hidden_coupling
        .status
        .is_available());
    assert!(!bundle.capability.views.ownership.status.is_available());
    assert_eq!(
        bundle.graph.history_navigation.by_module.disclosure.status,
        DisclosureStatus::Unavailable
    );
}

#[test]
fn every_page_discloses_its_own_truncation() {
    let (_dir, mut request) = fixture();
    request.bounds.commits = 0;
    let bundle = export(&request).unwrap();
    assert!(bundle.graph.commits.items.is_empty());
    assert!(bundle.graph.commits.truncated);
    assert_eq!(
        bundle.graph.commits.disclosure.status,
        DisclosureStatus::Truncated
    );
    assert!(matches!(
        bundle.graph.commits.total_count,
        Availability::Available { value: 1 }
    ));

    let _: &Page<_> = &bundle.graph.modules;
    assert!(!bundle.graph.modules.truncated);
}
