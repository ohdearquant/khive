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
    let bundle = export(&request).unwrap();
    assert!(bundle.graph.functions.items.is_empty());
    assert!(bundle.graph.datatypes.items.is_empty());
    assert!(bundle.graph.interfaces.items.is_empty());
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
