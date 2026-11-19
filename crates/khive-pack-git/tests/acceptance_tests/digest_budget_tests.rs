/// A per-record secret-gate refusal remains non-fatal so the rest of the
/// digest can land, but the public result must make the partial outcome
/// mechanically detectable and identify the refused write without echoing
/// the rejected content.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_counts_and_describes_partial_secret_gate_refusals() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "refusal-accounting-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");

    const CREDENTIAL: &str = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let pr_json = json!([
        {
            "number": 1,
            "title": "clean pull request",
            "author": {"login": "contributor"},
            "createdAt": "2026-01-01T00:00:00Z",
            "mergedAt": null,
            "closedAt": null,
            "updatedAt": "2026-01-01T00:00:00Z",
            "baseRefName": "main",
            "headRefName": "feature/clean",
            "mergeCommit": null,
            "body": "ordinary content"
        },
        {
            "number": 2,
            "title": "refused pull request",
            "author": {"login": "contributor"},
            "createdAt": CREDENTIAL,
            "mergedAt": null,
            "closedAt": null,
            "updatedAt": "2026-01-02T00:00:00Z",
            "baseRefName": "main",
            "headRefName": "feature/refused",
            "mergeCommit": null,
            "body": "ordinary content"
        }
    ])
    .to_string();
    write_fake_gh(&repo, &bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let result = registry
        .dispatch(
            "git.digest",
            json!({
                "source": repo.to_str().expect("utf-8 repo path"),
                "project": project_id.to_string(),
                "max_items": 10,
                "include": ["pull_requests"]
            }),
        )
        .await
        .expect("a partial refusal remains an in-band digest result");

    assert_eq!(
        result["prs_ingested"], 2,
        "the clean write and successful redacted replacement must land: {result}"
    );
    assert_eq!(
        result["writes_refused"], 1,
        "callers must be able to assert zero refused writes: {result}"
    );
    let refusals = result["write_refusals"]
        .as_array()
        .expect("write_refusals array");
    assert_eq!(refusals.len(), 1, "one detail per counted refusal");
    assert_eq!(refusals[0]["verb"], "create");
    assert_eq!(refusals[0]["record_kind"], "pull_request");
    assert_eq!(refusals[0]["record_key"], "#2");
    assert_eq!(refusals[0]["redacted"], true);
    assert!(
        refusals[0]["detector"]
            .as_str()
            .is_some_and(|d| !d.is_empty()),
        "the detector must be named: {refusals:?}"
    );
    assert!(
        refusals[0]["masked"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "the masked diagnostic must be present: {refusals:?}"
    );
    assert!(
        !result.to_string().contains(CREDENTIAL),
        "the rejected content must never be copied into the result: {result}"
    );

    let stored = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list pull requests");
    let items = list_items(&stored);
    let mut numbers: Vec<u64> = items
        .iter()
        .filter_map(|item| item["properties"]["number"].as_u64())
        .collect();
    numbers.sort_unstable();
    assert_eq!(
        numbers,
        vec![1, 2],
        "the clean record and redacted replacement must both persist"
    );
    let replacement_id = items
        .iter()
        .find(|item| item["properties"]["number"] == json!(2))
        .expect("the actual listed replacement for #2")["id"]
        .as_str()
        .expect("the listed replacement has a stored note id");
    let replacement = registry
        .dispatch("get", json!({"id": replacement_id}))
        .await
        .expect("read the actual stored replacement by its returned id");
    assert_eq!(replacement["properties"]["number"], json!(2));
    assert_eq!(
        replacement["content"],
        "GitHub content was redacted after a secret-gate refusal."
    );
    assert_eq!(
        replacement["properties"]["title"],
        "GitHub content redacted"
    );
    assert_eq!(replacement["properties"]["created_at"], json!(null));
    assert_eq!(replacement["properties"]["author"], json!(null));
    assert_eq!(replacement["properties"]["base_ref"], json!(null));
    assert_eq!(replacement["properties"]["head_ref"], json!(null));
    assert_eq!(
        replacement["properties"]["project_id"],
        json!(project_id.to_string())
    );
    assert!(
        !replacement.to_string().contains(CREDENTIAL),
        "the raw fixture credential must never persist in the replacement: {replacement}"
    );
}

/// `max_items` bounds work per call and the response is cursor-resumable:
/// looping `git.digest` calls until `done` eventually ingests every commit
/// with no duplicates.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_max_items_is_bounded_and_resumable() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    for i in 0..3 {
        write(repo, "f.txt", &format!("v{i}\n"));
        commit(repo, &["f.txt"], &format!("commit {i}"));
    }

    let mut total_ingested = 0u64;
    let mut project_id: Option<String> = None;
    let mut calls = 0;
    loop {
        calls += 1;
        assert!(calls <= 10, "must converge well within 10 calls");
        let mut args =
            json!({"source": repo.to_str().unwrap(), "max_items": 1, "include": ["commits"]});
        if let Some(p) = &project_id {
            args["project"] = json!(p);
        }
        let resp = registry
            .dispatch("git.digest", args)
            .await
            .expect("digest ok");
        project_id = Some(resp["project_id"].as_str().unwrap().to_string());
        total_ingested += resp["commits_ingested"].as_u64().unwrap();
        if resp["done"].as_bool().unwrap() {
            break;
        }
    }
    assert_eq!(total_ingested, 3, "all three commits eventually ingest");

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    assert_eq!(
        list_items(&list).len(),
        3,
        "no duplicates across the bounded/resumed calls"
    );
}

