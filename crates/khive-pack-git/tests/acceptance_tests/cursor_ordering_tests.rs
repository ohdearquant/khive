// ── unsorted `gh` output must not desync the frozen-cursor retry guarantee ──

/// Reads the git-ingest cursor row directly, mirroring `ingest.rs`'s private
/// `read_cursor` — the acceptance tests otherwise only observe cursor state
/// indirectly through `IngestReport` deltas across passes, which is not
/// precise enough to assert the exact freeze point below.
async fn read_git_cursor(rt: &KhiveRuntime, project_id: Uuid, kind: &str) -> Option<String> {
    use khive_storage::types::{SqlStatement, SqlValue};
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("sql reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind=?2"
                .into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text(kind.to_string()),
            ],
            label: Some("test_read_git_cursor".into()),
        })
        .await
        .expect("query cursor row");
    row.and_then(|r| match r.get("cursor_value") {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    })
}

/// `gh issue list` / `gh pr list` make no ordering guarantee: the
/// frozen-cursor retry contract only holds if records are
/// walked in nondecreasing `updatedAt` order, so unsorted `gh` output can let
/// a later, newer record's timestamp overwrite the freeze point before an
/// earlier, older record fails — after which the older failure looks
/// already-covered by the cursor and is skipped forever instead of retried.
///
/// This fixture returns three issues out of raw order — `[#10 (newest,
/// good), #20 (older than #10, BAD: ungoverned stateReason), #5 (oldest,
/// good)]` — so an unsorted walk would create #10 before #20 fails, wrongly
/// freezing the cursor at #10's timestamp (newer than #20's). Sorting
/// ascending first (the fix) walks `#5, #20, #10`, so the freeze lands at
/// #5's timestamp instead — at/below #20's — and pass 2 (after #20's
/// `stateReason` is corrected upstream) retries and lands it without
/// duplicating #5 or #10.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-order-repo"}),
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

    let bad_issue_json = |state_reason: &str| {
        json!([
            {"number": 10, "title": "i10-newest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-03T00:00:00Z", "labels": [], "stateReason": "completed", "body": ""},
            {"number": 20, "title": "i20-older-bad", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-01T00:00:00Z", "labels": [], "stateReason": state_reason, "body": ""},
            {"number": 5, "title": "i5-oldest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2025-12-01T00:00:00Z", "labels": [], "stateReason": "", "body": ""}
        ])
        .to_string()
    };

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &bad_issue_json("WONTFIX"));
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
        report.issues_ingested, 2,
        "#5 and #10 both land, #20 warns-and-skips: {report:?}"
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
        cursor_after_pass1, "2025-12-01T00:00:00Z",
        "cursor must freeze at #5's timestamp (at/below failed #20's \
         2026-01-01T00:00:00Z), not advance to #10's later 2026-01-03T00:00:00Z \
         just because #10 appeared earlier in gh's raw (unsorted) output: {cursor_after_pass1:?}"
    );

    // Upstream correction: #20's stateReason is fixed to a governed value —
    // pass 2 must retry and land it without duplicating #5 or #10.
    std::fs::write(
        log_dir.join("issue_response.json"),
        bad_issue_json("completed"),
    )
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
        "only #20 (now corrected) is newly created on pass 2: {report2:?}"
    );
    assert_eq!(
        report2.issues_skipped_existing, 1,
        "#5 replays its checkpoint; #10 is found by natural key; neither duplicates: {report2:?}"
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
        3,
        "exactly #5, #10, #20 — no duplicates: {numbers:?}"
    );

    let cursor_after_pass2 = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written");
    assert_eq!(cursor_after_pass2, "2026-01-03T00:00:00Z");
}

