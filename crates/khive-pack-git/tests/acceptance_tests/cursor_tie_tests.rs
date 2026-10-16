// ── equal-`updated_at` ties must not strand a failed record ─────────────────

/// Sorting alone (the earlier fix) does not cover a TIE: if a successful
/// record and a failing record share the exact same `updated_at`, the
/// success still advances the cursor to that shared timestamp, and an
/// exclusive `updated > cursor` retry check would then see the failed
/// record's `updated_at == cursor` on the next pass and treat it as
/// not-new — stranding it forever even with correct sort order. `is_new` is
/// now inclusive (`updated >= cursor`), so every record AT the cursor
/// timestamp is re-examined every pass until the cursor moves past it; the
/// already-landed one is a cheap no-op via the natural-key lookup.
///
/// Fixture: #5 (good) and #20 (bad — ungoverned `stateReason`) share the
/// identical `updatedAt`, #5 first in gh's raw output so it lands before
/// #20 fails and freezes the cursor at that shared timestamp. Pass 2 (after
/// #20's `stateReason` is corrected) must retry and land #20 without
/// duplicating #5.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_ingest_retries_tie_at_cursor_timestamp() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-tie-repo"}),
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

    const TIE_AT: &str = "2026-02-01T00:00:00Z";
    let issue_json = |state_reason: &str| {
        json!([
            {"number": 5, "title": "i5-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": TIE_AT, "labels": [], "stateReason": "", "body": ""},
            {"number": 20, "title": "i20-bad-tied", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": TIE_AT, "labels": [], "stateReason": state_reason, "body": ""}
        ])
        .to_string()
    };

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json("WONTFIX"));
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
        report.issues_ingested, 1,
        "#5 lands, #20 (tied timestamp, ungoverned stateReason) warns-and-skips: {report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("issue #20"))
            .count(),
        1,
        "exactly one warning names the ungoverned record: {:?}",
        report.warnings
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, TIE_AT,
        "cursor freezes at #5's timestamp, which is the SAME as failed #20's — \
         the exact tie the exclusive `updated > cursor` check used to strand \
         #20 on: {cursor_after_pass1:?}"
    );

    // Upstream correction: #20's stateReason is fixed to a governed value —
    // pass 2 must retry the tied record and land it without duplicating #5.
    std::fs::write(log_dir.join("issue_response.json"), issue_json("completed"))
        .expect("rewrite issue fixture with corrected stateReason");

    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");

    assert_eq!(
        report2.issues_ingested, 1,
        "only #20 (now corrected) is newly created on pass 2, proving the \
         tied timestamp did not strand it: {report2:?}"
    );
    assert_eq!(
        report2.issues_skipped_existing, 0,
        "#5 is acknowledged at the tied cursor; no duplicate or repeat lookup: {report2:?}"
    );
    assert!(
        report2.warnings.iter().all(|w| !w.contains("issue #20")),
        "#20 must not warn once its stateReason is corrected: {:?}",
        report2.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 20}))
        .await
        .expect("list issues ok");
    let numbers: Vec<u64> = list_items(&issues_list)
        .iter()
        .filter_map(|i| i["properties"]["number"].as_u64())
        .collect();
    assert_eq!(
        numbers.len(),
        2,
        "exactly #5, #20 — no duplicates: {numbers:?}"
    );
}

/// PR mirror of `issue_ingest_retries_tie_at_cursor_timestamp` — see that
/// test and `ingest_prs`'s sort-rationale comment for the tie hazard. Uses
/// the fail-once-embedder failure mechanism (see
/// `pr_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing`
/// for why the pre-#763 leaked-credential-in-title mechanism no longer
/// forces a create failure) since `pull_request` has no `stateReason` field.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn pr_ingest_retries_tie_at_cursor_timestamp() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    rt.register_embedder(FailOnceEmbedderProvider);

    let project_id = create(&registry, json!({"kind": "project", "name": "pr-tie-repo"})).await;

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

    const TIE_AT: &str = "2026-02-01T00:00:00Z";
    let pr_json = json!([
        {"number": 5, "title": "pr5-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": TIE_AT, "baseRefName": "main", "headRefName": "f5", "mergeCommit": null, "body": ""},
        {"number": 20, "title": "pr20-bad-tied", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": TIE_AT, "baseRefName": "main", "headRefName": "f20", "mergeCommit": null, "body": CURSOR_FAIL_SENTINEL}
    ])
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
    .expect("ingest ok (pass 1)");

    assert_eq!(
        report.prs_ingested, 1,
        "#5 lands, #20 (tied timestamp, injected embedder failure) warns-and-skips: {report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("pull_request #20"))
            .count(),
        1,
        "exactly one warning names the rejected record: {:?}",
        report.warnings
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, TIE_AT,
        "cursor freezes at #5's timestamp, which is the SAME as failed #20's — \
         the exact tie the exclusive `updated > cursor` check used to strand \
         #20 on: {cursor_after_pass1:?}"
    );

    // The embedder's one-shot fuse is already spent — pass 2 retries the tied
    // record with the SAME fixture and must land it without duplicating #5.
    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");

    assert_eq!(
        report2.prs_ingested, 1,
        "only #20 (embedder fuse now spent) is newly created on pass 2, proving the \
         tied timestamp did not strand it: {report2:?}"
    );
    assert_eq!(
        report2.prs_skipped_existing, 0,
        "#5 is acknowledged at the tied cursor; no duplicate or repeat lookup: {report2:?}"
    );
    assert!(
        report2
            .warnings
            .iter()
            .all(|w| !w.contains("pull_request #20")),
        "#20 must not warn once the embedder fuse is spent: {:?}",
        report2.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 20}))
        .await
        .expect("list prs ok");
    let numbers: Vec<u64> = list_items(&prs_list)
        .iter()
        .filter_map(|i| i["properties"]["number"].as_u64())
        .collect();
    assert_eq!(
        numbers.len(),
        2,
        "exactly #5, #20 — no duplicates: {numbers:?}"
    );
}
