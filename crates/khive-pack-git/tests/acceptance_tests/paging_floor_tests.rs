// ── raw `updatedAt` must never reach the
//    paging floor via a full page ─────────────────────────────────────────

/// Before this fix, `ingest_issues` sorted the raw `Vec<GhIssue>` and derived
/// its paging continuation floor (`last_updated_at`) from the raw,
/// pre-`MaskedIssueFields` `updated_at` before `MaskedIssueFields::new` ever
/// ran. A credential-shaped raw value sorts LAST under raw ASCII string
/// comparison (any letter-leading string outranks a digit-leading RFC3339
/// timestamp), so it could become the next `gh --search updated:>=...`
/// argument -- exposing it in process arguments -- or, once dropped by
/// canonicalization, corrupt the frozen-cursor invariant.
///
/// This test forces the vulnerable branch directly: a page of exactly
/// `PAGE_LIMIT` (1000) issues, one of which has a credential-shaped raw
/// `updatedAt` that fails RFC3339 parsing. Under the fix, the ENTIRE page is
/// masked/canonicalized before sorting, so the malformed record's
/// `updated_at` becomes `None` and sorts FIRST, never becoming
/// `last_updated_at`. Asserts the raw value never appears in any recorded
/// `gh` invocation's argv, in the persisted paging cursor, or in any
/// persisted issue record, and that a full page still triggers a
/// continuation fetch (not a false `WindowComplete`) -- pagination remains
/// resumable.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_full_page_never_leaks_raw_updated_at_into_paging_floor() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "full-page-repo"}),
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

    // Mirrors `ingest.rs`'s private `PAGE_LIMIT` -- `gh {pr,issue} list
    // --search` never returns more than this many results per page.
    const PAGE_LIMIT: usize = 1000;
    const CREDENTIAL: &str = concat!(
        "sk-ant-api03-",
        "FAKE1234567890",
        "FAKE1234567890",
        "FAKE1234567890",
        "FAKE1234567890",
        "FAKE1234567890",
        "FAKE1234567890",
        "FAKE1234567890",
        "FAKE1234567890",
        "AA"
    );

    let mut issues: Vec<Value> = (1..PAGE_LIMIT)
        .map(|i| {
            let minute = i / 60;
            let second = i % 60;
            json!({
                "number": i,
                "title": format!("issue {i}"),
                "author": {"login": "a"},
                "createdAt": "2026-01-01T00:00:00Z",
                "closedAt": null,
                "updatedAt": format!("2026-01-01T{minute:02}:{second:02}:00Z"),
                "labels": [],
                "stateReason": "",
                "body": ""
            })
        })
        .collect();
    // Raw ASCII compare: 's' (0x73) outranks every digit (0x30-0x39), so
    // this record's raw `updatedAt` sorts after all 999 valid timestamps
    // above -- exactly the "sorts last" shape the finding describes.
    issues.push(json!({
        "number": PAGE_LIMIT,
        "title": "issue with malformed updatedAt",
        "author": {"login": "a"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": CREDENTIAL,
        "labels": [],
        "stateReason": "",
        "body": ""
    }));
    assert_eq!(
        issues.len(),
        PAGE_LIMIT,
        "page must be exactly PAGE_LIMIT-sized to force the continuation branch"
    );
    let issue_json = Value::Array(issues).to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.pull_requests = false;
    opts.include.commits = false;

    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, PAGE_LIMIT as u64,
        "every record lands -- the malformed record only loses its updated_at field: {report:?}"
    );

    let args_log = std::fs::read_to_string(log_dir.join("args.log")).expect("read args.log");
    assert!(
        !args_log.contains(CREDENTIAL),
        "the raw credential-shaped updatedAt must never reach a gh invocation's argv \
         (paging floor leak): {args_log}"
    );
    // A full (PAGE_LIMIT-sized) page must trigger a second `gh issue list`
    // invocation -- proving pagination did not silently treat the full page
    // as window-complete.
    let issue_invocations = args_log.lines().filter(|l| l.starts_with("issue ")).count();
    assert!(
        issue_invocations >= 2,
        "a full page must be followed by a continuation fetch: {args_log}"
    );

    let cursor = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written");
    assert!(
        !cursor.contains(CREDENTIAL),
        "the persisted paging cursor must never contain the raw credential value: {cursor}"
    );

    // `list` uses the same `{items, effective_limit, ..}` envelope on both
    // sides of the cap, so scan every persisted issue by paging.
    let mut scanned = 0usize;
    let mut offset = 0u64;
    loop {
        let page = registry
            .dispatch(
                "list",
                json!({"kind": "issue", "limit": (PAGE_LIMIT + 10) as u64, "offset": offset}),
            )
            .await
            .expect("list issues ok");
        let items = list_items(&page);
        assert!(
            items.iter().all(|i| !i.to_string().contains(CREDENTIAL)),
            "no persisted issue record may contain the raw credential value"
        );
        scanned += items.len();
        if items.is_empty() {
            break;
        }
        offset += items.len() as u64;
    }
    assert!(
        scanned >= PAGE_LIMIT,
        "paging scan must cover every persisted issue: scanned {scanned}"
    );
}

