use super::*;

#[tokio::test]
async fn report_file_open_validates_wal_environment() {
    if crate::test_process::run_in_child() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("map.db");
    let policy_path = root.path().join("policy.toml");
    std::fs::write(&policy_path, "policy_version = 1\n[crate_ranks]\n").unwrap();
    let seed = StorageBackend::sqlite_for_test(&db).unwrap();
    seed.prepare_core_schema().unwrap();
    drop(seed);
    let before = std::fs::read(&db).unwrap();
    let request = AuditRequest {
        map_db: db.clone(),
        policy_path,
        as_of: DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        history_window_days: 180,
        include_dev_dependencies: false,
    };
    std::env::set_var("KHIVE_SQLITE_WAL_CEILING_BYTES", "abc");
    let error = generate_report(&request)
        .await
        .expect_err("AUDIT_INVALID_ENV");
    assert!(
        matches!(error.downcast_ref::<khive_runtime::RuntimeError>(), Some(khive_runtime::RuntimeError::Storage(khive_storage::StorageError::Driver { source, .. })) if matches!(source.downcast_ref(), Some(khive_db::SqliteError::InvalidConfig(message)) if message.contains("KHIVE_SQLITE_WAL_CEILING_BYTES")))
    );
    for raw in ["0", "8192"] {
        std::env::set_var("KHIVE_SQLITE_WAL_CEILING_BYTES", raw);
        let report = generate_report(&request)
            .await
            .expect("read-only ceiling is diagnostic");
        assert_eq!(report.as_of, "2026-10-01T00:00:00+00:00");
        assert!(!report.signals.is_empty());
    }
    assert_eq!(std::fs::read(&db).unwrap(), before);
}

use std::path::Path;

use khive_storage::{SqlWriter, StorageResult};

#[test]
fn query_bundle_hash_is_stable() {
    assert_eq!(
        query_bundle_sha256(),
        "f81a0ecdb297858ed0193e82ff6bfbd31da1d27793537691251c15df70784ad8"
    );
}

#[test]
fn normalized_dependency_scopes_are_canonical_with_legacy_fallback() {
    assert_eq!(
        dependency_scopes(&json!({
            "dependency_kinds": ["dependencies"],
            "dependency_scopes": ["dev"]
        })),
        ["dev".to_string()].into_iter().collect()
    );
    assert_eq!(
        dependency_scopes(&json!({
            "dependency_kinds": ["dependencies", "build-dependencies"]
        })),
        ["build".to_string(), "normal".to_string()]
            .into_iter()
            .collect()
    );
    assert_eq!(
        dependency_scopes(&json!({
            "dependency_kinds": ["dev-dependencies", "import"]
        })),
        ["dev".to_string()].into_iter().collect()
    );
}

async fn seed(
    writer: &mut dyn SqlWriter,
    id: &str,
    kind: &str,
    entity_type: Option<&str>,
    name: &str,
    properties: Value,
) -> StorageResult<()> {
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO entities (id, namespace, kind, name, properties, tags, \
                      created_at, updated_at, entity_type) \
                      VALUES (?1, 'local', ?2, ?3, ?4, '[]', 0, 0, ?5)"
                .to_string(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(kind.to_string()),
                SqlValue::Text(name.to_string()),
                SqlValue::Text(properties.to_string()),
                match entity_type {
                    Some(t) => SqlValue::Text(t.to_string()),
                    None => SqlValue::Null,
                },
            ],
            label: None,
        })
        .await?;
    Ok(())
}

async fn seed_edge(
    writer: &mut dyn SqlWriter,
    id: &str,
    source: &str,
    target: &str,
    metadata: Value,
) -> StorageResult<()> {
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO graph_edges (namespace, id, source_id, target_id, relation, \
                      created_at, updated_at, metadata) \
                      VALUES ('local', ?1, ?2, ?3, 'depends_on', 0, 0, ?4)"
                .to_string(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(source.to_string()),
                SqlValue::Text(target.to_string()),
                SqlValue::Text(metadata.to_string()),
            ],
            label: None,
        })
        .await?;
    Ok(())
}

