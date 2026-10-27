/// A database error during the first commit lookup is reported after the
/// commit walk has started, not as a pre-walk hard failure.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_commit_lookup_failure_is_reported_in_band_after_walk_start() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-lookup-failure-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");

    // SQLite evaluates an index expression on every insert, and `json_extract`
    // raises on invalid JSON — so a malformed-JSON row cannot land while ANY
    // index evaluates `json_extract` over `notes.properties`.
    //
    // Dropping one such index BY NAME is structurally fragile: this test used
    // to name `idx_comm_message_external_id` alone, and any later migration
    // adding a second json-evaluating index on `notes` silently re-imposes the
    // constraint, failing this test with a bare "malformed JSON" from an insert
    // that looks unrelated to the migration that caused it. Drop the CLASS
    // instead (test database only) — enumerate the json-evaluating indexes on
    // `notes` and drop every one. The lookup's own `json_extract` then still
    // fails at query time, mid-walk, which is the behaviour under test.
    let json_indexes = {
        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("sql reader");
        reader
            .query_all(SqlStatement {
                sql: "SELECT name FROM sqlite_master \
                      WHERE type = 'index' AND tbl_name = 'notes' \
                        AND sql LIKE '%json_extract%'"
                    .into(),
                params: vec![],
                label: Some("test_list_json_indexes_on_notes".into()),
            })
            .await
            .expect("list json-evaluating indexes on notes")
    };
    // Empty is not clean: if the enumeration silently returned nothing, the
    // sabotage row below would insert for the wrong reason and this test would
    // pass while asserting nothing.
    assert!(
        !json_indexes.is_empty(),
        "expected at least one json-evaluating index on notes; an empty result \
         means the enumeration is broken, not that the schema has none"
    );

    let mut writer = rt.sql().writer().await.expect("writer");
    for row in &json_indexes {
        let Some(SqlValue::Text(name)) = row.get("name") else {
            panic!("sqlite_master.name must be text");
        };
        writer
            .execute(SqlStatement {
                sql: format!("DROP INDEX IF EXISTS {name}"),
                params: vec![],
                label: Some("test_drop_json_index".into()),
            })
            .await
            .expect("drop json-evaluating index");
    }
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO notes(id, namespace, kind, content, properties, created_at, updated_at) \
                  VALUES(?1, ?2, 'commit', '', ?3, 0, 0)"
                .into(),
            params: vec![
                SqlValue::Text(Uuid::new_v4().to_string()),
                SqlValue::Text("local".into()),
                SqlValue::Text("not-json".into()),
            ],
            label: Some("test_malformed_commit_properties".into()),
        })
        .await
        .expect("insert malformed lookup row");
    drop(writer);

    let mut opts = IngestOptions::unbounded(repo, project_id.to_string());
    opts.include.issues = false;
    opts.include.pull_requests = false;
    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("a mid-walk lookup failure stays in-band");

    match &report.sources.commits {
        Some(khive_pack_git::ingest::IngestSourceState::StoppedEarly(reason)) => {
            assert!(
                reason.contains("walk began") && reason.contains("pass then failed after the walk"),
                "the source records a walked-then-failed lookup: {reason:?}"
            );
        }
        other => panic!("a mid-walk lookup failure must be stopped-early: {other:?}"),
    }
    assert!(!report.done, "{report:?}");
    assert!(!report.history_exhausted, "{report:?}");
}

