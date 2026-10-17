// ── issue #1617: PR/issue source states over a stubbed `gh` ─────

/// `gh {pr,issue} list --json` field sets, mirrored from `ingest.rs`'s
/// private constants so the fixtures below fail loudly (parse error) if the
/// production field set ever drifts.
fn pr_fixture(number: u64, title: &str, updated_at: &str) -> Value {
    json!({
        "number": number,
        "title": title,
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": updated_at,
        "baseRefName": "main",
        "headRefName": "feature/x",
        "mergeCommit": null,
        "body": format!("body of PR #{number}")
    })
}

fn issue_fixture(number: u64, title: &str, updated_at: &str) -> Value {
    json!({
        "number": number,
        "title": title,
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": updated_at,
        "labels": [],
        "stateReason": "",
        "body": format!("body of issue #{number}")
    })
}

/// A happy-path pass over a PATH-shadowing fake `gh`: the PR and issue
/// walkers report `completed`, records land, and `history_exhausted` is
/// true — the PR/issue half of the tri-state that previously only had
/// commit-side coverage (issue #1617).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_pr_issue_sources_completed_on_happy_path() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "tristate-happy-repo"}),
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

    let pr_json = json!([
        pr_fixture(1, "Add feature", "2026-01-01T00:00:01Z"),
        pr_fixture(2, "Fix bug", "2026-01-01T00:00:02Z"),
    ])
    .to_string();
    let issue_json = json!([
        issue_fixture(10, "Bug report", "2026-01-01T00:00:01Z"),
        issue_fixture(11, "Feature request", "2026-01-01T00:00:02Z"),
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
    .expect("ingest ok");

    assert_eq!(report.prs_ingested, 2, "{report:?}");
    assert_eq!(report.issues_ingested, 2, "{report:?}");
    assert_eq!(
        report.sources.pull_requests,
        Some(khive_pack_git::ingest::IngestSourceState::Completed),
        "a PR window walked to the end completes: {report:?}"
    );
    assert_eq!(
        report.sources.issues,
        Some(khive_pack_git::ingest::IngestSourceState::Completed),
        "an issue window walked to the end completes: {report:?}"
    );
    assert_eq!(
        report.sources.commits,
        Some(khive_pack_git::ingest::IngestSourceState::Completed),
        "{report:?}"
    );
    assert!(
        report.history_exhausted,
        "every included source completed: {report:?}"
    );
    assert!(report.done, "{report:?}");
}

/// With no source requested, exhaustion is vacuously true.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_history_exhausted_is_true_when_include_is_empty() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "empty-include-repo"}),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut opts = IngestOptions::unbounded(dir.path().to_path_buf(), project_id.to_string());
    opts.include.commits = false;
    opts.include.issues = false;
    opts.include.pull_requests = false;

    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("an empty include completes without walking a source");

    assert!(report.history_exhausted, "{report:?}");
    assert!(report.done, "{report:?}");
    assert!(report.sources.commits.is_none(), "{report:?}");
    assert!(report.sources.issues.is_none(), "{report:?}");
    assert!(report.sources.pull_requests.is_none(), "{report:?}");
}

/// With the budget spent mid-walk, a walker that breaks inside its record
/// loop reports `stopped_early` (visited, not exhausted) while sources
/// never even reached report `skipped` with the pre-reached budget reason
/// — stopped_early and skipped side by side in one pass (issue #1617).
/// Walk order is pull_requests → issues → commits, so `max_items: 1`
/// lands the first PR, stops the PR walk on its second record, and skips
/// issues and commits outright.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_pr_issue_sources_stopped_early_on_budget_stop() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "tristate-budget-repo"}),
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

    let pr_json = json!([
        pr_fixture(1, "Add feature", "2026-01-01T00:00:01Z"),
        pr_fixture(2, "Fix bug", "2026-01-01T00:00:02Z"),
    ])
    .to_string();
    let issue_json = json!([
        issue_fixture(10, "Bug report", "2026-01-01T00:00:01Z"),
        issue_fixture(11, "Feature request", "2026-01-01T00:00:02Z"),
    ])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, &pr_json, &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    // max_items = 1: the first PR lands, the PR walk breaks on its second
    // record, and issues/commits are never reached at all. (An in-walk
    // budget break for BOTH gh walkers in one pass is unreachable with
    // this fixture: the last budget unit is always spendable on a record
    // — `budget.exhausted()` gates the NEXT record, not the current one —
    // so the first walker to run consumes what the second would have
    // needed. The issue walker's own stopped-early machinery is covered
    // by `issue_full_page_never_leaks_raw_updated_at_into_paging_floor`
    // and the frozen-cursor coverage.)
    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.max_items = Some(1);
    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the first PR lands before the budget runs out: {report:?}"
    );
    assert_eq!(report.issues_ingested, 0, "{report:?}");
    assert_eq!(report.commits_ingested, 0, "{report:?}");
    match &report.sources.pull_requests {
        Some(khive_pack_git::ingest::IngestSourceState::StoppedEarly(reason)) => {
            assert!(
                reason.contains("budget exhausted"),
                "the reason names the budget as the cause: {reason:?}"
            );
        }
        other => panic!("a budget-stopped walk is stopped_early, got {other:?}"),
    }
    assert_eq!(
        report.sources.issues,
        Some(khive_pack_git::ingest::IngestSourceState::Skipped(
            "budget exhausted before issues were reached".to_string()
        )),
        "issues were never reached — a genuine skip: {report:?}"
    );
    assert_eq!(
        report.sources.commits,
        Some(khive_pack_git::ingest::IngestSourceState::Skipped(
            "budget exhausted before commits were reached".to_string()
        )),
        "commits were never reached — a genuine skip: {report:?}"
    );
    assert!(
        !report.history_exhausted,
        "budget-stopped sources are not exhaustion: {report:?}"
    );
    assert!(!report.done, "{report:?}");
}

