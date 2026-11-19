/// Multiple credential spans across both the title and the body of the same
/// PR must all be masked, and exactly one PR note must be written.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_multiple_credential_spans_in_pr_title_and_body() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-multi-span-repo"}),
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

    let title_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let body_token = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    let pr_json = json!([{
        "number": 13,
        "title": format!("Purge {title_token} from history"),
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/purge",
        "mergeCommit": null,
        "body": format!("Also found {body_token} committed earlier, and again {body_token} in a second commit.")
    }])
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
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "exactly one PR note written: {report:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = list_items(&prs_list);
    assert_eq!(items.len(), 1, "no duplicate PR notes: {items:?}");
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");
    let stored_content = item["content"].as_str().expect("content is string");
    assert!(
        !stored_name.contains(title_token)
            && !stored_title.contains(title_token)
            && !stored_content.contains(body_token),
        "no raw credential span may survive in any surface: \
         name={stored_name:?}, title={stored_title:?}, content={stored_content:?}"
    );
    assert_eq!(
        stored_content.matches("***MASKED***").count(),
        2,
        "both body occurrences of the credential must be masked independently: {stored_content:?}"
    );
    assert!(
        stored_title.contains("***MASKED***"),
        "title span must be masked: {stored_title:?}"
    );
}

/// Issue #763 regression: a clean (non-credential) PR title and a
/// null body must pass through `ingest_prs` byte-for-byte unchanged — a bare
/// 64-hex string with no trigger word nearby is allowlisted by
/// `mask_secrets`, so nothing in this fixture should ever be replaced.
/// Guards against a future over-aggressive masking regression that the
/// detector-positive tests above cannot catch, since they never exercise
/// clean input.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_leaves_clean_pr_title_and_null_body_unmasked() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-clean-title-repo"}),
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

    // A bare 64-hex string (git-SHA/checksum shape) with no trigger word
    // anywhere in the title is allowlisted by `mask_secrets` (`!near_trigger
    // && is_pure_hex`) — this title must survive unchanged.
    let hex64 = "deadbeef".repeat(8);
    let title = format!("Document the {hex64} commit reference");
    let pr_json = json!([{
        "number": 9,
        "title": title,
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "docs/reference",
        "mergeCommit": null,
        "body": null
    }])
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
    .expect("ingest ok");

    assert_eq!(report.prs_ingested, 1, "the PR must land: {report:?}");
    assert_eq!(
        report.prs_skipped_existing, 0,
        "a first-seen PR is never counted as skipped-existing: {report:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = list_items(&prs_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");
    let stored_content = item["content"].as_str().expect("content is string");

    assert_eq!(
        stored_title, title,
        "a clean title must be stored byte-for-byte unchanged"
    );
    assert_eq!(
        stored_name,
        format!("#9 {title}"),
        "the name must be the unmodified `#<number> <title>` form"
    );
    assert_eq!(
        stored_content, "",
        "a null body must serialize as an empty, unmasked content string"
    );
    assert!(
        !stored_title.contains("***MASKED***") && !stored_name.contains("***MASKED***"),
        "no masking marker may appear on clean input: title={stored_title:?}, name={stored_name:?}"
    );

    let project_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": project_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let pr_neighbor_count = project_neighbors
        .as_array()
        .expect("array")
        .iter()
        .filter(|h| h["kind"] == "pull_request")
        .count();
    assert_eq!(
        pr_neighbor_count, 1,
        "the PR's project annotation edge must still materialize for clean input: {project_neighbors:?}"
    );
}

/// Diagnostic #763 order-of-operations invariant: masking must run BEFORE
/// `NAME_MAX_CHARS` truncation, not after. The raw (unmasked) title is
/// constructed long enough that the `#<number> <title>` name exceeds the
/// 120-char budget partway through the credential token, but the masked
/// title (marker shrinks the token from 38 to 12 chars) comfortably fits —
/// a trailing marker placed after the token only survives in the stored
/// name if masking ran first.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_pr_title_before_truncating_name_to_max_chars() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-mask-before-truncate-repo"}),
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

    let filler = "x".repeat(80);
    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    // The token must be its own whitespace-delimited unit (a leading space
    // separates it from `filler`) so the vendor-prefix detector sees a token
    // that actually starts with `ghp_`, not a merged `filler+token` blob.
    let title = format!("{filler} {fake_token} TAIL_SURVIVED_MARKER");
    // Sanity-check the fixture math this test depends on: the raw name (with
    // the full token) must exceed NAME_MAX_CHARS (120) before the tail
    // marker even starts, while the masked name comfortably fits under it.
    let raw_name_len = format!("#1 {title}").len();
    assert!(
        raw_name_len > 120,
        "fixture must exceed NAME_MAX_CHARS pre-mask: {raw_name_len}"
    );

    let pr_json = json!([{
        "number": 1,
        "title": title,
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/order",
        "mergeCommit": null,
        "body": ""
    }])
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
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = list_items(&prs_list);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");

    assert!(
        stored_name.chars().count() <= 120,
        "the stored name must respect NAME_MAX_CHARS: {stored_name:?}"
    );
    assert!(
        !stored_name.contains(fake_token) && !stored_title.contains(fake_token),
        "raw credential must not survive: name={stored_name:?}, title={stored_title:?}"
    );
    assert!(
        stored_name.contains("***MASKED***"),
        "masked marker must be present in the truncated name: {stored_name:?}"
    );
    assert!(
        stored_name.contains("TAIL_SURVIVED_MARKER"),
        "masking must run before truncation so the trailing marker, which would be \
         cut away if the raw (unmasked) name were truncated first, survives: {stored_name:?}"
    );
}

/// Diagnostic #763 interaction check: a masked credential span and a
/// `Fixes #N` cross-reference in the same PR body must not interfere with
/// each other — masking must not corrupt the reference token, and the
/// post-ingest reference-extraction sweep (which runs over the
/// already-masked stored text) must still resolve the reference.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_masks_pr_body_credential_without_breaking_fixes_reference() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-mask-and-reference-repo"}),
    )
    .await;
    let issue_id = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 42, "title": "Some bug", "project_id": project_id.to_string()},
            "annotates": [project_id.to_string()],
        }),
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

    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let pr_json = json!([{
        "number": 7,
        "title": "Rotate the leaked credential",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/rotate",
        "mergeCommit": null,
        "body": format!("Rotated {fake_token} out of the config. Fixes #42.")
    }])
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
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert_eq!(
        report.reference_edges_created, 1,
        "the Fixes #42 reference must resolve even though the body was masked: {report:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = list_items(&prs_list);
    assert_eq!(items.len(), 1);
    let content = items[0]["content"].as_str().expect("content is string");
    assert!(
        !content.contains(fake_token),
        "raw credential must not survive: {content:?}"
    );
    assert!(
        content.contains("***MASKED***"),
        "masked marker must be present: {content:?}"
    );
    assert!(
        content.contains("Fixes #42"),
        "the reference text itself must be retained in stored content: {content:?}"
    );

    let issue_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": issue_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let hits = issue_neighbors.as_array().expect("array");
    assert_eq!(
        hits.len(),
        1,
        "exactly one PR annotates the referenced issue: {hits:?}"
    );
    assert_eq!(hits[0]["kind"], "pull_request");
}
