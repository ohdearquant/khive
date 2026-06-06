use super::*;
use tempfile::TempDir;

fn make_kg_dir(tmp: &TempDir) -> PathBuf {
    let kg_dir = tmp.path().join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).unwrap();
    kg_dir
}

fn write_entities(kg_dir: &Path, entities: &[(&str, &str, &str)]) {
    let content: String = entities
        .iter()
        .map(|(id, kind, name)| format!(r#"{{"id":"{id}","kind":"{kind}","name":"{name}"}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("entities.ndjson"), content + "\n").unwrap();
}

fn write_edges(kg_dir: &Path, edges: &[(&str, &str, &str)]) {
    let content: String = edges
        .iter()
        .map(|(src, tgt, rel)| {
            format!(r#"{{"source_id":"{src}","target_id":"{tgt}","relation":"{rel}"}}"#)
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("edges.ndjson"), content + "\n").unwrap();
}

#[test]
fn duplicate_uuid_detected() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A-dup"),
        ],
    );
    let result = check_no_duplicate_uuids(&kg_dir.join("entities.ndjson"));
    assert!(!result.passed, "duplicate UUID should fail");
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn no_duplicates_passes() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    let result = check_no_duplicate_uuids(&kg_dir.join("entities.ndjson"));
    assert!(result.passed);
}

#[test]
fn referential_integrity_catches_missing_target() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "extends",
        )],
    );
    let result = check_referential_integrity(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
    );
    assert!(!result.passed);
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn init_creates_expected_files() {
    let tmp = TempDir::new().unwrap();
    let args = InitArgs {
        repo: tmp.path().to_path_buf(),
        ci: false,
        add_hooks: false,
    };
    cmd_init(args).unwrap();

    assert!(tmp.path().join(".khive/kg/entities.ndjson").exists());
    assert!(tmp.path().join(".khive/kg/edges.ndjson").exists());
    assert!(tmp.path().join(".khive/khive.toml").exists());
    assert!(tmp.path().join(".khive/kg/hooks/pre-commit").exists());
}

#[test]
fn init_does_not_overwrite_existing_toml() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".khive")).unwrap();
    let toml_path = tmp.path().join(".khive/khive.toml");
    std::fs::write(&toml_path, "# custom\n").unwrap();

    let args = InitArgs {
        repo: tmp.path().to_path_buf(),
        ci: false,
        add_hooks: false,
    };
    cmd_init(args).unwrap();

    let content = std::fs::read_to_string(&toml_path).unwrap();
    assert_eq!(content, "# custom\n", "should not overwrite existing toml");
}

// ── configurable_rule_checks (issue #382) ─────────────────────────────────

#[test]
fn configurable_rule_checks_empty_rules_file_returns_no_results() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_path = tmp.path().join("rules.toml");
    // Valid TOML with empty rules array.
    std::fs::write(&rules_path, "rules = []\n").unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &rules_path,
    )
    .unwrap();
    assert!(results.is_empty(), "no rules → no results");
}

#[test]
fn configurable_rule_checks_require_field_detects_missing_description() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);

    // One entity with description, one without.
    let entities = r#"{"id":"aaa1","kind":"concept","name":"A","description":"has one"}
{"id":"aaa2","kind":"concept","name":"B"}
"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities).unwrap();
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_toml = r#"
[[rules]]
id = "concept-must-have-description"
severity = "warning"
kind = "entity"
condition = "kind=concept"
require_field = "description"
message = "Concept {id} missing description"
"#;
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, rules_toml).unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &rules_path,
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert_eq!(r.id, "concept-must-have-description");
    assert!(
        !r.passed,
        "rule should fail when a concept lacks description"
    );
    assert_eq!(r.violations.len(), 1);
    assert_eq!(r.violations[0].entity_id.as_deref(), Some("aaa2"));
}

#[test]
fn configurable_rule_checks_self_loop_sentinel_detects_loop() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);

    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    // One self-loop edge, one valid edge.
    let edges = r#"{"source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"aaaaaaaa-0000-0000-0000-000000000001","relation":"extends"}
{"source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}
"#;
    std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();

    let rules_toml = r#"
[[rules]]
id = "no-self-loops"
severity = "error"
kind = "edge"
condition = "source_id=target_id"
message = "Self-loop detected on {id}"
"#;
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, rules_toml).unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &rules_path,
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(!r.passed);
    assert_eq!(r.violations.len(), 1, "exactly one self-loop");
}