/// Builds a fixture map database at `path` with a small, deterministic
/// project/module graph: three projects (`crate-a` depends_on `crate-b`,
/// declared + observed; a policy-unmapped `crate-z` reachable only by a
/// dev edge; a policy-unmapped `crate-orphan` reachable by NO edge at
/// all), four modules (a self-import cycle between `crate_a::lib`/
/// `crate_a::util`, a zero-in-edge/duplicate-hash `crate_a::orphan`
/// carrying an unresolved import, and a same-named-but-different-
/// language `crate_a::lib` in python with no edges at all).
async fn build_fixture(path: &Path, edge_insert_order: [usize; 2]) {
    let backend = StorageBackend::sqlite_for_test(path).expect("open fixture backend");
    backend
        .pool()
        .run_migrations()
        .expect("run core migrations");
    let sql = backend.sql();
    let mut writer = sql.writer().await.expect("sql writer");

    seed(
        writer.as_mut(),
        "11111111-1111-1111-1111-111111111111",
        "project",
        None,
        "crate-a",
        json!({"source_project": "crate-a", "last_seen_at": "2026-07-16T00:00:00Z"}),
    )
    .await
    .unwrap();
    seed(
        writer.as_mut(),
        "22222222-2222-2222-2222-222222222222",
        "project",
        None,
        "crate-b",
        json!({"source_project": "crate-b", "last_seen_at": "2026-07-16T00:00:00Z"}),
    )
    .await
    .unwrap();
    seed(
        writer.as_mut(),
        "33333333-3333-3333-3333-333333333333",
        "project",
        None,
        "crate-z",
        json!({"source_project": "crate-z", "last_seen_at": "2026-07-16T00:00:00Z"}),
    )
    .await
    .unwrap();
    // H4: a project reachable by NO edge at all — must still degrade to
    // unavailable + POLICY_INCOMPLETE, not silently vanish.
    seed(
        writer.as_mut(),
        "cccccccc-cccc-cccc-cccc-cccccccccccc",
        "project",
        None,
        "crate-orphan",
        json!({"source_project": "crate-orphan", "last_seen_at": "2026-07-16T00:00:00Z"}),
    )
    .await
    .unwrap();

    seed_edge(
        writer.as_mut(),
        "44444444-4444-4444-4444-444444444444",
        "11111111-1111-1111-1111-111111111111",
        "22222222-2222-2222-2222-222222222222",
        json!({
            "dependency_kinds": ["dependencies"],
            "dependency_scopes": ["normal"]
        }),
    )
    .await
    .unwrap();
    // crate-b -> crate-z (dev-only): crate-z absent from policy.
    seed_edge(
        writer.as_mut(),
        "55555555-5555-5555-5555-555555555555",
        "22222222-2222-2222-2222-222222222222",
        "33333333-3333-3333-3333-333333333333",
        json!({
            "dependency_kinds": ["dev-dependencies"],
            "dependency_scopes": ["dev"]
        }),
    )
    .await
    .unwrap();
    // crate-b -> crate-a (production): the correct top-down direction
    // (rank 1 -> rank 0), so layering does NOT flag it — but together
    // with the first edge (crate-a -> crate-b, the violating direction)
    // it forms a project-level production cycle.
    seed_edge(
        writer.as_mut(),
        "66666666-6666-6666-6666-666666666666",
        "22222222-2222-2222-2222-222222222222",
        "11111111-1111-1111-1111-111111111111",
        json!({
            "dependency_kinds": ["dependencies"],
            "dependency_scopes": ["normal"]
        }),
    )
    .await
    .unwrap();

    seed(
        writer.as_mut(),
        "77777777-7777-7777-7777-777777777777",
        "concept",
        Some("module"),
        "crate_a::lib",
        json!({
            "source_project": "crate-a",
            "language": "rust",
            "module_path": "crate_a::lib",
            "content_hash": "deadbeef",
        }),
    )
    .await
    .unwrap();
    seed(
        writer.as_mut(),
        "88888888-8888-8888-8888-888888888888",
        "concept",
        Some("module"),
        "crate_a::util",
        json!({
            "source_project": "crate-a",
            "language": "rust",
            "module_path": "crate_a::util",
            "content_hash": "deadbeef",
        }),
    )
    .await
    .unwrap();
    seed(
        writer.as_mut(),
        "99999999-9999-9999-9999-999999999999",
        "concept",
        Some("module"),
        "crate_a::orphan",
        json!({
            "source_project": "crate-a",
            "language": "rust",
            "module_path": "crate_a::orphan",
            "content_hash": "cafef00d",
            "unresolved_specifiers": ["crate_a::missing"],
        }),
    )
    .await
    .unwrap();
    // H3: a SECOND module named identically to `crate_a::lib`, but a
    // distinct language/entity — must not be collapsed into the rust
    // module's Tarjan node or zero-in-edge subject by name collision.
    seed(
        writer.as_mut(),
        "dddddddd-dddd-dddd-dddd-dddddddddddd",
        "concept",
        Some("module"),
        "crate_a::lib",
        json!({
            "source_project": "crate-a",
            "language": "python",
            "module_path": "crate_a::lib",
            "content_hash": "beefcafe",
        }),
    )
    .await
    .unwrap();

    let module_edges = [
        (
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "77777777-7777-7777-7777-777777777777",
            "88888888-8888-8888-8888-888888888888",
        ),
        (
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "88888888-8888-8888-8888-888888888888",
            "77777777-7777-7777-7777-777777777777",
        ),
    ];
    for idx in edge_insert_order {
        let (id, source, target) = module_edges[idx];
        seed_edge(
            writer.as_mut(),
            id,
            source,
            target,
            json!({
                "dependency_kinds": ["import"],
                "dependency_scopes": ["build"]
            }),
        )
        .await
        .unwrap();
    }
}