/// The commit walker's post-walk cursor-write failure is reported in-band:
/// the walk records `completed` and THEN the final cursor write fails (here
/// via the same sabotage trigger the issue-side test uses). The call site
/// must handle that in-band exactly like the gh walkers — downgrade to
/// `stopped_early`, warn, force `done = false` — instead of aborting the
/// whole ingest with an Err.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_commit_walked_then_cursor_write_fails_stays_in_band() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-walked-then-failed-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");

    // Sabotage ONLY the cursor write: the commit itself must land first, so
    // the walker has recorded its walk-start state before the checkpoint fails.
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
    opts.include.issues = false;
    opts.include.pull_requests = false;
    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("a post-walk commit cursor failure must stay in-band, not abort the pass");

    assert_eq!(
        report.commits_ingested, 1,
        "the commit landed before the cursor write failed: {report:?}"
    );
    match &report.sources.commits {
        Some(khive_pack_git::ingest::IngestSourceState::StoppedEarly(reason)) => {
            assert!(
                reason.contains("pass then failed after the walk")
                    && reason.contains("sabotaged cursor write"),
                "the walked commit stays stopped-early with the cause: {reason:?}"
            );
        }
        other => {
            panic!("a walked-then-failed commit source is never skipped or completed: {other:?}")
        }
    }
    assert!(
        !report.cursor_stalled,
        "no PER-RECORD write failed — the cursor write did; the two signals stay distinct: {report:?}"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("commit ingest failed after the walk")),
        "the failure surfaces as a warning beside the downgraded state: {:?}",
        report.warnings
    );
    assert!(
        !report.done,
        "the resume loop must re-walk a source whose pass failed: {report:?}"
    );
    assert!(
        !report.history_exhausted,
        "a failed pass must not claim exhaustion: {report:?}"
    );
}

/// Sources not requested by `include` serialize as JSON `null` in
/// `IngestReport.sources`: `Option<IngestSourceState>`
/// with serde's default `None` encoding, asserted explicitly so a future
/// `skip_serializing_if` or repr change cannot silently drop the
/// distinction between "not requested" and "requested but state lost".
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_report_serializes_omitted_sources_as_null() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "null-sources-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");

    // Commits only: no gh probe, no gh fixture needed.
    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.issues = false;
    opts.include.pull_requests = false;
    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok");

    let serialized = serde_json::to_value(&report).expect("report serializes");
    assert_eq!(
        serialized["sources"]["commits"]["state"], "completed",
        "the requested source carries its walk state: {serialized}"
    );
    assert!(
        serialized["sources"]["issues"].is_null(),
        "an unrequested source is JSON null, not an absent key or a state: {serialized}"
    );
    assert!(
        serialized["sources"]["pull_requests"].is_null(),
        "an unrequested source is JSON null, not an absent key or a state: {serialized}"
    );
    assert_eq!(
        serialized["history_exhausted"], true,
        "a fully walked requested source exhausts the pass: {serialized}"
    );
}

/// A local cursor read failure happens before either remote listing. It keeps
/// the source-level failure distinct from a remote listing skip.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_local_cursor_read_failure_is_not_remote_listing_skip() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "cursor-read-failure-repo"}),
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
    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let mut writer = rt.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "DROP TABLE git_mirror_cursor".into(),
            params: vec![],
            label: Some("test_drop_cursor_table".into()),
        })
        .await
        .expect("drop cursor table");
    drop(writer);

    let mut opts = IngestOptions::unbounded(repo, project_id.to_string());
    opts.include.commits = false;
    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("a local cursor read failure stays in-band");

    for (source, state) in [
        ("pull requests", &report.sources.pull_requests),
        ("issues", &report.sources.issues),
    ] {
        match state {
            Some(khive_pack_git::ingest::IngestSourceState::Skipped(reason)) => {
                assert!(
                    reason.contains("local cursor/database read failed"),
                    "{source}: local cursor failure is classified explicitly: {reason:?}"
                );
                assert!(
                    !reason.contains("gh pr list failed")
                        && !reason.contains("gh issue list failed"),
                    "{source}: the state is not a remote listing failure: {reason:?}"
                );
            }
            other => panic!("{source}: expected local cursor read classification, got {other:?}"),
        }
    }
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| !warning.contains("list failed, skipping")),
        "a local cursor failure must not use the remote-skip warning: {:?}",
        report.warnings
    );
}
