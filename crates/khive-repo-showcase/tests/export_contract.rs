use std::fs;
use std::path::Path;

use khive_repo_showcase::{
    canonical_bytes, export, Availability, CodeIngestProvenance, DisclosureStatus, ExportBounds,
    ExportRequest, GitDigestProvenance, Granularity, GraphEdge, HistorySourceCoverage, JoinTag,
    Page, PipelineProvenance, RepoBundle, SchemaVersion, ScorecardKey, ScorecardValue,
    SourceCoverage,
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
            }),
            clone_tags: SourceCoverage::Completed,
        },
        default_branch: Availability::available("main".into()),
    };
    (dir, request)
}

include!("export_contract_tests/graph_views_and_serialization_tests.rs");

include!("export_contract_tests/provenance_and_availability_tests.rs");

include!("export_contract_tests/history_and_release_validation_tests.rs");

include!("export_contract_tests/schema_provenance_and_page_disclosure_tests.rs");
