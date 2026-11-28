/// The walked-then-failed invariant (the walked-then-failed finding), driven
/// through the one post-visit Err site the standing stub infra can reach:
/// the checkpoint write after the issue page. A pass that walks the
/// window to completion and THEN fails persisting the cursor must never
/// regress to `skipped` ("never walked") — and must not stay `completed`
/// either: the walk happened but the pass failed, so the state downgrades
/// to `stopped_early` with the failure in the reason, and neither `done`
/// nor `history_exhausted` may claim the source is finished.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_walked_then_cursor_write_fails_never_reports_skipped() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "walked-then-failed-repo"}),
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

    let issue_json = json!([issue_fixture(1, "Bug report", "2026-01-01T00:00:01Z")]).to_string();
    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    // Sabotage ONLY the cursor write: read_cursor must keep working (it is
    // the walker's first statement, before any page fetch), so the table
    // stays, with a trigger that aborts every INSERT. The walker then runs
    // page — records land and the walk-start state is recorded — and
    // fails saving the page checkpoint before completion is reported.
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "CREATE TRIGGER fail_cursor_write BEFORE INSERT ON git_mirror_cursor \
                  BEGIN SELECT RAISE(ABORT, 'sabotaged cursor write'); END"
                .into(),
            params: vec![],
            label: Some("test_fail_cursor_write".into()),
        })
        .await
        .expect("install sabotage trigger");

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.commits = false;
    opts.include.pull_requests = false;
    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("a post-walk cursor-write failure must stay in-band");

    assert_eq!(
        report.issues_ingested, 1,
        "the record landed before the cursor write failed: {report:?}"
    );
    match &report.sources.issues {
        Some(khive_pack_git::ingest::IngestSourceState::StoppedEarly(reason)) => {
            assert!(
                reason.contains("pass then failed after the walk")
                    && reason.contains("sabotaged cursor write"),
                "the walked page stays stopped-early with the cause: {reason:?}"
            );
        }
        other => panic!("a walked-then-failed source is never skipped or completed: {other:?}"),
    }
    assert!(
        report.warnings.iter().any(
            |w| w.contains("gh issue list failed after walking 1 record")
                && w.contains("stopped early, not skipped")
        ),
        "the warning names the walked-then-failed shape, not a skip: {:?}",
        report.warnings
    );
    assert!(
        report
            .warnings
            .iter()
            .all(|w| !w.contains("skipping issues")),
        "a walked-then-failed source must never be described as skipped: {:?}",
        report.warnings
    );
    assert!(
        !report.history_exhausted,
        "a failed pass must not claim exhaustion: {report:?}"
    );
    assert!(
        !report.done,
        "the resume loop must re-walk a source whose pass failed: {report:?}"
    );
}

