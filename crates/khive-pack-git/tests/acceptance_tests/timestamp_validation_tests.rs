/// PR #835: `created_at`/`closed_at` bypassed
/// `MaskedIssueFields` entirely and entered `properties` as arbitrary raw
/// strings, so a credential-shaped `createdAt` tripped the runtime's
/// recursive secret-gate scan on `properties` and silently dropped the
/// whole issue. The fix parses every issue timestamp into a canonical
/// RFC3339 form (or rejects it) before it ever reaches `properties` -- a
/// credential-shaped value is not a valid timestamp, so it is rejected
/// (becomes `null`) rather than persisted raw, and the issue itself must
/// still be ingested.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_rejects_credential_shaped_issue_created_at_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-created-at-credential-repo"}),
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

    let fake_token = "ghp_DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";
    let issue_json = json!([{
        "number": 20,
        "title": "Investigate credential-shaped createdAt",
        "author": {"login": "octocat"},
        "createdAt": fake_token,
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
        "an unparseable createdAt must not drop the issue: {report:?}"
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert!(
        item["properties"]["created_at"].is_null(),
        "an unparseable createdAt must be rejected, not persisted raw: {:?}",
        item["properties"]["created_at"]
    );
    let dumped = item.to_string();
    assert!(
        !dumped.contains(fake_token),
        "raw credential-shaped createdAt must not survive anywhere in the stored record: {dumped}"
    );
}

/// Sibling of the `createdAt` regression above, for `closedAt` (ingest.rs
/// line ~1550).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_rejects_credential_shaped_issue_closed_at_without_dropping_note() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-closed-at-credential-repo"}),
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

    let fake_token = "ghp_FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
    let issue_json = json!([{
        "number": 21,
        "title": "Investigate credential-shaped closedAt",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": fake_token,
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
        "an unparseable closedAt must not drop the issue: {report:?}"
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert!(
        item["properties"]["closed_at"].is_null(),
        "an unparseable closedAt must be rejected, not persisted raw: {:?}",
        item["properties"]["closed_at"]
    );
    let dumped = item.to_string();
    assert!(
        !dumped.contains(fake_token),
        "raw credential-shaped closedAt must not survive anywhere in the stored record: {dumped}"
    );
}

/// `updatedAt` is not stored in `properties` at all -- it is only used to
/// advance the paging cursor (ingest.rs line ~1632).
/// A credential-shaped `updatedAt` must still not drop the issue, and the
/// cursor persisted to `git_mirror_cursor` must never contain the raw
/// value; it must advance only from a sibling record's validated,
/// canonicalized timestamp.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_rejects_credential_shaped_issue_updated_at_and_cursor_never_persists_raw_value() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-updated-at-credential-repo"}),
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

    let fake_token = "ghp_EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE";
    let issue_json = json!([
        {
            "number": 22,
            "title": "Credential-shaped updatedAt",
            "author": {"login": "octocat"},
            "createdAt": "2026-01-01T00:00:00Z",
            "closedAt": null,
            "updatedAt": fake_token,
            "labels": [],
            "stateReason": "",
            "body": ""
        },
        {
            "number": 23,
            "title": "Clean sibling issue",
            "author": {"login": "octocat"},
            "createdAt": "2026-01-02T00:00:00Z",
            "closedAt": null,
            "updatedAt": "2026-01-02T00:00:00Z",
            "labels": [],
            "stateReason": "",
            "body": ""
        }
    ])
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
        report.issues_ingested, 2,
        "an unparseable updatedAt must not drop the issue: {report:?}"
    );

    let cursor = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written from the sibling issue's valid updatedAt");
    assert!(
        !cursor.contains(fake_token),
        "the paging cursor must never persist a raw credential-shaped updatedAt: {cursor:?}"
    );
    assert!(
        cursor.starts_with("2026-01-02"),
        "the cursor must advance to the sibling issue's valid, canonicalized updatedAt: {cursor:?}"
    );
}

/// The PR paging boundary has the same trust requirement as issues: a raw
/// `updatedAt` is remote input, even though it is not persisted on the PR
/// note. A malformed value is dropped before sorting, while a clean sibling
/// supplies the only durable/resumable cursor value.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_rejects_credential_shaped_pr_updated_at_and_cursor_never_persists_raw_value() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-updated-at-credential-repo"}),
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

    let fake_token = "ghp_FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
    let pr_json = json!([
        {
            "number": 24,
            "title": "Credential-shaped updatedAt",
            "author": {"login": "octocat"},
            "createdAt": "2026-01-01T00:00:00Z",
            "mergedAt": null,
            "closedAt": null,
            "updatedAt": fake_token,
            "baseRefName": "main",
            "headRefName": "fix/malformed-updated-at",
            "mergeCommit": null,
            "body": ""
        },
        {
            "number": 25,
            "title": "Clean sibling PR",
            "author": {"login": "octocat"},
            "createdAt": "2026-01-02T00:00:00Z",
            "mergedAt": null,
            "closedAt": null,
            "updatedAt": "2026-01-02T00:00:00Z",
            "baseRefName": "main",
            "headRefName": "fix/clean-sibling",
            "mergeCommit": null,
            "body": ""
        }
    ])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.issues = false;
    opts.include.commits = false;
    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 2,
        "an unparseable updatedAt must not drop the PR: {report:?}"
    );
    assert!(
        !format!("{report:?}").contains(fake_token),
        "warnings/report must never echo the rejected remote value: {report:?}"
    );

    let cursor = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written from the sibling PR's valid updatedAt");
    assert!(
        !cursor.contains(fake_token),
        "the paging cursor must never persist a raw credential-shaped updatedAt: {cursor:?}"
    );
    assert!(
        cursor.starts_with("2026-01-02"),
        "the cursor must advance to the sibling PR's valid, canonicalized updatedAt: {cursor:?}"
    );

    let args_log = std::fs::read_to_string(log_dir.join("args.log")).expect("read args.log");
    assert!(
        !args_log.contains(fake_token),
        "the rejected updatedAt must never enter a gh invocation's argv: {args_log}"
    );
}