fn test_policy(dir: &Path) -> PathBuf {
    write_policy(
        dir,
        "policy.toml",
        r#"
policy_version = 1
coverage_floor = 0.0
denied_pairs = []

[crate_ranks]
crate-a = 0
crate-b = 1
"#,
    )
}

fn write_policy(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn base_request(db: PathBuf, policy: PathBuf) -> AuditRequest {
    AuditRequest {
        map_db: db,
        policy_path: policy,
        as_of: DateTime::parse_from_rfc3339("2026-07-16T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        history_window_days: 180,
        include_dev_dependencies: false,
    }
}

#[cfg(unix)]
use khive_storage::test_support::freeze_snapshot_sidecars;

/// `generate_report` behind the frozen-snapshot form its read-only open
/// accepts. SQLite connection close is deferred, so a fixture seeded and
/// dropped just above can still hold writable `-wal`/`-shm` sidecars when
/// the report's read-only open runs; freeze them at the read boundary
/// (not inside the seeder — several tests reopen the fixture writably to
/// add rows after seeding).
async fn frozen_generate_report(request: &AuditRequest) -> Result<AuditReport> {
    #[cfg(unix)]
    freeze_snapshot_sidecars(&request.map_db);
    generate_report(request).await
}

#[tokio::test]
async fn layering_violation_flags_upward_dependency() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    let policy = test_policy(tmp.path());

    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    // crate-a (rank 0) depends_on crate-b (rank 1): a lower-ranked,
    // more-foundational crate depending on a higher-ranked crate is the
    // violating direction. The reverse edge in the fixture (crate-b ->
    // crate-a, rank 1 -> rank 0) is the correct direction and must NOT
    // be flagged, even though together the two edges form a cycle
    // (asserted separately by the dependency_cycle signal test).
    let violation = report
        .signals
        .iter()
        .find(|s| s.id == "layering_violation" && s.subject_id == "crate-a->crate-b")
        .expect("expected crate-a->crate-b layering violation");
    assert_eq!(violation.status, SignalStatus::Observed);
    assert!(
        !report
            .signals
            .iter()
            .any(|s| s.id == "layering_violation" && s.subject_id == "crate-b->crate-a"),
        "crate-b->crate-a is the correct top-down direction and must not be flagged"
    );

    // crate-z is only reachable via a dev-only edge and crate-orphan has
    // NO edge at all — both are unmapped in the policy, so BOTH must
    // degrade to unavailable regardless of include_dev_dependencies:
    // POLICY_INCOMPLETE is derived from the full project set, not from
    // which edges happened to be selected.
    for unmapped in ["crate-z", "crate-orphan"] {
        let signal = report
            .signals
            .iter()
            .find(|s| s.id == "layering_violation" && s.subject_id == unmapped)
            .unwrap_or_else(|| panic!("expected {unmapped} reported as policy-incomplete"));
        assert_eq!(signal.status, SignalStatus::Unavailable);
    }
    assert!(report
        .errors
        .iter()
        .any(|e| matches!(e.code, AuditErrorCode::PolicyIncomplete)));

    let mut with_dev = base_request(tmp.path().join("map.db"), test_policy(tmp.path()));
    with_dev.include_dev_dependencies = true;
    let report_with_dev = frozen_generate_report(&with_dev).await.unwrap();
    let unmapped = report_with_dev
        .signals
        .iter()
        .find(|s| s.id == "layering_violation" && s.subject_id == "crate-z")
        .expect("expected crate-z reported as policy-incomplete once dev edges are included");
    assert_eq!(unmapped.status, SignalStatus::Unavailable);
}

#[tokio::test]
async fn dependency_cycle_detects_project_and_module_cycles() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    let policy = test_policy(tmp.path());

    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    let project_cycle = report.signals.iter().any(|s| {
        s.id == "dependency_cycle"
            && s.subject_id.starts_with("project_production:")
            && s.subject_id
                .contains("11111111-1111-1111-1111-111111111111")
            && s.subject_id
                .contains("22222222-2222-2222-2222-222222222222")
    });
    assert!(project_cycle, "expected a project_production cycle");

    // H3: the module_import cycle must be exactly the rust lib/util
    // pair by ID — the same-named python `crate_a::lib` module (which
    // has no edges at all) must NOT appear in any cycle membership.
    let module_cycle = report
        .signals
        .iter()
        .find(|s| s.id == "dependency_cycle" && s.subject_id.starts_with("module_import:"))
        .expect("expected a module_import cycle");
    assert_eq!(
        module_cycle.subject_id,
        "module_import:77777777-7777-7777-7777-777777777777,\
             88888888-8888-8888-8888-888888888888"
    );
    assert!(
        !module_cycle
            .subject_id
            .contains("dddddddd-dddd-dddd-dddd-dddddddddddd"),
        "the edge-less python crate_a::lib must not be pulled into the rust SCC by name"
    );
}

