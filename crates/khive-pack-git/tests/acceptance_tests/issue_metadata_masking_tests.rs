/// Issue #801: a credential-shaped issue title (sibling site of #763/#785)
/// must be masked in place rather than causing the whole issue note to be
/// rejected by the runtime's recursive `properties` secret scan.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_issue_title_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-title-credential-repo"}),
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
    let issue_json = json!([{
        "number": 11,
        "title": format!("Rotate leaked {fake_token} immediately"),
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
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
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #11")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
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
}

/// Issue #977: a legitimate issue whose title mentions a credential-like
/// word (e.g. "key", "auth", "token") while the body separately contains an
/// unrelated full UUID must not be write-blocked by the `uuid-near-trigger`
/// heuristic. `MaskedIssueFields` masks `title` and `body` independently
/// before the note is ever created, so a trigger word in one field cannot
/// "arm" a UUID in the other -- covers the exact cross-field shape reported
/// live against `git.digest`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_does_not_block_issue_with_credential_word_in_title_and_uuid_in_body() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-977-cross-field-repo"}),
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

    let issue_json = json!([{
        "number": 977,
        "title": "Rotate the api_key configuration",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": "See tracking record 550e8400-e29b-41d4-a716-446655440000 for details."
    }])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
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
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #977")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
    assert_eq!(items.len(), 1);
    let stored_content = items[0]["content"].as_str().expect("content is string");
    assert!(
        stored_content.contains("550e8400-e29b-41d4-a716-446655440000"),
        "an unrelated UUID with no trigger word nearby in its own field must \
         survive verbatim, not be masked away: {stored_content:?}"
    );
}

/// Issue #977 sibling case: the credential-like word and the UUID appear
/// together in the SAME field (the body). `mask_secrets` -- not the hard
/// `check()` gate -- is what `ingest_issues` applies to `body`, so the
/// ambiguous span is redacted in place and the issue still lands, rather
/// than the whole note being dropped with "reword the source" advice that
/// makes no sense for content the ingester does not own.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_word_and_uuid_co_occurring_in_issue_body() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-977-same-field-repo"}),
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

    let issue_json = json!([{
        "number": 978,
        "title": "Investigate flaky ingest",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": "We rotated the auth key, tracking record 550e8400-e29b-41d4-a716-446655440000 for details."
    }])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
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
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #978")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
    assert_eq!(items.len(), 1);
    let stored_content = items[0]["content"].as_str().expect("content is string");
    assert!(
        !stored_content.contains("550e8400-e29b-41d4-a716-446655440000"),
        "the ambiguous UUID-near-trigger span must be masked, not stored raw: {stored_content:?}"
    );
    assert!(
        stored_content.contains("***MASKED***"),
        "masked marker must be present in place of the ambiguous span: {stored_content:?}"
    );
}

/// A clean (non-credential) issue title and an empty body must pass through
/// `ingest_issues` unchanged -- guards against a future over-aggressive
/// masking regression that the detector-positive test above cannot catch.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_leaves_clean_issue_title_unmasked() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-clean-title-repo"}),
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

    let issue_json = json!([{
        "number": 12,
        "title": "Fix the flaky retry test",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(report.issues_ingested, 1, "{report:?}");

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(
        item["name"].as_str().unwrap(),
        "#12 Fix the flaky retry test"
    );
    assert_eq!(
        item["properties"]["title"].as_str().unwrap(),
        "Fix the flaky retry test"
    );
}

/// PR #835: the runtime secret gate recursively scans
/// every string in `properties` (not just `title`), so a credential-shaped
/// label name previously tripped the gate on `create()` even after the
/// title was masked, silently dropping the whole issue note.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_issue_label_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-label-credential-repo"}),
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

    let fake_token = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    let issue_json = json!([{
        "number": 13,
        "title": "Rotate the leaked deploy key",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [{"name": fake_token}],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
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
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #13")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_labels = item["properties"]["labels"]
        .as_array()
        .expect("properties.labels is array");
    assert_eq!(stored_labels.len(), 1);
    let stored_label = stored_labels[0].as_str().expect("label is string");
    assert!(
        !stored_label.contains(fake_token),
        "raw credential must not survive into properties.labels: {stored_label:?}"
    );
    assert!(
        stored_label.contains("***MASKED***"),
        "masked marker must be present in properties.labels: {stored_label:?}"
    );
}

/// Same finding as above, for the author login field: a credential-shaped
/// login previously tripped the recursive secret gate on `properties` and
/// silently dropped the issue note.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_issue_author_login_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-login-credential-repo"}),
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

    let fake_token = "ghp_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
    let issue_json = json!([{
        "number": 14,
        "title": "Investigate flaky CI runner",
        "author": {"login": fake_token},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
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
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #14")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_author = item["properties"]["author"]
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