/// The cursor-floor guard must hold while stalled: while a pass
/// is stalled on a refused record, an ALREADY-EXISTING record walked after
/// the stall point — with a strictly newer `updated_at` — must not advance
/// the persisted floor past the refused record. The natural-key lookup
/// proves only the existing record's own landing; advancing past it would
/// persist a cursor strictly newer than the refused record's timestamp, so
/// the next pass's inclusive `updated >= cursor` filter would never
/// re-fetch the refused record — skipping it forever instead of retrying
/// it. Without the guard, this fixture's persisted cursor would be #10's
/// 2026-01-03, past refused #20's 2026-01-01.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn pr_cursor_does_not_advance_past_refused_record_on_later_existing() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    rt.register_embedder(FailOnceEmbedderProvider);

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "stalled-floor-existing-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");

    // Pre-land PR #10 as if an earlier pass created it, with the NEWEST
    // timestamp of the three — it is the record walked after the stall
    // point whose timestamp the unguarded advance would persist.
    create(
        &registry,
        json!({
            "kind": "pull_request",
            "name": "#10 pre-existing PR",
            "content": "landed in an earlier pass",
            "properties": {
                "number": 10,
                "project_id": project_id.to_string()
            },
            "annotates": [project_id.to_string()]
        }),
    )
    .await;

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");

    // Ascending by updatedAt: #5 (good), #20 (refused once), #10 (existing,
    // newest). The stall fires at #20; the guard must keep #10's newer
    // timestamp out of the persisted floor.
    let pr_json = json!([
        {"number": 5, "title": "pr5-oldest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2025-12-01T00:00:00Z", "baseRefName": "main", "headRefName": "f5", "mergeCommit": null, "body": ""},
        {"number": 20, "title": "pr20-refused", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2026-01-01T00:00:00Z", "baseRefName": "main", "headRefName": "f20", "mergeCommit": null, "body": CURSOR_FAIL_SENTINEL},
        {"number": 10, "title": "pr10-existing-newest", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2026-01-03T00:00:00Z", "baseRefName": "main", "headRefName": "f10", "mergeCommit": null, "body": ""}
    ])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.commits = false;
    opts.include.issues = false;

    let report = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .expect("ingest ok (pass 1)");

    assert_eq!(
        report.prs_ingested, 1,
        "#5 lands, #20 is refused once, #10 is found by natural key: {report:?}"
    );
    assert_eq!(report.prs_skipped_existing, 1, "{report:?}");
    assert!(
        report.cursor_stalled,
        "the refused #20 freezes the PR cursor: {report:?}"
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, "2025-12-01T00:00:00Z",
        "the stalled floor must not advance past refused #20 (2026-01-01) on \
         the strength of later EXISTING #10's newer 2026-01-03 timestamp — \
         that would strand #20 behind the cursor forever: {cursor_after_pass1:?}"
    );

    // The embedder's one-shot fuse is spent: pass 2 re-walks from the frozen
    // floor, retries #20, lands it, and only then does the floor advance to
    // the newest walked timestamp.
    let report2 = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok (pass 2)");
    assert_eq!(
        report2.prs_ingested, 1,
        "the refused record is retried and lands once the upstream failure clears: {report2:?}"
    );
    assert_eq!(
        report2.prs_skipped_existing, 1,
        "#5 is acknowledged; #10 needs a lookup: {report2:?}"
    );
    assert!(!report2.cursor_stalled, "{report2:?}");

    let cursor_after_pass2 = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written");
    assert_eq!(
        cursor_after_pass2, "2026-01-03T00:00:00Z",
        "after recovery the floor advances to the newest walked record: {cursor_after_pass2:?}"
    );
}

/// The issue walker also keeps its cursor at a refused record when a
/// newer, already-landed issue follows it. Refusal uses the deterministic
/// ungoverned-`stateReason` rejection.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_cursor_does_not_advance_past_refused_record_on_later_existing() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "stalled-floor-existing-issue-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");

    create(
        &registry,
        json!({
            "kind": "issue",
            "name": "#10 pre-existing issue",
            "content": "landed in an earlier pass",
            "properties": {
                "number": 10,
                "project_id": project_id.to_string()
            },
            "annotates": [project_id.to_string()]
        }),
    )
    .await;

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");

    let issue_json = json!([
        {"number": 5, "title": "issue5-oldest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2025-12-01T00:00:00Z", "labels": [], "stateReason": "", "body": ""},
        {"number": 20, "title": "issue20-refused", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-01T00:00:00Z", "labels": [], "stateReason": "bogus_ungoverned_value", "body": ""},
        {"number": 10, "title": "issue10-existing-newest", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-03T00:00:00Z", "labels": [], "stateReason": "", "body": ""}
    ])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.commits = false;
    opts.include.pull_requests = false;

    let report = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .expect("ingest ok (pass 1)");

    assert_eq!(report.issues_ingested, 1, "{report:?}");
    assert_eq!(report.issues_skipped_existing, 1, "{report:?}");
    assert!(report.cursor_stalled, "{report:?}");

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, "2025-12-01T00:00:00Z",
        "the stalled floor must not advance past refused #20 on the strength \
         of later EXISTING #10's newer timestamp: {cursor_after_pass1:?}"
    );

    // Upstream correction: #20's stateReason becomes governed — pass 2
    // retries the refused record from the frozen floor and lands it.
    let corrected = json!([
        {"number": 5, "title": "issue5-oldest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2025-12-01T00:00:00Z", "labels": [], "stateReason": "", "body": ""},
        {"number": 20, "title": "issue20-refused", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-01T00:00:00Z", "labels": [], "stateReason": "completed", "body": ""},
        {"number": 10, "title": "issue10-existing-newest", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-03T00:00:00Z", "labels": [], "stateReason": "", "body": ""}
    ])
    .to_string();
    std::fs::write(log_dir.join("issue_response.json"), corrected)
        .expect("rewrite issue fixture with corrected stateReason");

    let report2 = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok (pass 2)");
    assert_eq!(report2.issues_ingested, 1, "{report2:?}");
    assert_eq!(
        report2.issues_skipped_existing, 1,
        "#5 is acknowledged; #10 needs a lookup: {report2:?}"
    );
    assert!(!report2.cursor_stalled, "{report2:?}");

    let cursor_after_pass2 = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written");
    assert_eq!(
        cursor_after_pass2, "2026-01-03T00:00:00Z",
        "{cursor_after_pass2:?}"
    );
}
