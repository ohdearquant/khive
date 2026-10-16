// ── Issue #841: residual unmasked ingest fields ─────────────────────────────
//
// PR #835's `ingest_masks_secrets_in_commit_message` and the PR-title/body
// tests above already cover `content`/`title`
// masking. These tests cover the fields #841 found still passed raw into
// gated `properties`/the note `name`: the commit note `name` (built from the
// raw subject), commit `author`/`author_email`, and PR `author`/`base_ref`/
// `head_ref`. Each fix pairs a credential-shaped positive (record must not
// be dropped, field must be masked) with a clean-input regression guard
// (field must survive byte-for-byte unchanged).

/// The commit note `name` was built from the raw (unmasked) commit subject
/// even though `content` already masked the same text — a credential-shaped
/// subject line tripped the runtime's `secret_gate::check(name)` call and
/// silently dropped the whole commit, despite `content` being safe.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_commit_subject_in_name_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-subject-credential-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    let fake_token = "ghp_EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE";
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], fake_token);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.commits_ingested, 1,
        "the commit must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("create commit")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list_items(&list);
    assert_eq!(items.len(), 1);
    let stored_name = items[0]["name"].as_str().expect("name is string");
    assert!(
        !stored_name.contains(fake_token),
        "raw token must not survive into the stored name: {stored_name:?}"
    );
    assert!(
        stored_name.contains("***MASKED***"),
        "masked marker must be present in name: {stored_name:?}"
    );
}

/// The commit `author` field entered gated `properties` raw — a
/// credential-shaped `git config user.name` silently dropped the commit via
/// the runtime's recursive `properties` secret scan.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_commit_author_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-author-credential-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    let fake_token = "ghp_FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
    git(repo, &["config", "user.name", fake_token]);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Normal commit message");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.commits_ingested, 1,
        "the commit must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("create commit")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list_items(&list);
    assert_eq!(items.len(), 1);
    let stored_author = items[0]["properties"]["author"]
        .as_str()
        .expect("properties.author is string");
    assert!(
        !stored_author.contains(fake_token),
        "raw token must not survive into properties.author: {stored_author:?}"
    );
    assert!(
        stored_author.contains("***MASKED***"),
        "masked marker must be present in properties.author: {stored_author:?}"
    );
}

/// The commit `author_email` field entered gated `properties` raw — a
/// credential-shaped `git config user.email` silently dropped the commit via
/// the runtime's recursive `properties` secret scan.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_credential_shaped_commit_author_email_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-author-email-credential-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    let fake_token = "ghp_GGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG";
    git(
        repo,
        &["config", "user.email", &format!("{fake_token}@example.com")],
    );
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Normal commit message");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.commits_ingested, 1,
        "the commit must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("create commit")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list_items(&list);
    assert_eq!(items.len(), 1);
    let stored_email = items[0]["properties"]["author_email"]
        .as_str()
        .expect("properties.author_email is string");
    assert!(
        !stored_email.contains(fake_token),
        "raw token must not survive into properties.author_email: {stored_email:?}"
    );
    assert!(
        stored_email.contains("***MASKED***"),
        "masked marker must be present in properties.author_email: {stored_email:?}"
    );
}

/// Regression guard: clean (non-credential-shaped) commit author, email, and
/// subject must survive byte-for-byte unchanged — the fix above must not
/// become an over-aggressive masking regression.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_leaves_clean_commit_author_and_subject_unmasked() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-clean-fields-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    git(repo, &["config", "user.name", "Ada Lovelace"]);
    git(repo, &["config", "user.email", "ada@example.com"]);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Add README with usage notes");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1, "{report:?}");

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list_items(&list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_author = item["properties"]["author"]
        .as_str()
        .expect("author is string");
    let stored_email = item["properties"]["author_email"]
        .as_str()
        .expect("author_email is string");
    let stored_name = item["name"].as_str().expect("name is string");
    assert_eq!(
        stored_author, "Ada Lovelace",
        "clean author must be stored unchanged"
    );
    assert_eq!(
        stored_email, "ada@example.com",
        "clean author_email must be stored unchanged"
    );
    assert!(
        stored_name.ends_with("Add README with usage notes"),
        "clean subject must survive unmodified in name: {stored_name:?}"
    );
    assert!(
        !stored_author.contains("***MASKED***")
            && !stored_email.contains("***MASKED***")
            && !stored_name.contains("***MASKED***"),
        "no masking marker may appear on clean input: author={stored_author:?} email={stored_email:?} name={stored_name:?}"
    );
}