#[tokio::test]
async fn zero_in_edge_and_duplicate_hash_are_candidates() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    let policy = test_policy(tmp.path());

    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();

    // Subject is now the module's own ID (not its display name), so the
    // two same-named `crate_a::lib` modules each get an independent,
    // unambiguous zero_in_edge_module signal.
    let orphan = report
        .signals
        .iter()
        .find(|s| {
            s.id == "zero_in_edge_module" && s.subject_id == "99999999-9999-9999-9999-999999999999"
        })
        .expect("orphan module has no incoming depends_on edge");
    assert_eq!(orphan.status, SignalStatus::Candidate);

    let python_lib = report
        .signals
        .iter()
        .find(|s| {
            s.id == "zero_in_edge_module" && s.subject_id == "dddddddd-dddd-dddd-dddd-dddddddddddd"
        })
        .expect("edge-less python crate_a::lib is independently a zero-in-edge candidate");
    assert_eq!(python_lib.status, SignalStatus::Candidate);

    let dup = report
        .signals
        .iter()
        .find(|s| s.id == "duplicate_content_hash" && s.subject_id == "deadbeef")
        .expect("crate_a::lib and crate_a::util share a content hash");
    assert_eq!(dup.status, SignalStatus::Candidate);
    assert_eq!(dup.evidence_ids.len(), 2);
}