#[test]
fn configurable_rule_checks_yaml_extension_returns_error() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_path = tmp.path().join("rules.yaml");
    std::fs::write(&rules_path, "rules: []\n").unwrap();

    let result = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &rules_path,
    );
    assert!(result.is_err(), "YAML extension must return an error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("YAML") || msg.contains("toml"),
        "error message should mention TOML: {msg}"
    );
}

#[test]
fn configurable_rule_checks_unknown_kind_produces_error_result() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_toml = r#"
[[rules]]
id = "bad-kind"
severity = "error"
kind = "note"
condition = "kind=concept"
require_field = "description"
message = "bad"
"#;
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, rules_toml).unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &rules_path,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert!(!results[0].passed);
    assert_eq!(results[0].severity, "error");
}

#[test]
fn configurable_rule_checks_invalid_severity_produces_error_result() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_toml = r#"
[[rules]]
id = "bad-severity"
severity = "erorr"
kind = "entity"
require_field = "description"
message = "bad"
"#;
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, rules_toml).unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &rules_path,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert!(!results[0].passed, "invalid severity must fail");
    assert_eq!(results[0].severity, "error");
    assert!(
        results[0].violations[0]
            .message
            .contains("invalid severity"),
        "error message should mention invalid severity"
    );
}

#[test]
fn sort_order_fix_sorts_entities() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    // Write out-of-order entities.
    write_entities(
        &kg_dir,
        &[
            ("cccccccc-0000-0000-0000-000000000003", "concept", "C"),
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    fix_sort_order(&kg_dir.join("entities.ndjson"), "id").unwrap();
    let result = check_sort_order(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
    );
    assert!(result.passed, "sort-order should pass after fix");
}

// ── fetch / sync alias ────────────────────────────────────────────────────

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
    assert!(
        status.success(),
        "git {} exited with {}",
        args.join(" "),
        status
    );
}

fn make_git_remote_for_kg(dir: &std::path::Path) -> String {
    let kg_dir = dir.join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).unwrap();
    let entity_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let entities = format!(
        r#"{{"id":"{entity_id}","kind":"concept","name":"RemoteEntity","properties":{{}},"tags":[]}}"#
    );
    std::fs::write(kg_dir.join("entities.ndjson"), &entities).unwrap();
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-m", "init"]);
    dir.to_string_lossy().into_owned()
}

#[tokio::test]
async fn fetch_populates_temp_remote_cache() {
    let remote_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();
    let remote_url = make_git_remote_for_kg(remote_dir.path());

    let args = FetchArgs {
        remote: "upstream".to_string(),
        repo: repo_dir.path().to_path_buf(),
        url: remote_url,
        git_ref: "main".to_string(),
        namespace: "remote-ns".to_string(),
        pin: None,
        repin: false,
    };

    cmd_fetch(args).await.unwrap();

    let cache = repo_dir.path().join(".khive/kg/remotes/upstream");
    assert!(
        cache.join("entities.ndjson").exists(),
        "entities.ndjson in cache"
    );
    assert!(cache.join("edges.ndjson").exists(), "edges.ndjson in cache");
    assert!(cache.join("meta.json").exists(), "meta.json in cache");
}

// ── export ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn export_creates_archive_json() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let output_path = tmp.path().join("archive.json");

    let ns = Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let token = runtime.authorize(ns).unwrap();
    runtime
        .create_entity(&token, "concept", None, "TestEntity", None, None, vec![])
        .await
        .unwrap();

    let args = ExportArgs {
        output: output_path.clone(),
        db: db_path,
        namespace: "test-ns".to_string(),
    };
    cmd_export(args).await.unwrap();

    assert!(output_path.exists(), "output archive must exist");
    let content = std::fs::read_to_string(&output_path).unwrap();
    let archive: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(archive["format"].as_str().unwrap(), "khive-kg");
    let entities = archive["entities"].as_array().unwrap();
    assert_eq!(entities.len(), 1, "one entity exported");
    assert_eq!(entities[0]["name"].as_str().unwrap(), "TestEntity");
}