/// The full-page paging stop: a `PAGE_LIMIT`-sized PR page forces the
/// walker to stop with the remote window unproven — `stopped_early`, not
/// `completed` (issue #1617; the issue walker's paging stop is
/// already covered by
/// `issue_full_page_never_leaks_raw_updated_at_into_paging_floor`).
///
/// Then the walked-then-failed arm (the walked-then-failed finding): the stub
/// is swapped to fail, and the second pass's FIRST fetch errors. Because
/// the walk-start marker is only written after the first page lands, a
/// fresh pass whose first fetch fails was genuinely never walked — so
/// `skipped` remains correct HERE, and the never-overwrite invariant is
/// instead pinned by the cursor-write-failure test below (the one
/// post-visit Err site reachable with the standing stub infra). A
/// `stopped_early`-after-full-window pass is also not completable with
/// this stub infra: the canned stub cannot honor the `updated:>=` search
/// floor, so a resumed pass re-fetches the identical full page and the
/// floor legitimately stalls (`StopFloorStalled`) — the `completed` state
/// is covered by the happy-path and budget-stop tests instead.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_pr_source_stopped_early_on_full_page_then_refetch_failure_stays_skipped() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "tristate-paging-repo"}),
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

    // Mirrors `ingest.rs`'s private `PAGE_LIMIT`.
    const PAGE_LIMIT: usize = 1000;
    let prs: Vec<Value> = (1..=PAGE_LIMIT as u64)
        .map(|i| pr_fixture(i, &format!("pr {i}"), "2026-01-01T00:16:00Z"))
        .collect();
    write_fake_gh(
        &repo,
        &bin_dir,
        &log_dir,
        &Value::Array(prs).to_string(),
        "[]",
    );
    let _path_guard = PathGuard::install(&bin_dir);

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.commits = false;
    opts.include.issues = false;
    let report = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .expect("ingest ok");

    assert_eq!(report.prs_ingested, PAGE_LIMIT as u64, "{report:?}");
    match &report.sources.pull_requests {
        Some(khive_pack_git::ingest::IngestSourceState::StoppedEarly(reason)) => {
            assert!(
                reason.contains("full page"),
                "the reason names the full-page floor stop: {reason:?}"
            );
        }
        other => panic!("a full page with an unproven window is stopped_early, got {other:?}"),
    }
    assert!(
        !report.done && !report.history_exhausted,
        "the resume loop must keep going past an unproven window: {report:?}"
    );

    // A fresh pass whose FIRST fetch fails was never walked: `skipped`
    // stays accurate (the marker distinguishes "failed before the walk"
    // from "failed after it").
    let script = r#"#!/bin/sh
case "$1" in
  repo)
    echo '{"nameWithOwner":"fixture/repository","url":"https://github.com/fixture/repository"}'
    ;;
  *)
    echo "fake gh: simulated listing outage" 1>&2
    exit 1
    ;;
esac
"#;
    std::fs::write(bin_dir.join("gh"), script).expect("swap stub to failing");

    let report2 = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("a gh listing failure must stay an in-band ingest result");
    match &report2.sources.pull_requests {
        Some(khive_pack_git::ingest::IngestSourceState::Skipped(reason)) => {
            assert!(
                reason.contains("gh pr list failed"),
                "a first-fetch failure reports skipped with the cause: {reason:?}"
            );
        }
        other => panic!("a never-walked source must report skipped, got {other:?}"),
    }
    assert!(
        report2
            .warnings
            .iter()
            .any(|w| w.contains("gh pr list failed")),
        "the failure also surfaces as a warning: {:?}",
        report2.warnings
    );
    assert!(
        report2.done,
        "a first-fetch listing failure leaves the budget-cursor signal unchanged: {report2:?}"
    );
}
