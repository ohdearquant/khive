/// A commit whose raw touched paths are ALL noncanonical is the third
/// stored state: not a genuinely empty commit (`[]`), so `changed_paths`
/// is omitted entirely and the run's
/// `changed_paths_filtered_noncanonical` count carries the evidence.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_omits_changed_paths_when_all_paths_filtered() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "all-filtered-path-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    // Both shapes the canonical contract can never carry: a `\\` byte and
    // an `X:` drive prefix.
    write(repo, "src/back\\slash.rs", "pub fn backslash() {}\n");
    write(repo, "X:drive.rs", "pub fn drive() {}\n");
    commit(
        repo,
        &["src/back\\slash.rs", "X:drive.rs"],
        "Only non-canonical paths",
    );

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions {
            repo: repo.to_path_buf(),
            project: project_id.to_string(),
            expected_github_repo: None,
            max_items: None,
            include: IngestInclude {
                commits: true,
                issues: false,
                pull_requests: false,
            },
        },
    )
    .await
    .expect("an all-filtered commit must not fail the pass");
    assert_eq!(report.commits_ingested, 1, "{report:?}");
    assert_eq!(
        report.changed_paths_filtered_noncanonical, 2,
        "both noncanonical paths are counted as drops: {report:?}"
    );

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let stored = &list_items(&commits)[0];
    assert!(
        stored["properties"].get("changed_paths").is_none(),
        "an all-filtered commit omits changed_paths rather than storing the \
         [] reserved for a genuinely empty commit: {stored:?}"
    );

    let cursor = read_git_cursor(&rt, project_id, "commits")
        .await
        .expect("cursor must be written");
    assert_eq!(
        cursor,
        head_sha(repo),
        "the cursor advances: an all-filtered commit is still ingested"
    );
}

/// Unix filenames may legitimately contain `\` or start `X:` — shapes the
/// hook's canonical `changed_paths` contract can never carry. The ingester
/// must drop exactly those elements, still store the commit with the
/// canonical remainder (never fail the create and stall the cursor), count
/// the dropped paths, and warn once per run.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_filters_noncanonical_changed_paths_but_keeps_commit() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "noncanonical-path-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "good.rs", "pub fn good() {}\n");
    write(repo, "src/back\\slash.rs", "pub fn backslash() {}\n");
    write(repo, "X:drive.rs", "pub fn drive() {}\n");
    commit(
        repo,
        &["good.rs", "src/back\\slash.rs", "X:drive.rs"],
        "Add canonical and non-canonical paths",
    );

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions {
            repo: repo.to_path_buf(),
            project: project_id.to_string(),
            expected_github_repo: None,
            max_items: None,
            include: IngestInclude {
                commits: true,
                issues: false,
                pull_requests: false,
            },
        },
    )
    .await
    .expect("a non-canonical path must not fail the pass");
    assert_eq!(report.commits_ingested, 1, "{report:?}");
    assert_eq!(
        report.changed_paths_filtered_noncanonical, 2,
        "exactly the backslash and drive-prefixed paths are dropped: {report:?}"
    );
    let filter_warnings: Vec<&str> = report
        .warnings
        .iter()
        .map(String::as_str)
        .filter(|w| w.contains("dropped from changed_paths"))
        .collect();
    assert_eq!(
        filter_warnings.len(),
        1,
        "one bounded warning per run, never one per path: {:?}",
        report.warnings
    );
    assert!(
        filter_warnings[0].contains('2'),
        "the warning carries the dropped count: {}",
        filter_warnings[0]
    );
    assert!(
        filter_warnings[0].contains("NUL"),
        "the warning names every rejected shape the predicate enforces, \
         including the NUL byte: {}",
        filter_warnings[0]
    );

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let stored = &list_items(&commits)[0];
    assert_eq!(
        stored["properties"]["changed_paths"],
        json!(["good.rs"]),
        "the commit lands with exactly the canonical remainder"
    );

    let cursor = read_git_cursor(&rt, project_id, "commits")
        .await
        .expect("cursor must be written");
    assert_eq!(
        cursor,
        head_sha(repo),
        "the cursor advances past the commit: filtered paths never stall a run"
    );
}