// codex #529: a symlinked --output pointing at the DB must be refused, and
// the source DB must remain byte-for-byte intact.
#[tokio::test]
#[cfg(unix)]
async fn export_refuses_symlinked_output_to_db() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("working.db");

    let ns = Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let token = runtime.authorize(ns).unwrap();
    runtime
        .create_entity(&token, "concept", None, "Keep", None, None, vec![])
        .await
        .unwrap();
    drop(runtime);
    let before = std::fs::read(&db_path).unwrap();

    // --output is a symlink pointing straight at the DB.
    let link = tmp.path().join("archive.json");
    std::os::unix::fs::symlink(&db_path, &link).unwrap();

    let args = ExportArgs {
        output: link,
        db: db_path.clone(),
        namespace: "test-ns".to_string(),
    };
    assert!(
        cmd_export(args).await.is_err(),
        "export through a symlink to the DB must be refused"
    );

    let after = std::fs::read(&db_path).unwrap();
    assert_eq!(before, after, "source DB must be byte-for-byte unchanged");
}

// codex #529 round 3: a planted symlink at the temp write path must not be
// followed into the DB either (create_new / O_EXCL refuses it).
#[tokio::test]
#[cfg(unix)]
async fn export_refuses_symlinked_temp_to_db() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("working.db");

    let ns = Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let token = runtime.authorize(ns).unwrap();
    runtime
        .create_entity(&token, "concept", None, "Keep", None, None, vec![])
        .await
        .unwrap();
    drop(runtime);
    let before = std::fs::read(&db_path).unwrap();

    // Plant a symlink at the exact temp path cmd_export will try to create
    // (same process => same pid suffix).
    let out = tmp.path().join("archive.json");
    let mut tmp_name = out.file_name().unwrap().to_os_string();
    tmp_name.push(format!(".{}.inprogress", std::process::id()));
    let temp_path = out.with_file_name(tmp_name);
    std::os::unix::fs::symlink(&db_path, &temp_path).unwrap();

    let args = ExportArgs {
        output: out,
        db: db_path.clone(),
        namespace: "test-ns".to_string(),
    };
    assert!(
        cmd_export(args).await.is_err(),
        "export must refuse when the temp path is a symlink to the DB"
    );
    let after = std::fs::read(&db_path).unwrap();
    assert_eq!(before, after, "source DB must be byte-for-byte unchanged");
}

// ── import archive ────────────────────────────────────────────────────────

#[tokio::test]
async fn import_archive_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("import-test.db");
    let entity_id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

    let archive_json = format!(
        r#"{{"format":"khive-kg","version":"0.1","namespace":"test-ns","exported_at":"2026-01-01T00:00:00Z","entities":[{{"id":"{entity_id}","kind":"concept","name":"Imported","tags":[],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}],"edges":[]}}"#
    );
    let source_path = tmp.path().join("archive.json");
    std::fs::write(&source_path, &archive_json).unwrap();

    let args = ImportArgs {
        source: source_path,
        db: db_path.clone(),
        namespace: "test-ns".to_string(),
        format: ImportFormat::Archive,
        verbose: false,
    };
    cmd_import(args).await.unwrap();

    let ns = Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let rt2 = KhiveRuntime::new(config).unwrap();
    let tok2 = rt2.authorize(ns).unwrap();
    let entity_uuid: Uuid = entity_id.parse().unwrap();
    let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
    assert_eq!(entity.name, "Imported");
}

// ── import --format json ──────────────────────────────────────────────────

#[tokio::test]
async fn import_json_adapter_imports_entities() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("adapter-json.db");
    let e1_id = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    let e2_id = "dddddddd-dddd-dddd-dddd-dddddddddddd";

    let json_input = format!(
        r#"[{{"id":"{e1_id}","kind":"concept","name":"Entity1"}},{{"id":"{e2_id}","kind":"concept","name":"Entity2"}}]"#
    );
    let source_path = tmp.path().join("records.json");
    std::fs::write(&source_path, &json_input).unwrap();

    let args = ImportArgs {
        source: source_path,
        db: db_path.clone(),
        namespace: "test-ns".to_string(),
        format: ImportFormat::Json,
        verbose: false,
    };
    cmd_import(args).await.unwrap();

    let ns = Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let rt2 = KhiveRuntime::new(config).unwrap();
    let tok2 = rt2.authorize(ns).unwrap();
    let e1_uuid: Uuid = e1_id.parse().unwrap();
    let entity = rt2.get_entity(&tok2, e1_uuid).await.unwrap();
    assert_eq!(entity.name, "Entity1");
}

