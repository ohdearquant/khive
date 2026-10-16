#[test]
fn independent_commits_only_builds_are_byte_identical() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let first = tmp.path().join("first.json");
    let second = tmp.path().join("second.json");

    let first_run = build(
        &home,
        &repo,
        &tmp.path().join("work-first"),
        &first,
        "commits",
    );
    assert!(
        first_run.status.success(),
        "first build failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&first_run.stdout),
        String::from_utf8_lossy(&first_run.stderr)
    );
    let first_report: serde_json::Value =
        serde_json::from_slice(&first_run.stdout).expect("build report JSON");
    assert_eq!(first_report["head_sha"], git(&repo, &["rev-parse", "HEAD"]));
    assert_eq!(
        first_report["digest"]["sources"]["commits"]["state"],
        "completed"
    );
    assert!(first_report["digest"]["sources"]["issues"].is_null());

    let second_run = build(
        &home,
        &repo,
        &tmp.path().join("work-second"),
        &second,
        "commits",
    );
    assert!(
        second_run.status.success(),
        "second build failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&second_run.stdout),
        String::from_utf8_lossy(&second_run.stderr)
    );
    assert_eq!(
        std::fs::read(&first).expect("first bundle"),
        std::fs::read(&second).expect("second bundle"),
        "random storage UUIDs or ingest clocks leaked into canonical bundle bytes"
    );
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(first).expect("bundle bytes")).expect("bundle JSON");
    assert_eq!(bundle["schema_version"], "khive.repo.v1");
    assert_eq!(
        bundle["meta"]["ingest"]["git_digest"]["status"],
        "available"
    );
    assert_eq!(
        bundle["meta"]["ingest"]["git_digest"]["value"]["sources"]["issues"]["state"],
        "unrequested"
    );
    assert_eq!(
        bundle["meta"]["ingest"]["code_ingest"]["value"]["languages"],
        serde_json::json!(["rust"])
    );
    assert_eq!(
        bundle["meta"]["ingest"]["clone_tags"]["state"],
        "unrequested"
    );
    assert_eq!(
        bundle["meta"]["repository"]["default_branch"]["value"],
        "main"
    );
    let resolution = &bundle["graph"]["join_resolution"]["repositories"]["value"][0];
    assert_eq!(resolution["files"], 2);
    assert_eq!(resolution["derived_keys"], 2);
    assert_eq!(resolution["entity_keys"], 2);
    assert_eq!(resolution["matched"], 2);
    assert_eq!(resolution["resolution_rate"]["value"], 1.0);
    assert_eq!(resolution["residuals"]["items"], serde_json::json!([]));
    let historical = &bundle["graph"]["join_resolution"]["historical"]["value"][0];
    assert_eq!(historical["total_changed_paths"], 3);
    assert_eq!(historical["rust_in_scope_paths"], 2);
    assert_eq!(historical["matched_rust_paths"], 2);
    assert_eq!(historical["out_of_scope_paths"], 1);
    assert_eq!(
        historical["unresolved_rust_paths"]["items"],
        serde_json::json!([])
    );
}

#[test]
fn export_reads_existing_stores_without_running_ingest() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let work = tmp.path().join("work");
    let built = tmp.path().join("built.json");
    let build_run = build(&home, &repo, &work, &built, "commits");
    assert!(
        build_run.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build_run.stderr)
    );

    let exported = tmp.path().join("exported.json");
    let history_db = work.join("history.db");
    let map_db = work.join("code-map.db");
    let output = isolated_command(&home)
        .args([
            "repo",
            "export",
            "--repo",
            repo.to_str().unwrap(),
            "--history-db",
            history_db.to_str().unwrap(),
            "--map-db",
            map_db.to_str().unwrap(),
            "--repository-url",
            "https://github.com/example/showcase-fixture",
            "--generated-at",
            "2026-08-07T16:00:00Z",
            "--out",
            exported.to_str().unwrap(),
        ])
        .output()
        .expect("run repo export");
    assert!(
        output.status.success(),
        "export failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(exported.is_file());
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(exported).expect("exported bundle"))
            .expect("exported bundle JSON");
    assert_eq!(
        bundle["meta"]["ingest"]["git_digest"]["status"],
        "unavailable"
    );
    assert_eq!(
        bundle["meta"]["repository"]["default_branch"]["status"],
        "unavailable"
    );

    let history_before = std::fs::read(&history_db).expect("history database before collision");
    let collision = isolated_command(&home)
        .args([
            "repo",
            "export",
            "--repo",
            repo.to_str().unwrap(),
            "--history-db",
            history_db.to_str().unwrap(),
            "--map-db",
            map_db.to_str().unwrap(),
            "--repository-url",
            "https://github.com/example/showcase-fixture",
            "--generated-at",
            "2026-08-07T16:00:00Z",
            "--out",
            history_db.to_str().unwrap(),
        ])
        .output()
        .expect("run colliding repo export");
    assert!(!collision.status.success());
    assert!(String::from_utf8_lossy(&collision.stderr).contains("must not overwrite"));
    assert_eq!(
        std::fs::read(&history_db).expect("history database after collision"),
        history_before
    );
}
