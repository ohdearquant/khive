/// The PR `author` field entered gated `properties` raw — a credential-shaped
/// GitHub login silently dropped the PR via the runtime's recursive
/// `properties` secret scan (the sibling of the already-fixed issue
/// `author_login` bug).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_pr_author_login_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-author-credential-repo"}),
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

    let fake_token = "ghp_HHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH";
    let pr_json = json!([{
        "number": 55,
        "title": "Fix pagination edge case",
        "author": {"login": fake_token},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/pagination",
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
    let stored_author = items[0]["properties"]["author"]
        .as_str()
        .expect("properties.author is string");
    assert!(
        !stored_author.contains(fake_token),
        "raw credential must not survive into properties.author: {stored_author:?}"
    );
    assert!(
        stored_author.contains("***MASKED***"),
        "masked marker must be present in properties.author: {stored_author:?}"
    );
}

/// The PR `base_ref` field entered gated `properties` raw — a
/// credential-shaped base branch name silently dropped the PR.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_pr_base_ref_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-base-ref-credential-repo"}),
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

    let fake_token = "ghp_IIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIII";
    let pr_json = json!([{
        "number": 56,
        "title": "Retarget release branch",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": fake_token,
        "headRefName": "release/next",
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
    let stored_base_ref = items[0]["properties"]["base_ref"]
        .as_str()
        .expect("properties.base_ref is string");
    assert!(
        !stored_base_ref.contains(fake_token),
        "raw credential must not survive into properties.base_ref: {stored_base_ref:?}"
    );
    assert!(
        stored_base_ref.contains("***MASKED***"),
        "masked marker must be present in properties.base_ref: {stored_base_ref:?}"
    );
}

/// The PR `head_ref` field entered gated `properties` raw — a
/// credential-shaped head branch name (realistic for a fork PR, where the
/// contributor fully controls the branch name) silently dropped the PR.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_pr_head_ref_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-head-ref-credential-repo"}),
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

    let fake_token = "ghp_JJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJ";
    let pr_json = json!([{
        "number": 57,
        "title": "Fork contribution",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": fake_token,
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
    let stored_head_ref = items[0]["properties"]["head_ref"]
        .as_str()
        .expect("properties.head_ref is string");
    assert!(
        !stored_head_ref.contains(fake_token),
        "raw credential must not survive into properties.head_ref: {stored_head_ref:?}"
    );
    assert!(
        stored_head_ref.contains("***MASKED***"),
        "masked marker must be present in properties.head_ref: {stored_head_ref:?}"
    );
}

/// Regression guard: clean (non-credential-shaped) PR author, base ref, and
/// head ref must survive byte-for-byte unchanged.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_leaves_clean_pr_author_and_refs_unmasked() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-clean-author-refs-repo"}),
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

    let pr_json = json!([{
        "number": 58,
        "title": "Improve README clarity",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "docs/readme-clarity",
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
    assert_eq!(report.prs_ingested, 1, "{report:?}");

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = list_items(&prs_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_author = item["properties"]["author"]
        .as_str()
        .expect("author is string");
    let stored_base_ref = item["properties"]["base_ref"]
        .as_str()
        .expect("base_ref is string");
    let stored_head_ref = item["properties"]["head_ref"]
        .as_str()
        .expect("head_ref is string");
    assert_eq!(stored_author, "octocat", "clean author must be unchanged");
    assert_eq!(stored_base_ref, "main", "clean base_ref must be unchanged");
    assert_eq!(
        stored_head_ref, "docs/readme-clarity",
        "clean head_ref must be unchanged"
    );
    assert!(
        !stored_author.contains("***MASKED***")
            && !stored_base_ref.contains("***MASKED***")
            && !stored_head_ref.contains("***MASKED***"),
        "no masking marker may appear on clean input: author={stored_author:?} base_ref={stored_base_ref:?} head_ref={stored_head_ref:?}"
    );
}