// ── issue #1617: per-source tri-state + history-exhaustion reporting ───────

/// A clean full pass marks every included source `completed` and reports
/// `history_exhausted: true` — "nothing walked past this point" is now
/// distinguishable from silence (issue #1617).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_sources_completed_and_history_exhausted_on_full_walk() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    for i in 0..2 {
        write(repo, "f.txt", &format!("v{i}\n"));
        commit(repo, &["f.txt"], &format!("commit {i}"));
    }

    // No fake `gh` on PATH: issues/PRs take the gh_cli_absent skip arm.
    let resp = registry
        .dispatch(
            "git.digest",
            json!({
                "source": repo.to_str().unwrap(),
                "include": ["commits", "issues", "pull_requests"]
            }),
        )
        .await
        .expect("digest ok");

    assert_eq!(resp["commits_ingested"].as_u64().unwrap(), 2);
    assert_eq!(
        resp["sources"]["commits"],
        json!({"state": "completed"}),
        "a walked-to-the-tip commit source completes: {resp:?}"
    );
    assert_eq!(
        resp["sources"]["issues"]["state"], "skipped",
        "gh absent must surface as a structured skip, not prose: {resp:?}"
    );
    assert_eq!(resp["sources"]["pull_requests"]["state"], "skipped");
    assert!(
        !resp["history_exhausted"].as_bool().unwrap(),
        "skipped sources mean the walk did not cover everything requested: {resp:?}"
    );

    // A commits-only pass walks every requested source to the end.
    let commits_only = registry
        .dispatch(
            "git.digest",
            json!({
                "source": repo.to_str().unwrap(),
                "include": ["commits"]
            }),
        )
        .await
        .expect("digest ok");
    assert_eq!(
        commits_only["sources"]["commits"],
        json!({"state": "completed"}),
        "an idempotent empty-range walk is still a completion: {commits_only:?}"
    );
    assert!(
        commits_only["history_exhausted"].as_bool().unwrap(),
        "every requested source completed: {commits_only:?}"
    );
    assert!(
        commits_only["done"].as_bool().unwrap(),
        "done keeps its budget-cursor meaning alongside: {commits_only:?}"
    );
}

