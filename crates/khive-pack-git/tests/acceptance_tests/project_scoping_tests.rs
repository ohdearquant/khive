// ── project-scoped idempotency ──────────────────────────────────────────

/// GitHub issue/PR numbers are repository-scoped: two different `project`
/// entities can each have a `#1`. Both a direct `find_by_number` collision
/// and the commit ingester's squash-merge-suffix PR fallback must resolve
/// within the ingesting project only, never across projects.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_and_pr_idempotency_is_scoped_per_project() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_a = create(&registry, json!({"kind": "project", "name": "repo-a"})).await;
    let project_b = create(&registry, json!({"kind": "project", "name": "repo-b"})).await;

    // Project A already has issue #1 and PR #1 (as if ingested in a prior pass).
    let pr_a = create(
        &registry,
        json!({
            "kind": "pull_request",
            "content": "",
            "properties": {"number": 1, "title": "A#1", "project_id": project_a.to_string()},
            "annotates": [project_a.to_string()],
        }),
    )
    .await;
    let issue_a = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 1, "title": "issue A#1", "project_id": project_a.to_string()},
            "annotates": [project_a.to_string()],
        }),
    )
    .await;

    // Project B's fixture repo has its own squash-merge commit suffixed
    // "(#1)" -- before project B has any PR #1 of its own, the fallback must
    // NOT resolve to project A's PR #1.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo_b: PathBuf = dir.path().to_path_buf();
    init_repo(&repo_b);
    write(&repo_b, "README.md", "b\n");
    commit(&repo_b, &["README.md"], "Add repo-b feature (#1)");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo_b.clone(), project_b.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");
    assert_eq!(report.commits_ingested, 1);

    let pr_a_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": pr_a.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    assert_eq!(
        pr_a_neighbors.as_array().expect("array").len(),
        0,
        "project B's commit must never attach to project A's PR #1: {pr_a_neighbors:?}"
    );

    // Directly create project B's own issue #1 and PR #1 (both numbered the
    // same as project A's), then ingest a second squash-merge commit -- the
    // fallback must now resolve within project B.
    let pr_b = create(
        &registry,
        json!({
            "kind": "pull_request",
            "content": "",
            "properties": {"number": 1, "title": "B#1", "project_id": project_b.to_string()},
            "annotates": [project_b.to_string()],
        }),
    )
    .await;
    let issue_b = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 1, "title": "issue B#1", "project_id": project_b.to_string()},
            "annotates": [project_b.to_string()],
        }),
    )
    .await;
    assert_ne!(pr_a, pr_b, "both projects' #1 PRs are distinct records");
    assert_ne!(
        issue_a, issue_b,
        "both projects' #1 issues are distinct records"
    );

    write(&repo_b, "src/lib.rs", "// b2\n");
    commit(&repo_b, &["src/lib.rs"], "Add another repo-b feature (#1)");

    run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo_b.clone(), project_b.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");

    let pr_b_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": pr_b.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    assert_eq!(
        pr_b_neighbors.as_array().expect("array").len(),
        1,
        "project B's own PR #1 resolves the squash-merge fallback: {pr_b_neighbors:?}"
    );
    let pr_a_neighbors_after = registry
        .dispatch(
            "neighbors",
            json!({"id": pr_a.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    assert_eq!(
        pr_a_neighbors_after.as_array().expect("array").len(),
        0,
        "project A's PR #1 remains untouched: {pr_a_neighbors_after:?}"
    );
}

// ── gh boundary contract + per-record warning aggregation ───────────────────

/// End-to-end over a PATH-shadowing fake `gh`: locks the four demo-found
/// regression classes ((a) no `-C`, correct cwd; (b) empty `stateReason`
/// omitted; (c) uppercase enum values lowercased; (d) all four governed
/// values accepted), asserts `pull_request` properties use `base_ref`/
/// `head_ref` (not `base`/`head`), and asserts one ungoverned-`state_reason`
/// issue between two valid ones aborts only its own record (one warning,
/// both neighbors still land) rather than the whole ingest pass.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn gh_boundary_contract_and_partial_ingest_failure() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "gh-boundary-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");
    let repo_canon = repo.canonicalize().expect("canonicalize repo");

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");

    let pr_json = json!([{
        "number": 99,
        "title": "Add feature",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": "2026-01-02T00:00:00Z",
        "closedAt": "2026-01-02T00:00:00Z",
        "updatedAt": "2026-01-02T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "feature/x",
        "mergeCommit": null,
        "body": "PR body"
    }])
    .to_string();

    // 6 issues, ordered good, good, BAD, good, good, good -- #3's ungoverned
    // `stateReason` must warn-and-skip without aborting #4/#5/#6, and the two
    // good records sandwiching it (#2, #4) must both land.
    let issue_json = json!([
        {"number": 1, "title": "i1", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-01T00:00:01Z", "labels": [], "stateReason": "", "body": ""},
        {"number": 2, "title": "i2", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:02Z", "updatedAt": "2026-01-01T00:00:02Z", "labels": [], "stateReason": "NOT_PLANNED", "body": ""},
        {"number": 3, "title": "i3-bad", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:03Z", "updatedAt": "2026-01-01T00:00:03Z", "labels": [], "stateReason": "WONTFIX", "body": ""},
        {"number": 4, "title": "i4", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:04Z", "updatedAt": "2026-01-01T00:00:04Z", "labels": [], "stateReason": "COMPLETED", "body": ""},
        {"number": 5, "title": "i5", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:05Z", "updatedAt": "2026-01-01T00:00:05Z", "labels": [], "stateReason": "REOPENED", "body": ""},
        {"number": 6, "title": "i6", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:06Z", "updatedAt": "2026-01-01T00:00:06Z", "labels": [], "stateReason": "DUPLICATE", "body": ""}
    ])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, &pr_json, &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");

    assert_eq!(
        report.gh_available,
        Some(true),
        "fake gh must be probed and found on PATH: {report:?}"
    );
    assert_eq!(report.prs_ingested, 1, "{report:?}");
    assert_eq!(
        report.issues_ingested, 5,
        "5 of 6 issues land, #3 warns-and-skips: {report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("issue #3"))
            .count(),
        1,
        "exactly one warning names the ungoverned record: {:?}",
        report.warnings
    );

    // (a) gh is never invoked with -C, and always runs with the repo as cwd.
    let args_log = std::fs::read_to_string(log_dir.join("args.log")).expect("read args.log");
    assert!(
        !args_log.contains("-C"),
        "gh must never receive an unsupported -C flag: {args_log}"
    );
    // ADR-088 Amendment 1: the paging rewrite (--search
    // "sort:updated-asc ...") must not silently drop --state all -- gh
    // defaults to open-only listing, which would make closed issues and
    // closed/merged PRs vanish from every ingest.
    let pr_and_issue_invocations: Vec<&str> = args_log
        .lines()
        .filter(|l| l.starts_with("pr ") || l.starts_with("issue "))
        .collect();
    assert!(
        !pr_and_issue_invocations.is_empty(),
        "expected at least one gh pr/issue list invocation: {args_log}"
    );
    for line in &pr_and_issue_invocations {
        assert!(
            line.contains("--state all"),
            "every gh pr/issue list invocation must request --state all: {line:?}"
        );
        assert_eq!(
            repo_flag_value(line),
            Some("fixture/repository"),
            "every gh pr/issue list invocation must explicitly pin the probed repo: {line:?}"
        );
    }
    let cwd_log = std::fs::read_to_string(log_dir.join("cwd.log")).expect("read cwd.log");
    let cwd_lines: Vec<&str> = cwd_log.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !cwd_lines.is_empty(),
        "gh must have been invoked at least once"
    );
    for line in &cwd_lines {
        let logged = Path::new(line)
            .canonicalize()
            .expect("canonicalize logged cwd");
        assert_eq!(
            logged, repo_canon,
            "every gh invocation must run with the repo as its current_dir"
        );
    }

    // (b)/(c)/(d): pull_request properties use base_ref/head_ref.
    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 20}))
        .await
        .expect("list issues ok");
    let issue_items = list_items(&issues_list);
    let issue_by_number = |n: u64| -> &Value {
        issue_items
            .iter()
            .find(|i| i["properties"]["number"].as_u64() == Some(n))
            .unwrap_or_else(|| panic!("issue #{n} must be stored: {issue_items:?}"))
    };
    assert!(
        issue_by_number(1)["properties"]
            .get("state_reason")
            .is_none(),
        "empty stateReason must be omitted, not stored as \"\""
    );
    assert_eq!(
        issue_by_number(2)["properties"]["state_reason"],
        "not_planned"
    );
    assert_eq!(
        issue_by_number(4)["properties"]["state_reason"],
        "completed"
    );
    assert_eq!(issue_by_number(5)["properties"]["state_reason"], "reopened");
    assert_eq!(
        issue_by_number(6)["properties"]["state_reason"],
        "duplicate"
    );
    assert!(
        !issue_items
            .iter()
            .any(|i| i["properties"]["number"].as_u64() == Some(3)),
        "the ungoverned record must never land: {issue_items:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let pr_items = list_items(&prs_list);
    let pr99 = pr_items
        .iter()
        .find(|i| i["properties"]["number"].as_u64() == Some(99))
        .expect("PR #99 must be stored");
    assert_eq!(pr99["properties"]["base_ref"], "main");
    assert_eq!(pr99["properties"]["head_ref"], "feature/x");
    assert!(
        pr99["properties"].get("base").is_none() && pr99["properties"].get("head").is_none(),
        "ADR-088 names these base_ref/head_ref, not base/head: {pr99:?}"
    );

    // Second pass: the frozen cursor retries #3 (fails again) without
    // re-creating any already-landed record.
    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");
    assert_eq!(
        report2.issues_ingested, 0,
        "already-landed issues are found by natural key, not re-created: {report2:?}"
    );
    assert_eq!(
        report2
            .warnings
            .iter()
            .filter(|w| w.contains("issue #3"))
            .count(),
        1,
        "the frozen cursor retries the failed record every pass, not just once: {:?}",
        report2.warnings
    );
}