#[tokio::test]
async fn ingest_coverage_aggregates_module_level_unresolved_imports() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    let policy = test_policy(tmp.path());

    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    // crate-a itself carries no `unresolved_specifiers` property; the
    // one unresolved import lives on its `crate_a::orphan` MODULE
    // (source_ingest.rs:776-784). The pre-fix code read only the
    // project's own property and reported 0.
    let coverage = report
        .signals
        .iter()
        .find(|s| s.id == "ingest_coverage" && s.subject_id == "crate-a")
        .expect("expected an ingest_coverage signal for crate-a");
    assert_eq!(
        coverage
            .value
            .get("unresolved_specifier_count")
            .and_then(Value::as_u64),
        Some(1),
        "unresolved specifier recorded on a module must roll up to its project"
    );
    assert!(coverage
        .evidence_ids
        .contains(&"99999999-9999-9999-9999-999999999999".to_string()));
}

#[tokio::test]
async fn coverage_floor_gates_zero_in_edge_to_unavailable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    // crate-a has 4 modules (lib, util, orphan, python-lib) and 1
    // unresolved specifier => coverage_ratio = 4/(4+1) = 0.8. A floor of
    // 0.9 must suppress crate-a's zero_in_edge candidates.
    let policy = write_policy(
        tmp.path(),
        "strict-policy.toml",
        r#"
policy_version = 1
coverage_floor = 0.9
denied_pairs = []

[crate_ranks]
crate-a = 0
crate-b = 1
"#,
    );

    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    let orphan = report
        .signals
        .iter()
        .find(|s| {
            s.id == "zero_in_edge_module" && s.subject_id == "99999999-9999-9999-9999-999999999999"
        })
        .unwrap();
    assert_eq!(orphan.status, SignalStatus::Unavailable);
    assert!(report
        .errors
        .iter()
        .any(|e| matches!(e.code, AuditErrorCode::CoverageInsufficient)));
}

#[tokio::test]
async fn every_catalog_signal_declares_status() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    let policy = test_policy(tmp.path());

    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    for id in ["churn_hotspot", "dead_file", "orphan_test_file"] {
        assert!(
            report
                .signals
                .iter()
                .any(|s| s.id == id && s.status == SignalStatus::Unavailable),
            "expected an unavailable {id} signal"
        );
    }
    assert!(report
        .errors
        .iter()
        .any(|e| matches!(e.code, AuditErrorCode::HistoryAbsent)));
}

#[tokio::test]
async fn report_is_byte_identical_across_runs_and_insertion_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let policy = test_policy(tmp.path());

    let db_a = tmp.path().join("map_a.db");
    build_fixture(&db_a, [0, 1]).await;
    let report_a = frozen_generate_report(&base_request(db_a, policy.clone()))
        .await
        .unwrap();
    let json_a = serde_json::to_string_pretty(&report_a).unwrap();

    let db_b = tmp.path().join("map_b.db");
    build_fixture(&db_b, [1, 0]).await;
    let report_b = frozen_generate_report(&base_request(db_b, policy.clone()))
        .await
        .unwrap();
    let json_b = serde_json::to_string_pretty(&report_b).unwrap();

    assert_eq!(
        json_a, json_b,
        "report JSON must be byte-identical regardless of edge insertion order"
    );

    let db_c = tmp.path().join("map_c.db");
    build_fixture(&db_c, [0, 1]).await;
    let report_c = frozen_generate_report(&base_request(db_c, policy))
        .await
        .unwrap();
    let json_c = serde_json::to_string_pretty(&report_c).unwrap();
    assert_eq!(
        json_a, json_c,
        "two runs over the same fixture must produce byte-identical JSON"
    );
}

