//! Bounded visits and durable continuation through the real ingest boundary.
use super::*;
use khive_pack_git::ingest::IngestSourceState;

const TIE: &str = "2026-01-01T00:00:01Z";

fn remote_fixture(
    prs: &[Value],
    issues: &[Value],
) -> (tempfile::TempDir, PathBuf, PathBuf, PathGuard) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let bin = dir.path().join("bin");
    let logs = dir.path().join("logs");
    for path in [&repo, &bin, &logs] {
        std::fs::create_dir(path).unwrap();
    }
    init_repo(&repo);
    write_fake_gh(
        &repo,
        &bin,
        &logs,
        &json!(prs).to_string(),
        &json!(issues).to_string(),
    );
    let guard = PathGuard::install(&bin);
    (dir, repo, logs, guard)
}

fn options(repo: &Path, project: Uuid, prs: bool, max: u64) -> IngestOptions {
    IngestOptions {
        repo: repo.into(),
        expected_github_repo: Some("fixture/repository".into()),
        project: project.to_string(),
        max_items: Some(max),
        include: IngestInclude {
            commits: false,
            issues: !prs,
            pull_requests: prs,
        },
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_one_item_handles_undated_ties_and_unseen_lower_numbers() {
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (rt, token, registry) = fixture().await;
        let project = create(&registry, json!({"kind":"project","name":"bounded ties"})).await;
        let make = if prs { pr_fixture } else { issue_fixture };
        let mut undated = make(10, "undated", TIE);
        undated["updatedAt"] = Value::Null;
        let mut rows = vec![make(30, "third", TIE), make(20, "second", TIE), undated];
        let (pr_rows, issue_rows) = if prs {
            (rows.clone(), vec![])
        } else {
            (vec![], rows.clone())
        };
        let (_dir, repo, logs, _path) = remote_fixture(&pr_rows, &issue_rows);
        let opts = options(&repo, project, prs, 1);
        for pass in 0..3 {
            let report = run_ingest(&rt, &token, &registry, opts.clone())
                .await
                .unwrap();
            assert_eq!(
                report.prs_ingested + report.issues_ingested,
                1,
                "pass={pass}: {report:?}"
            );
            assert_eq!(
                report.prs_skipped_existing + report.issues_skipped_existing,
                0,
                "acknowledged boundaries do not perform a lookup: {report:?}"
            );
            assert!(
                !report.done,
                "an exact-budget pass conservatively requests one more call: {report:?}"
            );
        }
        // Exact membership matters: a newly visible, smaller number at the same
        // timestamp must not be discarded by a numeric high-water predicate.
        rows.push(make(5, "late boundary arrival", TIE));
        let response = if prs {
            "pr_response.json"
        } else {
            "issue_response.json"
        };
        std::fs::write(logs.join(response), json!(rows).to_string()).unwrap();
        let report = run_ingest(&rt, &token, &registry, opts.clone())
            .await
            .unwrap();
        assert_eq!(
            report.prs_ingested + report.issues_ingested,
            1,
            "{report:?}"
        );
        assert!(!report.done, "exact-budget pass: {report:?}");
        let final_report = run_ingest(&rt, &token, &registry, opts).await.unwrap();
        assert_eq!(
            final_report.prs_ingested
                + final_report.issues_ingested
                + final_report.prs_skipped_existing
                + final_report.issues_skipped_existing,
            0,
            "{final_report:?}"
        );
        assert!(
            final_report.done,
            "a free boundary replay proves completion: {final_report:?}"
        );
        let kind = if prs { "pull_request" } else { "issue" };
        let listed = registry
            .dispatch("list", json!({"kind":kind,"limit":20}))
            .await
            .unwrap();
        assert_eq!(list_items(&listed).len(), 4);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_legacy_timestamp_charges_existing_before_new_records() {
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (rt, token, registry) = fixture().await;
        let project = create(&registry, json!({"kind":"project","name":"legacy cursor"})).await;
        let kind = if prs { "pull_request" } else { "issue" };
        let cursor_kind = if prs { "prs" } else { "issues" };
        create(&registry, json!({"kind":kind,"name":"existing","content":"existing",
            "properties":{"number":10,"project_id":project.to_string()},"annotates":[project.to_string()]})).await;
        rt.sql().writer().await.unwrap().execute(SqlStatement {
            sql: "INSERT INTO git_mirror_cursor(project_id,kind,cursor_value,updated_at) VALUES(?1,?2,?3,0)".into(),
            params: vec![SqlValue::Text(project.to_string()),SqlValue::Text(cursor_kind.into()),SqlValue::Text(TIE.into())],label:None,
        }).await.unwrap();
        let make = if prs { pr_fixture } else { issue_fixture };
        let rows = vec![make(10, "existing", TIE), make(20, "new", TIE)];
        let (pr_rows, issue_rows) = if prs { (rows, vec![]) } else { (vec![], rows) };
        let (_dir, repo, _logs, _path) = remote_fixture(&pr_rows, &issue_rows);
        let opts = options(&repo, project, prs, 1);
        let first = run_ingest(&rt, &token, &registry, opts.clone())
            .await
            .unwrap();
        assert_eq!(
            first.prs_skipped_existing + first.issues_skipped_existing,
            1,
            "{first:?}"
        );
        assert_eq!(first.prs_ingested + first.issues_ingested, 0, "{first:?}");
        assert!(!first.done, "{first:?}");
        let second = run_ingest(&rt, &token, &registry, opts).await.unwrap();
        assert_eq!(
            second.prs_ingested + second.issues_ingested,
            1,
            "{second:?}"
        );
        assert_eq!(
            second.prs_skipped_existing + second.issues_skipped_existing,
            0,
            "{second:?}"
        );
        assert!(!second.done, "exact-budget pass: {second:?}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_rejected_tie_is_charged_and_retried() {
    let _lock = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project = create(&registry, json!({"kind":"project","name":"retry boundary"})).await;
    let mut rejected = issue_fixture(20, "rejected", TIE);
    rejected["stateReason"] = json!("unknown");
    let rows = vec![
        issue_fixture(10, "good", TIE),
        rejected,
        issue_fixture(30, "later", TIE),
    ];
    let (_dir, repo, logs, _path) = remote_fixture(&[], &rows);
    let opts = options(&repo, project, false, 1);
    let first = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .unwrap();
    assert_eq!(first.issues_ingested, 1, "{first:?}");
    for _ in 0..2 {
        let failed = run_ingest(&rt, &token, &registry, opts.clone())
            .await
            .unwrap();
        assert_eq!(
            failed.issues_ingested, 0,
            "failure consumes the one visit before #30: {failed:?}"
        );
        assert_eq!(failed.issues_skipped_existing, 0, "{failed:?}");
        assert!(failed.cursor_stalled && !failed.done, "{failed:?}");
        assert!(
            failed.warnings.iter().any(|w| w.contains("issue #20")),
            "{failed:?}"
        );
        assert_eq!(
            read_git_cursor(&rt, project, "issues").await.as_deref(),
            Some(TIE)
        );
    }
    let corrected = vec![
        issue_fixture(10, "good", TIE),
        issue_fixture(20, "corrected", TIE),
        issue_fixture(30, "later", TIE),
    ];
    std::fs::write(
        logs.join("issue_response.json"),
        json!(corrected).to_string(),
    )
    .unwrap();
    let retry = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .unwrap();
    assert_eq!(retry.issues_ingested, 1, "{retry:?}");
    assert!(!retry.done && !retry.cursor_stalled, "{retry:?}");
    let last = run_ingest(&rt, &token, &registry, opts).await.unwrap();
    assert_eq!(last.issues_ingested, 1, "{last:?}");
    assert!(!last.done, "exact-budget pass: {last:?}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_completed_pr_yields_to_issue_and_commit_with_merge_annotation() {
    let _lock = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind":"project","name":"shared visit budget"}),
    )
    .await;
    let (_dir, repo, logs, _path) = remote_fixture(&[], &[issue_fixture(2, "issue", TIE)]);
    write(&repo, "README.md", "test\n");
    commit(
        &repo,
        &["README.md"],
        "ordinary merge without a PR-number suffix",
    );
    let mut pr = pr_fixture(1, "merged PR", TIE);
    pr["mergeCommit"] = json!({"oid":head_sha(&repo)});
    std::fs::write(logs.join("pr_response.json"), json!([pr]).to_string()).unwrap();
    let mut opts = IngestOptions::unbounded(repo, project.to_string());
    opts.max_items = Some(1);
    let first = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .unwrap();
    assert_eq!(
        (
            first.prs_ingested,
            first.issues_ingested,
            first.commits_ingested
        ),
        (1, 0, 0),
        "{first:?}"
    );
    let second = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .unwrap();
    assert_eq!(
        (
            second.prs_ingested,
            second.issues_ingested,
            second.commits_ingested
        ),
        (0, 1, 0),
        "{second:?}"
    );
    let third = run_ingest(&rt, &token, &registry, opts).await.unwrap();
    assert_eq!(
        (
            third.prs_ingested,
            third.issues_ingested,
            third.commits_ingested
        ),
        (0, 0, 1),
        "{third:?}"
    );
    assert!(!third.done, "exact-budget pass: {third:?}");
    let prs = registry
        .dispatch("list", json!({"kind":"pull_request","limit":10}))
        .await
        .unwrap();
    let pr_id = Uuid::parse_str(list_items(&prs)[0]["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        incoming_annotating_ids(&registry, pr_id).await.len(),
        1,
        "replayed PR must populate the merge-SHA map without a fresh lookup"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_first_page_survives_second_fetch_failure() {
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (rt, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project","name":"page durability"}),
        )
        .await;
        let kind = if prs { "pull_request" } else { "issue" };
        let cursor_kind = if prs { "prs" } else { "issues" };
        let make = if prs { pr_fixture } else { issue_fixture };
        let rows: Vec<_> = (1..=1000).map(|n| make(n, "existing", TIE)).collect();
        {
            let writer = rt.backend().pool().try_writer().unwrap();
            writer.conn().execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1000) INSERT INTO notes(id,namespace,kind,name,content,properties,created_at,updated_at) SELECT printf('10000000-0000-4000-8000-%012x',i),'local',?1,'existing','existing',json_object('number',i,'project_id',?2),i,i FROM n", (kind,project.to_string())).unwrap();
        }
        let (pr_rows, issue_rows) = if prs { (rows, vec![]) } else { (vec![], rows) };
        let (dir, repo, logs, _path) = remote_fixture(&pr_rows, &issue_rows);
        let script_path = dir.path().join("bin/gh");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let outage_log = repo.join("second-fetch-outage.log");
        // First fetch has no floor. The next fetch fails only after that full
        // page has been walked, so an end-of-pass-only checkpoint loses it.
        // gh_json intentionally omits stderr on nonzero exits. Record the
        // injected failure in this fixture's cwd, independently of the report.
        let sabotage = r#"case "$*" in
  *'updated:>='*)
    printf '%s\n' "$*" >> second-fetch-outage.log
    echo 'second-page outage' >&2
    exit 1
    ;;
esac
"#;
        std::fs::write(
            &script_path,
            script.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{sabotage}"), 1),
        )
        .unwrap();
        assert!(!outage_log.exists());
        let opts = options(&repo, project, prs, 1001);
        let first = run_ingest(&rt, &token, &registry, opts).await.unwrap();
        let outage = std::fs::read_to_string(&outage_log)
            .expect("the second fetch must reach the injected nonzero exit");
        let operation = if prs { "pr" } else { "issue" };
        assert_eq!(outage.lines().count(), 1, "{outage}");
        assert!(
            outage.starts_with(&format!("{operation} list ")),
            "{outage}"
        );
        assert!(
            outage.contains(&format!("--search sort:updated-asc updated:>={TIE}")),
            "{outage}"
        );
        assert_eq!(
            first.prs_skipped_existing + first.issues_skipped_existing,
            1000,
            "{first:?}"
        );
        assert!(!first.done, "{first:?}");
        let first_source = if prs {
            &first.sources.pull_requests
        } else {
            &first.sources.issues
        };
        assert!(
            matches!(first_source,Some(IngestSourceState::StoppedEarly(reason))
            if reason.contains("pass then failed after the walk")
                && reason.contains(&format!("gh {operation} list failed"))),
            "the second fetch must actually fail during this pass: {first:?}"
        );
        assert_eq!(
            read_git_cursor(&rt, project, cursor_kind).await.as_deref(),
            Some(TIE)
        );
        std::fs::write(&script_path, script).unwrap();
        let resumed = run_ingest(&rt, &token, &registry, options(&repo, project, prs, 1))
            .await
            .unwrap();
        assert_eq!(
            resumed.prs_skipped_existing + resumed.issues_skipped_existing,
            0,
            "{resumed:?}"
        );
        assert!(
            !resumed.done,
            "a full 1000-record timestamp tie cannot prove exhaustion: {resumed:?}"
        );
        assert!(matches!(
            if prs {
                resumed.sources.pull_requests
            } else {
                resumed.sources.issues
            },
            Some(IngestSourceState::StoppedEarly(_))
        ));
        assert!(std::fs::read_to_string(logs.join("args.log"))
            .unwrap()
            .contains(&format!("updated:>={TIE}")));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_commit_prefix_survives_later_cursor_write_failure() {
    let _lock = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind":"project","name":"commit checkpoint"}),
    )
    .await;
    let (_dir, repo, _logs, _path) = remote_fixture(&[], &[]);
    write(&repo, "README.md", "one\n");
    commit(&repo, &["README.md"], "first");
    let first_sha = head_sha(&repo);
    write(&repo, "README.md", "two\n");
    commit(&repo, &["README.md"], "second");
    let second_sha = head_sha(&repo);
    rt.sql().writer().await.unwrap().execute(SqlStatement {
        sql:format!("CREATE TRIGGER fail_second_cursor BEFORE INSERT ON git_mirror_cursor WHEN NEW.kind='commits' AND NEW.cursor_value='{second_sha}' BEGIN SELECT RAISE(ABORT,'second cursor failure'); END"),params:vec![],label:None,
    }).await.unwrap();
    let mut opts = options(&repo, project, false, 2);
    opts.include = IngestInclude {
        commits: true,
        issues: false,
        pull_requests: false,
    };
    let failed = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .unwrap();
    assert_eq!(failed.commits_ingested, 2, "{failed:?}");
    assert!(!failed.done, "{failed:?}");
    assert_eq!(
        read_git_cursor(&rt, project, "commits").await.as_deref(),
        Some(first_sha.as_str())
    );
    let progress: Value = serde_json::from_str(
        &read_git_cursor(&rt, project, "commits_checkpoint")
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        progress["last_completed_sha"], first_sha,
        "the failed main-row write rolls back its paired continuation"
    );
    assert_eq!(progress["snapshot_head"], second_sha);
    rt.sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "DROP TRIGGER fail_second_cursor".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    opts.max_items = Some(1);
    let resumed = run_ingest(&rt, &token, &registry, opts).await.unwrap();
    assert_eq!(resumed.commits_skipped_existing, 1, "{resumed:?}");
    assert_eq!(resumed.commits_ingested, 0, "{resumed:?}");
    assert!(!resumed.done, "exact-budget pass: {resumed:?}");
    assert_eq!(
        read_git_cursor(&rt, project, "commits").await.as_deref(),
        Some(second_sha.as_str())
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_record_stall_survives_atomic_checkpoint_failure() {
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (rt, token, registry) = fixture().await;
        rt.register_embedder(FailOnceEmbedderProvider);
        let project = create(&registry, json!({"kind":"project","name":"two failures"})).await;
        let make = if prs { pr_fixture } else { issue_fixture };
        let mut rejected = make(20, "rejected", TIE);
        if prs {
            rejected["body"] = json!(CURSOR_FAIL_SENTINEL);
        } else {
            rejected["stateReason"] = json!("unknown");
        }
        let rows = vec![make(10, "good", TIE), rejected];
        let (pr_rows, issue_rows) = if prs { (rows, vec![]) } else { (vec![], rows) };
        let (_dir, repo, _logs, _path) = remote_fixture(&pr_rows, &issue_rows);
        let kind = if prs { "prs" } else { "issues" };
        // Sidecar is the first VALUES row, main timestamp the second. Failure
        // of the main-row INSERT must roll back both rows as one checkpoint.
        rt.sql().writer().await.unwrap().execute(SqlStatement {
            sql:format!("CREATE TRIGGER fail_main_cursor BEFORE INSERT ON git_mirror_cursor WHEN NEW.kind='{kind}' BEGIN SELECT RAISE(ABORT,'checkpoint refused'); END"),params:vec![],label:None,
        }).await.unwrap();
        let failed = run_ingest(&rt, &token, &registry, options(&repo, project, prs, 2))
            .await
            .unwrap();
        assert_eq!(
            failed.prs_ingested + failed.issues_ingested,
            1,
            "{failed:?}"
        );
        assert!(
            failed.cursor_stalled && !failed.done,
            "the storage error must preserve the record failure: {failed:?}"
        );
        assert!(
            failed
                .warnings
                .iter()
                .any(|w| w.contains("checkpoint refused")),
            "{failed:?}"
        );
        assert_eq!(read_git_cursor(&rt, project, kind).await, None);
        assert_eq!(
            read_git_cursor(&rt, project, &format!("{kind}_checkpoint")).await,
            None,
            "main-row failure must not leave a replay hint"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_undated_acknowledgments_do_not_fill_across_windows() {
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (rt, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project","name":"undated window rotation"}),
        )
        .await;
        let kind = if prs { "pull_request" } else { "issue" };
        let make = if prs { pr_fixture } else { issue_fixture };
        let rows: Vec<_> = (1..=1000)
            .map(|n| {
                let mut v = make(n, "existing", TIE);
                v["updatedAt"] = Value::Null;
                v
            })
            .collect();
        {
            let writer = rt.backend().pool().try_writer().unwrap();
            writer.conn().execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1000) INSERT INTO notes(id,namespace,kind,name,content,properties,created_at,updated_at) SELECT printf('10000000-0000-4000-8000-%012x',i),'local',?1,'existing','existing',json_object('number',i,'project_id',?2),i,i FROM n", (kind,project.to_string())).unwrap();
        }
        let (pr_rows, issue_rows) = if prs { (rows, vec![]) } else { (vec![], rows) };
        let (_dir, repo, logs, _path) = remote_fixture(&pr_rows, &issue_rows);
        let first = run_ingest(&rt, &token, &registry, options(&repo, project, prs, 1001))
            .await
            .unwrap();
        assert_eq!(
            first.prs_skipped_existing + first.issues_skipped_existing,
            1000,
            "{first:?}"
        );
        assert!(
            !first.done,
            "a full undated page cannot prove exhaustion: {first:?}"
        );
        let mut next = make(1001, "new undated", TIE);
        next["updatedAt"] = Value::Null;
        let response = if prs {
            "pr_response.json"
        } else {
            "issue_response.json"
        };
        std::fs::write(
            logs.join(response),
            json!([next, make(1002, "dated", TIE)]).to_string(),
        )
        .unwrap();
        let second = run_ingest(&rt, &token, &registry, options(&repo, project, prs, 3))
            .await
            .unwrap();
        assert_eq!(
            second.prs_ingested + second.issues_ingested,
            2,
            "{second:?}"
        );
        assert!(
            second.done && !second.cursor_stalled,
            "old absent undated IDs must not permanently fill the checkpoint: {second:?}"
        );
        assert_eq!(
            read_git_cursor(&rt, project, if prs { "prs" } else { "issues" })
                .await
                .as_deref(),
            Some(TIE)
        );
    }
}