/// PR mirror of `issue_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing`.
/// `pull_request` has no `stateReason` field in `gh pr list --json` (verified
/// against the live `gh` CLI's field list — only `issue`s carry one), so this
/// fixture forces the per-record failure a different, equally real way. Prior
/// to #763 this used a leaked-credential-shaped PR title, but #763 now masks
/// credential-shaped title/body in place instead of dropping the record, so
/// that mechanism would land #20 instead of failing it. Per #763's approved
/// design (`architect-2/approved_design.md` §3, "Required tests"), the
/// replacement is a deterministic, secret-detection-independent failure: a
/// test-only embedder ([`FailOnceEmbeddingService`]) that fails exactly once
/// for a PR body containing [`CURSOR_FAIL_SENTINEL`].
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn pr_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    rt.register_embedder(FailOnceEmbedderProvider);

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-order-repo"}),
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

    let bad_pr_json = json!([
        {"number": 10, "title": "pr10-newest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2026-01-03T00:00:00Z", "baseRefName": "main", "headRefName": "f10", "mergeCommit": null, "body": ""},
        {"number": 20, "title": "pr20-older-bad", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2026-01-01T00:00:00Z", "baseRefName": "main", "headRefName": "f20", "mergeCommit": null, "body": CURSOR_FAIL_SENTINEL},
        {"number": 5, "title": "pr5-oldest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2025-12-01T00:00:00Z", "baseRefName": "main", "headRefName": "f5", "mergeCommit": null, "body": ""}
    ])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, &bad_pr_json, "[]");
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
        report.prs_ingested, 2,
        "#5 and #10 both land, #20 (injected embedder failure) warns-and-skips: {report:?}"
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
    assert!(
        report.cursor_stalled,
        "a frozen PR cursor must surface as cursor_stalled (issue #1645): {report:?}"
    );
    assert!(
        !report.done,
        "a stalled cursor means unretried records exist past the floor, so the \
         pass must not report done (issue #1645): {report:?}"
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, "2025-12-01T00:00:00Z",
        "cursor must freeze at #5's timestamp (at/below failed #20's \
         2026-01-01T00:00:00Z), not advance to #10's later 2026-01-03T00:00:00Z \
         just because #10 appeared earlier in gh's raw (unsorted) output: {cursor_after_pass1:?}"
    );

    // The embedder's one-shot fuse is already spent — pass 2 retries #20 with
    // the SAME fixture (no upstream correction needed) and must land it
    // without duplicating #5 or #10.
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
        "only #20 (embedder fuse now spent) is newly created on pass 2: {report2:?}"
    );
    assert_eq!(
        report2.prs_skipped_existing, 1,
        "#5 replays its checkpoint; #10 is found by natural key; neither duplicates: {report2:?}"
    );
    assert!(
        report2
            .warnings
            .iter()
            .all(|w| !w.contains("pull_request #20")),
        "#20 must not warn once the embedder fuse is spent: {:?}",
        report2.warnings
    );
    assert!(
        !report2.cursor_stalled,
        "a clean recovery pass must not report a stall (issue #1645): {report2:?}"
    );
    assert!(
        report2.done,
        "with the stall recovered and the window walked, the pass is complete \
         (issue #1645): {report2:?}"
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
        3,
        "exactly #5, #10, #20 — no duplicates: {numbers:?}"
    );

    let cursor_after_pass2 = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written");
    assert_eq!(cursor_after_pass2, "2026-01-03T00:00:00Z");
}

// ── an empty walk is only a completion when the cursor is an ancestor ───────

/// A `{cursor}..HEAD` walk that yields nothing is a genuine completion only
/// when the cursor is an ancestor of the walked HEAD. If the walked source's
/// history LAGS the history that advanced the cursor (measured in production:
/// a scratch clone whose checked-out HEAD trailed the cursor by weeks walked
/// nothing and reported zero commits, `done: true` — issue #1644), the empty
/// range means "this source cannot see the cursor's history", not "nothing is
/// new". The fix surfaces a warning and refuses the completion claim.
///
/// Fixture: pass 1 ingests commits A and B, planting the cursor at B. HEAD is
/// then reset back to A — B still exists in the object store (so the walk's
/// `B..HEAD` range resolves and is empty) but is no longer an ancestor of
/// HEAD, exactly the lagging-source shape.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn empty_walk_with_non_ancestor_cursor_refuses_completion() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "non-ancestor-cursor-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "a.md", "a\n");
    commit(&repo, &["a.md"], "commit A");
    let sha_a = head_sha(&repo);
    write(&repo, "b.md", "b\n");
    commit(&repo, &["b.md"], "commit B");
    let sha_b = head_sha(&repo);

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");
    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");
    assert_eq!(report.commits_ingested, 2, "{report:?}");
    assert!(report.done, "clean full walk completes: {report:?}");
    let cursor = read_git_cursor(&rt, project_id, "commits")
        .await
        .expect("cursor written");
    assert_eq!(cursor, sha_b, "cursor advances to HEAD (B)");

    // The walked source now lags the cursor: HEAD back at A, cursor still B.
    git(&repo, &["reset", "--hard", "-q", &sha_a]);

    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");
    assert_eq!(
        report2.commits_ingested, 0,
        "nothing to walk in the empty range: {report2:?}"
    );
    assert!(
        matches!(
            report2.sources.commits.as_ref(),
            Some(khive_pack_git::ingest::IngestSourceState::StoppedEarly(_))
        ),
        "a diverged empty range records a stopped-early commit source: {report2:?}"
    );
    assert!(
        !report2.done,
        "an empty walk from a non-ancestor cursor is not a completion \
         (issue #1644): {report2:?}"
    );
    assert_eq!(
        report2
            .warnings
            .iter()
            .filter(|w| w.contains("not an ancestor"))
            .count(),
        1,
        "exactly one warning names the lagging source: {:?}",
        report2.warnings
    );
}