#[tokio::test]
async fn manifest_import_mismatch_treats_every_declaration_kind_as_declared() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    let backend = StorageBackend::sqlite_for_test(&db).expect("open fixture backend");
    backend
        .pool()
        .run_migrations()
        .expect("run core migrations");
    let sql = backend.sql();
    let mut writer = sql.writer().await.expect("sql writer");

    let projects = [
        ("11111111-1111-1111-1111-11111111aaaa", "app"),
        ("22222222-2222-2222-2222-22222222aaaa", "npm-dev-dep"),
        ("33333333-3333-3333-3333-33333333aaaa", "npm-optional-dep"),
        ("44444444-4444-4444-4444-44444444aaaa", "py-extra-dep"),
        ("55555555-5555-5555-5555-55555555aaaa", "cargo-dev-dep"),
        ("66666666-6666-6666-6666-66666666aaaa", "undeclared-dep"),
    ];
    for (id, name) in projects {
        seed(
            writer.as_mut(),
            id,
            "project",
            None,
            name,
            json!({"source_project": name, "last_seen_at": "2026-07-16T00:00:00Z"}),
        )
        .await
        .unwrap();
    }

    // Each ecosystem's declaration vocabulary alongside import evidence:
    // none of these may be flagged. The last edge is the must-match
    // control — import evidence with no declaration at all.
    let edges = [
        (
            "aaaaaaaa-1111-1111-1111-aaaaaaaaaaaa",
            "22222222-2222-2222-2222-22222222aaaa",
            json!({"dependency_kinds": ["devDependencies", "import"]}),
        ),
        (
            "aaaaaaaa-2222-2222-2222-aaaaaaaaaaaa",
            "33333333-3333-3333-3333-33333333aaaa",
            json!({"dependency_kinds": ["optionalDependencies", "import"]}),
        ),
        (
            "aaaaaaaa-3333-3333-3333-aaaaaaaaaaaa",
            "44444444-4444-4444-4444-44444444aaaa",
            json!({"dependency_kinds": ["optional-dependencies:test", "import"]}),
        ),
        (
            "aaaaaaaa-4444-4444-4444-aaaaaaaaaaaa",
            "55555555-5555-5555-5555-55555555aaaa",
            json!({"dependency_kinds": ["dev-dependencies", "import"]}),
        ),
        (
            "aaaaaaaa-5555-5555-5555-aaaaaaaaaaaa",
            "66666666-6666-6666-6666-66666666aaaa",
            json!({"dependency_kinds": ["import"]}),
        ),
    ];
    for (id, target, metadata) in edges {
        seed_edge(
            writer.as_mut(),
            id,
            "11111111-1111-1111-1111-11111111aaaa",
            target,
            metadata,
        )
        .await
        .unwrap();
    }
    drop(writer);

    let policy = write_policy(
        tmp.path(),
        "policy.toml",
        r#"
policy_version = 1
coverage_floor = 0.0
denied_pairs = []

[crate_ranks]
app = 0
npm-dev-dep = 1
npm-optional-dep = 1
py-extra-dep = 1
cargo-dev-dep = 1
undeclared-dep = 1
"#,
    );
    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();

    let flagged: Vec<&Signal> = report
        .signals
        .iter()
        .filter(|s| s.id == "manifest_import_mismatch")
        .collect();
    assert_eq!(
        flagged.len(),
        1,
        "only the declaration-free edge may be flagged: {flagged:?}"
    );
    assert_eq!(
        flagged[0].evidence_ids,
        vec!["aaaaaaaa-5555-5555-5555-aaaaaaaaaaaa".to_string()],
        "the flagged evidence must be the import-only edge"
    );
    assert_eq!(flagged[0].subject_id, "app->undeclared-dep");
}

#[tokio::test]
async fn missing_map_database_fails_loud() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("does-not-exist.db");
    let policy = test_policy(tmp.path());
    let err = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not exist"));
}

#[tokio::test]
async fn missing_table_degrades_affected_signals_instead_of_aborting() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    {
        let backend = StorageBackend::sqlite_for_test(&db).expect("open fixture backend");
        let writer = backend.pool().writer().expect("writer guard");
        writer
            .autocommit(|conn| Ok(conn.execute_batch("DROP TABLE graph_edges;")?))
            .expect("drop graph_edges to simulate a partial/older map");
    }
    let policy = test_policy(tmp.path());

    // H1: a partial map missing an entire table must still produce a
    // report — not abort the whole command.
    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    assert!(report
        .errors
        .iter()
        .any(|e| matches!(e.code, AuditErrorCode::SchemaUnsupported)
            && e.message.contains("graph_edges")));
    for id in [
        "module_fan_in",
        "layering_violation",
        "manifest_import_mismatch",
        "dependency_cycle_summary",
        "zero_in_edge_module",
    ] {
        assert!(
            report
                .signals
                .iter()
                .any(|s| s.id == id && s.status == SignalStatus::Unavailable),
            "expected {id} to degrade to unavailable when graph_edges is missing"
        );
    }
    // entities-only signals are unaffected by a missing graph_edges table.
    assert!(report
        .signals
        .iter()
        .any(|s| s.id == "ingest_coverage" && s.status == SignalStatus::Observed));
}