/// PR pagination must canonicalize the complete remote page before sorting
/// it or deriving the next inclusive `updated:>=...` floor. This is the PR
/// counterpart of `issue_full_page_never_leaks_raw_updated_at_into_paging_floor`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn pr_full_page_never_leaks_raw_updated_at_into_paging_floor() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-full-page-repo"}),
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

    const PAGE_LIMIT: usize = 1000;
    const CREDENTIAL: &str = concat!(
        "sk-ant-api03-",
        "FAKE0987654321",
        "FAKE0987654321",
        "FAKE0987654321",
        "FAKE0987654321",
        "FAKE0987654321",
        "FAKE0987654321",
        "FAKE0987654321",
        "FAKE0987654321",
        "AA"
    );

    let mut prs: Vec<Value> = (1..PAGE_LIMIT)
        .map(|i| {
            let hour = i / 60;
            let minute = i % 60;
            json!({
                "number": i,
                "title": format!("pull request {i}"),
                "author": {"login": "a"},
                "createdAt": "2026-01-01T00:00:00Z",
                "mergedAt": null,
                "closedAt": null,
                "updatedAt": format!("2026-01-01T{hour:02}:{minute:02}:00Z"),
                "baseRefName": "main",
                "headRefName": format!("test/pr-{i}"),
                "mergeCommit": null,
                "body": ""
            })
        })
        .collect();
    prs.push(json!({
        "number": PAGE_LIMIT,
        "title": "pull request with malformed updatedAt",
        "author": {"login": "a"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": CREDENTIAL,
        "baseRefName": "main",
        "headRefName": "test/malformed-updated-at",
        "mergeCommit": null,
        "body": ""
    }));
    assert_eq!(prs.len(), PAGE_LIMIT);
    let pr_json = Value::Array(prs).to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.issues = false;
    opts.include.commits = false;
    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, PAGE_LIMIT as u64,
        "every PR lands; only the malformed updatedAt is dropped: {report:?}"
    );
    assert!(
        !format!("{report:?}").contains(CREDENTIAL),
        "warnings/report must never echo the rejected remote value: {report:?}"
    );

    let args_log = std::fs::read_to_string(log_dir.join("args.log")).expect("read args.log");
    assert!(
        !args_log.contains(CREDENTIAL),
        "the raw updatedAt must never become a PR paging argv value: {args_log}"
    );
    let pr_invocations = args_log
        .lines()
        .filter(|line| line.starts_with("pr "))
        .count();
    assert!(
        pr_invocations >= 2,
        "a full PR page must trigger a continuation fetch: {args_log}"
    );

    let cursor = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("PR cursor must be written");
    assert!(
        !cursor.contains(CREDENTIAL),
        "the persisted PR cursor must never contain the raw updatedAt: {cursor}"
    );

    let mut scanned = 0usize;
    let mut offset = 0u64;
    loop {
        let page = registry
            .dispatch(
                "list",
                json!({"kind": "pull_request", "limit": (PAGE_LIMIT + 10) as u64, "offset": offset}),
            )
            .await
            .expect("list PRs ok");
        let items = list_items(&page);
        assert!(
            items
                .iter()
                .all(|item| !item.to_string().contains(CREDENTIAL)),
            "no persisted PR record may contain the raw updatedAt"
        );
        scanned += items.len();
        if items.is_empty() {
            break;
        }
        offset += items.len() as u64;
    }
    assert!(
        scanned >= PAGE_LIMIT,
        "paging scan must cover every persisted PR: scanned {scanned}"
    );
}