// ── import --format ndjson ────────────────────────────────────────────────

#[tokio::test]
async fn import_ndjson_adapter_imports_entity() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("adapter-ndjson.db");
    let entity_id = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";

    let ndjson_input = format!(r#"{{"id":"{entity_id}","kind":"concept","name":"NdjsonEntity"}}"#);
    let source_path = tmp.path().join("records.ndjson");
    std::fs::write(&source_path, &ndjson_input).unwrap();

    let args = ImportArgs {
        source: source_path,
        db: db_path.clone(),
        namespace: "test-ns".to_string(),
        format: ImportFormat::Ndjson,
        verbose: false,
    };
    cmd_import(args).await.unwrap();

    let ns = Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db_path),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let rt2 = KhiveRuntime::new(config).unwrap();
    let tok2 = rt2.authorize(ns).unwrap();
    let entity_uuid: Uuid = entity_id.parse().unwrap();
    let entity = rt2.get_entity(&tok2, entity_uuid).await.unwrap();
    assert_eq!(entity.name, "NdjsonEntity");
}

// ── status ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn status_hashes_clean_after_sync() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    let entity_id = "ffffffff-ffff-ffff-ffff-ffffffffffff";
    let entity_ndjson = format!(
        r#"{{"id":"{entity_id}","kind":"concept","name":"StatusEntity","properties":{{}},"tags":[]}}"#
    );
    let kg_dir = repo.join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).unwrap();
    std::fs::write(kg_dir.join("entities.ndjson"), &entity_ndjson).unwrap();
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let db = repo.join(".khive/state/working.db");
    crate::sync::run_sync(repo, &db, "test-ns").await.unwrap();

    let ns = Namespace::parse("test-ns").unwrap();
    let config = RuntimeConfig {
        db_path: Some(db),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let token = runtime.authorize(ns).unwrap();

    let db_archive = runtime.export_kg(&token).await.unwrap();
    let ndjson_archive = archive_from_ndjson_repo(repo, "test-ns").unwrap();

    let db_hash = khive_vcs::hash::snapshot_id_for_archive(&db_archive).unwrap();
    let ndjson_hash = khive_vcs::hash::snapshot_id_for_archive(&ndjson_archive).unwrap();
    assert_eq!(db_hash, ndjson_hash, "hashes must match after sync");
}

// ── edge weight validation ────────────────────────────────────────────────

#[test]
fn validate_edge_weight_valid_boundaries() {
    assert!(validate_edge_weight(0.0, "edge-a").is_ok());
    assert!(validate_edge_weight(1.0, "edge-a").is_ok());
    assert!(validate_edge_weight(0.5, "edge-a").is_ok());
}

#[test]
fn validate_edge_weight_nan_is_rejected() {
    let err = validate_edge_weight(f64::NAN, "edge-x").unwrap_err();
    assert!(
        err.to_string().contains("not finite"),
        "expected 'not finite' in error: {err}"
    );
}

#[test]
fn validate_edge_weight_infinity_is_rejected() {
    let err = validate_edge_weight(f64::INFINITY, "edge-y").unwrap_err();
    assert!(
        err.to_string().contains("not finite"),
        "expected 'not finite' in error: {err}"
    );
    let err = validate_edge_weight(f64::NEG_INFINITY, "edge-y").unwrap_err();
    assert!(
        err.to_string().contains("not finite"),
        "expected 'not finite' in error: {err}"
    );
}

#[test]
fn validate_edge_weight_out_of_range_is_rejected() {
    let err = validate_edge_weight(1.5, "edge-z").unwrap_err();
    assert!(
        err.to_string().contains("outside the valid range"),
        "expected range error: {err}"
    );
    let err = validate_edge_weight(-0.1, "edge-z").unwrap_err();
    assert!(
        err.to_string().contains("outside the valid range"),
        "expected range error: {err}"
    );
}