#[tokio::test]
async fn missing_column_degrades_affected_signals_instead_of_aborting() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    {
        let backend = StorageBackend::sqlite_for_test(&db).expect("open fixture backend");
        let writer = backend.pool().writer().expect("writer guard");
        writer
            .autocommit(|conn| {
                Ok(conn.execute_batch(
                    "DROP INDEX IF EXISTS idx_entities_kind_entity_type; \
                     DROP INDEX IF EXISTS idx_entities_legacy_type; \
                     DROP INDEX IF EXISTS idx_entities_live_namespace_type_order; \
                     ALTER TABLE entities DROP COLUMN entity_type;",
                )?)
            })
            .expect("drop entity_type to simulate an older map schema");
    }
    let policy = test_policy(tmp.path());

    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    assert!(report
        .errors
        .iter()
        .any(|e| matches!(e.code, AuditErrorCode::SchemaUnsupported)
            && e.message.contains("entity_type")));
    assert!(report
        .signals
        .iter()
        .any(|s| s.id == "ingest_coverage" && s.status == SignalStatus::Unavailable));
    assert!(report
        .signals
        .iter()
        .any(|s| s.id == "module_fan_in" && s.status == SignalStatus::Unavailable));
}

#[tokio::test]
async fn missing_edge_metadata_column_degrades_only_metadata_dependent_signals() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    {
        let backend = StorageBackend::sqlite_for_test(&db).expect("open fixture backend");
        let writer = backend.pool().writer().expect("writer guard");
        writer
            .autocommit(|conn| {
                Ok(conn.execute_batch("ALTER TABLE graph_edges DROP COLUMN metadata;")?)
            })
            .expect(
                "drop metadata to simulate a map missing dependency-kind classification \
                     while every other edge column is intact",
            );
    }
    let policy = test_policy(tmp.path());

    // `module_fan_in` and `zero_in_edge_module` never
    // read `graph_edges.metadata`, so a map missing ONLY that column
    // must not suppress them — only the dependency-kind-classification
    // signals (layering, manifest/import mismatch, dependency cycles)
    // may degrade.
    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    assert!(report
        .errors
        .iter()
        .any(|e| matches!(e.code, AuditErrorCode::SchemaUnsupported)
            && e.message.contains("metadata")));

    for id in ["module_fan_in", "zero_in_edge_module"] {
        assert!(
            report
                .signals
                .iter()
                .any(|s| s.id == id && s.status != SignalStatus::Unavailable),
            "expected {id} to remain available when only graph_edges.metadata is missing"
        );
    }
    for id in [
        "layering_violation",
        "manifest_import_mismatch",
        "dependency_cycle_summary",
    ] {
        assert!(
            report
                .signals
                .iter()
                .any(|s| s.id == id && s.status == SignalStatus::Unavailable),
            "expected {id} to degrade to unavailable when graph_edges.metadata is missing"
        );
    }
}