/// The canonical filter runs on the RAW path, never the masked one: a
/// secret-shaped token can itself contain a rejected byte (the masker's
/// tokens are whitespace-delimited, so a backslash inside the token is
/// redacted along with it), and filtering after masking would flip the
/// verdict from reject to accept — storing `src/***MASKED***` for a file
/// whose real name is non-canonical. The defect must be dropped and
/// counted, not laundered into a canonical-looking masked path.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_filters_noncanonical_path_even_when_masking_hides_the_defect() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "masked-defect-path-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "good.rs", "pub fn good() {}\n");
    // Secret-shaped (github-token: `ghp_` + 36 base62 chars) AND
    // non-canonical (a `\` byte inside the token).
    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let rel = format!("src/{fake_token}\\back.rs");
    write(repo, &rel, "pub fn secret_defect() {}\n");
    commit(repo, &["good.rs", rel.as_str()], "Mixed paths");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1, "{report:?}");
    assert_eq!(
        report.changed_paths_filtered_noncanonical, 1,
        "the raw path's backslash rejects it even though masking would \
         redact that byte away: {report:?}"
    );

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let stored = &list_items(&commits)[0];
    assert_eq!(
        stored["properties"]["changed_paths"],
        json!(["good.rs"]),
        "the non-canonical path must be dropped, not stored as a \
         canonical-looking masked residue: {stored:?}"
    );
    assert!(
        !format!("{stored}").contains(fake_token),
        "the raw token must not survive anywhere in the stored record"
    );
}

/// The stalled-cursor contract relies on sha-keyed dedup: a record that
/// lands AFTER a stall is re-walked by the next pass (the cursor froze
/// before it) and must come out as `skipped_existing`, never a duplicate
/// row. Pass 1 stalls on commit B (injected embedder failure) while later
/// commit C still lands; pass 2 re-walks `first..HEAD`, retries B
/// successfully, and re-encounters C — total stored rows must stay exactly
/// one per sha.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_repeat_pass_after_stall_creates_no_duplicates() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    rt.register_embedder(FailOnceEmbedderProvider);
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "stall-redo-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "a.rs", "pub fn a() {}\n");
    commit(repo, &["a.rs"], "Add a");
    let sha_a = head_sha(repo);
    write(repo, "b.rs", "pub fn b() {}\n");
    commit(repo, &["b.rs"], &format!("Add b {CURSOR_FAIL_SENTINEL}"));
    let sha_b = head_sha(repo);
    write(repo, "c.rs", "pub fn c() {}\n");
    commit(repo, &["c.rs"], "Add c");
    let sha_c = head_sha(repo);

    let opts = || IngestOptions {
        repo: repo.to_path_buf(),
        project: project_id.to_string(),
        expected_github_repo: None,
        max_items: None,
        include: IngestInclude {
            commits: true,
            issues: false,
            pull_requests: false,
        },
    };

    let report1 = run_ingest(&rt, &token, &registry, opts())
        .await
        .expect("pass 1 completes despite the injected failure");
    assert_eq!(
        report1.commits_ingested, 2,
        "A and C land; B fails once on the injected embedder error: {report1:?}"
    );
    assert_eq!(
        read_git_cursor(&rt, project_id, "commits").await.as_deref(),
        Some(sha_a.as_str()),
        "pass 1 cursor stalls at A, before failed B"
    );

    let report2 = run_ingest(&rt, &token, &registry, opts())
        .await
        .expect("pass 2 completes");
    assert_eq!(
        report2.commits_ingested, 1,
        "only B is newly created on the retry: {report2:?}"
    );
    assert_eq!(
        report2.commits_skipped_existing, 1,
        "C is re-walked from the stalled cursor and deduplicated by sha: {report2:?}"
    );
    assert_eq!(
        read_git_cursor(&rt, project_id, "commits").await.as_deref(),
        Some(sha_c.as_str()),
        "pass 2 cursor advances to HEAD"
    );

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let mut shas: Vec<&str> = list_items(&commits)
        .iter()
        .map(|note| note["properties"]["sha"].as_str().expect("sha"))
        .collect();
    shas.sort_unstable();
    let mut expected = [sha_a.as_str(), sha_b.as_str(), sha_c.as_str()];
    expected.sort_unstable();
    assert_eq!(
        shas, expected,
        "exactly one stored row per sha after the repeat pass"
    );
}
