/// Coordinator addendum requirement: a commit message containing a
/// credential-shaped token must be masked before it is stored.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_secrets_in_commit_message() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(&registry, json!({"kind": "project", "name": "secret-repo"})).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    write(repo, "README.md", "hello\n");
    commit(
        repo,
        &["README.md"],
        &format!("Rotate deploy key\n\naccidentally committed {fake_token} here"),
    );

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1);

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list_items(&list);
    assert_eq!(items.len(), 1);
    let stored_content = items[0]["content"].as_str().expect("content is string");
    assert!(
        !stored_content.contains(fake_token),
        "raw token must not survive into stored content: {stored_content:?}"
    );
    assert!(
        stored_content.contains("***MASKED***") || stored_content.contains("MASKED"),
        "masked marker must be present: {stored_content:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_secret_shaped_changed_paths() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "secret-path-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let rel = format!("src/{fake_token}.rs");
    write(repo, &rel, "pub fn safe() {}\n");
    commit(repo, &[rel.as_str()], "Add source file");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1, "{report:?}");
    assert_eq!(report.writes_refused, 0, "{report:?}");

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let commit = &list_items(&commits)[0];
    let stored_path = commit["properties"]["changed_paths"][0]
        .as_str()
        .expect("changed path");
    assert!(!stored_path.contains(fake_token), "{stored_path:?}");
    assert!(stored_path.contains("MASKED"), "{stored_path:?}");
}

/// Issue #763 exact acceptance repro: a PR body containing a bare 64-char hex
/// hash in the same sentence as the standalone word "token" must ingest with only the flagged
/// span masked — the containing PR note (and its surrounding prose) must be
/// retained, not dropped.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_pr_body_hash_near_token_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-hash-near-token-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");

    let hex64 = "deadbeef".repeat(8);
    assert_eq!(hex64.len(), 64, "fixture hash must be exactly 64 hex chars");
    let pr_json = json!([{
        "number": 42,
        "title": "Document the rotation process",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "docs/rotation",
        "mergeCommit": null,
        "body": format!("Rotated the deploy token, old hash was {hex64} before rotation.")
    }])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("pull_request")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = list_items(&prs_list);
    assert_eq!(items.len(), 1);
    let content = items[0]["content"].as_str().expect("content is string");
    assert!(
        !content.contains(&hex64),
        "raw 64-hex hash must not survive into stored content: {content:?}"
    );
    let expected = "Rotated the deploy token, old hash was ***MASKED*** before rotation.";
    assert_eq!(
        content, expected,
        "only the flagged hash span is replaced, surrounding prose is retained exactly: {content:?}"
    );
    let report_debug = format!("{report:?}");
    assert!(
        !report_debug.contains(&hex64),
        "the report itself must not carry the raw detected hash: {report_debug}"
    );
}

/// A credential-shaped PR title (not just body) must also be masked in place
/// rather than causing the whole PR note to be rejected by the runtime's
/// recursive `properties` secret scan.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_pr_title_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-title-credential-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");

    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let pr_json = json!([{
        "number": 7,
        "title": format!("Rotate leaked {fake_token} immediately"),
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/rotate",
        "mergeCommit": null,
        "body": ""
    }])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("pull_request")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = list_items(&prs_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");
    assert!(
        !stored_name.contains(fake_token) && !stored_title.contains(fake_token),
        "raw credential must not survive into name or properties.title: \
         name={stored_name:?}, title={stored_title:?}"
    );
    assert!(
        stored_name.contains("***MASKED***") && stored_title.contains("***MASKED***"),
        "masked marker must be present in both name and properties.title: \
         name={stored_name:?}, title={stored_title:?}"
    );
    let stored_content = item["content"].as_str().expect("content is string");
    assert_eq!(
        stored_content, "",
        "an empty body must remain empty, not acquire a masking placeholder: {stored_content:?}"
    );
}