#[tokio::test]
async fn missing_edge_id_column_degrades_only_id_dependent_signals() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("map.db");
    build_fixture(&db, [0, 1]).await;
    {
        let backend = StorageBackend::sqlite_for_test(&db).expect("open fixture backend");
        let writer = backend.pool().writer().expect("writer guard");
        // `id` is part of `graph_edges`'s composite PRIMARY KEY
        // (namespace, id), so SQLite's `ALTER TABLE ... DROP COLUMN`
        // refuses it ("cannot drop PRIMARY KEY column"); rebuild the
        // table without the column instead to simulate a map missing
        // ONLY `graph_edges.id`. Drop the GTD integrity triggers first:
        // they intentionally depend on `graph_edges`, while this fixture
        // intentionally replaces that table with a malformed schema.
        writer
            .autocommit(|conn| {
                Ok(conn.execute_batch(
                    "DROP TRIGGER gtd_task_dependency_cycle_notes_bi;
                     DROP TRIGGER gtd_task_dependency_cycle_notes_bu;
                     DROP TRIGGER gtd_task_dependency_cycle_note_activation_bi;
                     DROP TRIGGER gtd_task_dependency_cycle_note_activation_bu;
                     DROP TRIGGER gtd_task_dependency_cycle_edges_bi;
                     DROP TRIGGER gtd_task_dependency_cycle_edges_bu;
                     CREATE TABLE graph_edges_no_id (
                         namespace      TEXT NOT NULL,
                         source_id      TEXT NOT NULL,
                         target_id      TEXT NOT NULL,
                         relation       TEXT NOT NULL,
                         weight         REAL NOT NULL DEFAULT 1.0,
                         created_at     INTEGER NOT NULL,
                         updated_at     INTEGER NOT NULL,
                         deleted_at     INTEGER,
                         metadata       TEXT,
                         target_backend TEXT
                     );
                     INSERT INTO graph_edges_no_id (
                         namespace, source_id, target_id, relation, weight,
                         created_at, updated_at, deleted_at, metadata, target_backend
                     )
                     SELECT namespace, source_id, target_id, relation, weight,
                            created_at, updated_at, deleted_at, metadata, target_backend
                     FROM graph_edges;
                     DROP TABLE graph_edges;
                     ALTER TABLE graph_edges_no_id RENAME TO graph_edges;",
                )?)
            })
            .expect("rebuild graph_edges without id to simulate a map missing only that column");
    }
    let policy = test_policy(tmp.path());

    // `module_fan_in` and `zero_in_edge_module` never
    // read `graph_edges.id` (`SQL_FAN_IN`/`SQL_ZERO_IN_EDGE` select only
    // `source_id`/`target_id`/`relation`/`deleted_at`), so a map missing
    // ONLY that column must not suppress them — only the signals that
    // read `id` (layering, manifest/import mismatch, dependency cycles)
    // may degrade.
    let report = frozen_generate_report(&base_request(db, policy))
        .await
        .unwrap();
    assert!(report
        .errors
        .iter()
        .any(|e| matches!(e.code, AuditErrorCode::SchemaUnsupported)
            && e.message.contains("graph_edges")
            && e.message.contains("id")));

    for id in ["module_fan_in", "zero_in_edge_module"] {
        assert!(
            report
                .signals
                .iter()
                .any(|s| s.id == id && s.status != SignalStatus::Unavailable),
            "expected {id} to remain available when only graph_edges.id is missing"
        );
    }
    for id in [
        "layering_violation",
        "manifest_import_mismatch",
        "dependency_cycle_summary",
    ] {
        assert!(
            report
                .signals
                .iter()
                .any(|s| s.id == id && s.status == SignalStatus::Unavailable),
            "expected {id} to degrade to unavailable when graph_edges.id is missing"
        );
    }
}

#[tokio::test]
async fn policy_rejects_unknown_fields() {
    let tmp = tempfile::TempDir::new().unwrap();
    let policy = write_policy(
        tmp.path(),
        "bad.toml",
        r#"
policy_version = 1
typo_field = true

[crate_ranks]
crate-a = 0
"#,
    );
    let err = load_policy(&policy).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("typo_field")
            || format!("{err:#}").to_lowercase().contains("unknown field")
    );
}

#[tokio::test]
async fn policy_rejects_missing_version() {
    let tmp = tempfile::TempDir::new().unwrap();
    let policy = write_policy(
        tmp.path(),
        "no-version.toml",
        r#"
[crate_ranks]
crate-a = 0
"#,
    );
    assert!(load_policy(&policy).is_err());
}

#[tokio::test]
async fn policy_rejects_unsupported_version() {
    let tmp = tempfile::TempDir::new().unwrap();
    let policy = write_policy(
        tmp.path(),
        "future-version.toml",
        r#"
policy_version = 99

[crate_ranks]
crate-a = 0
"#,
    );
    let err = load_policy(&policy).unwrap_err();
    assert!(err.to_string().contains("policy_version"));
}