/// Budget exhaustion marks the source `stopped_early` and
/// `history_exhausted: false`, while `done: false` keeps its existing
/// resume-loop meaning (issue #1617).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_sources_stopped_early_on_budget_exhaustion() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    for i in 0..3 {
        write(repo, "f.txt", &format!("v{i}\n"));
        commit(repo, &["f.txt"], &format!("commit {i}"));
    }

    let resp = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 1, "include": ["commits"]}),
        )
        .await
        .expect("digest ok");

    assert_eq!(resp["commits_ingested"].as_u64().unwrap(), 1);
    assert_eq!(resp["max_items_requested"], 1);
    assert_eq!(resp["max_items_effective"], 1);
    assert!(
        resp["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|warning| !warning
                .as_str()
                .is_some_and(|text| text.contains("max_items"))),
        "an in-range budget must not produce a clamp warning: {resp:?}"
    );
    assert!(
        !resp["done"].as_bool().unwrap(),
        "budget exhausted with commits unwalked: {resp:?}"
    );
    assert_eq!(
        resp["sources"]["commits"]["state"], "stopped_early",
        "budget exhaustion is a stop-early, not a completion: {resp:?}"
    );
    assert!(
        resp["sources"]["commits"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("budget")),
        "the reason names the budget as the cause: {resp:?}"
    );
    assert!(
        !resp["history_exhausted"].as_bool().unwrap(),
        "a budget-stopped walk is not exhaustion: {resp:?}"
    );
}

/// A per-record write refusal on one commit skips that record and the walk
/// continues: later commits in the same pass still land. The source is
/// `stopped_early` (cursor frozen for retry, issue #1645 semantics) and the
/// refusal is mechanically attributable to the commits source (issue #1617).
/// The deterministic failure uses the design-mandated fail-once embedder
/// (see `pr_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing`
/// for why a leaked-credential fixture no longer forces a create failure).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_sources_gate_refusal_skips_record_and_walk_continues() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, _token, registry) = fixture().await;
    rt.register_embedder(FailOnceEmbedderProvider);

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    // Oldest first: clean, refused (sentinel body), clean.
    write(repo, "f.txt", "v0\n");
    commit(repo, &["f.txt"], "commit 0");
    let sha0 = head_sha(repo);
    write(repo, "f.txt", "v1\n");
    commit(
        repo,
        &["f.txt"],
        &format!("commit 1\n\n{CURSOR_FAIL_SENTINEL}"),
    );
    write(repo, "f.txt", "v2\n");
    commit(repo, &["f.txt"], "commit 2");
    let sha2 = head_sha(repo);

    let resp = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "include": ["commits"]}),
        )
        .await
        .expect("a per-record refusal must stay an in-band digest result");
    let project_id = Uuid::parse_str(
        resp["project_id"]
            .as_str()
            .expect("the digest response carries its project id"),
    )
    .expect("project id is a UUID");

    assert_eq!(
        resp["commits_ingested"].as_u64().unwrap(),
        2,
        "the refusal never terminates the walk — the later commit lands: {resp:?}"
    );
    assert_eq!(
        resp["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .filter(|w| w.as_str().is_some_and(|w| w.contains("create commit")))
            .count(),
        1,
        "exactly one warning records the refused commit write: {resp:?}"
    );
    assert_eq!(
        resp["sources"]["commits"]["state"], "stopped_early",
        "a frozen cursor is a stop-early, distinct from budget and skip: {resp:?}"
    );
    assert!(
        resp["sources"]["commits"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("cursor_stalled")),
        "the reason names the frozen cursor as the cause: {resp:?}"
    );
    assert!(
        !resp["history_exhausted"].as_bool().unwrap(),
        "an unretried refused record means the history is not exhausted: {resp:?}"
    );
    assert!(
        !resp["done"].as_bool().unwrap(),
        "cursor_stalled still forces done:false beside the new fields: {resp:?}"
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "commits")
        .await
        .expect("the successful prefix persists a cursor");
    assert_eq!(
        cursor_after_pass1, sha0,
        "the cursor freezes before the refused commit and does not skip it: {resp:?}"
    );

    let resp2 = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "include": ["commits"]}),
        )
        .await
        .expect("the next pass retries the frozen commit");
    assert_eq!(
        resp2["commits_ingested"].as_u64().unwrap(),
        1,
        "the refused commit is retried and lands on the next pass: {resp2:?}"
    );
    assert!(!resp2["cursor_stalled"].as_bool().unwrap(), "{resp2:?}");
    assert_eq!(
        read_git_cursor(&rt, project_id, "commits").await,
        Some(sha2),
        "the cursor advances through the recovered commit after retry: {resp2:?}"
    );
}
