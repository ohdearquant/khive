/// Issue #1604: `git log -z` is required for an exact path identity. Git's
/// display form quotes non-ASCII and delimiter-bearing names, which would not
/// equal ADR-085's filesystem-derived `source_path`.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_preserves_unicode_and_delimiter_bearing_changed_paths() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "raw-path-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    // Backslash is deliberately absent: the hook's canonical shape is
    // `/`-separated repo-relative and rejects `\` anywhere, so a
    // backslash-bearing filesystem name can never round-trip through
    // `changed_paths`. Tab, quote, newline, and non-ASCII coverage remain —
    // all of them still trigger git's quoted display form without `-z`.
    let unusual_path = "src/café\t\"quoted\"leaf\nline.rs";
    write(repo, unusual_path, "pub fn unusual() {}\n");
    commit(repo, &[unusual_path], "Add unusual path");
    let revision = head_sha(repo);

    let module_id = create(
        &registry,
        json!({
            "kind": "concept",
            "entity_type": "module",
            "name": "unusual_module",
            "properties": {
                "source_project": "raw-path-repo",
                "source_path": unusual_path,
                "source_revision": revision
            }
        }),
    )
    .await;

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1, "{report:?}");

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    assert_eq!(
        list_items(&commits)[0]["properties"]["changed_paths"],
        json!([unusual_path])
    );
    assert_eq!(
        incoming_annotating_ids(&registry, module_id).await.len(),
        1,
        "raw path must retain its exact module binding"
    );
}

/// Issue #1604: merge commits use their first-parent diff as their one
/// canonical changed-path set. This records the change introduced to the
/// destination branch without producing one path set per parent.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_records_first_parent_paths_for_merge_commits() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "merge-path-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "base.rs", "pub fn base() {}\n");
    commit(repo, &["base.rs"], "Base");

    git(repo, &["checkout", "-q", "-b", "feature"]);
    write(repo, "feature.rs", "pub fn feature() {}\n");
    commit(repo, &["feature.rs"], "Feature");

    git(repo, &["checkout", "-q", "main"]);
    write(repo, "main.rs", "pub fn main_change() {}\n");
    commit(repo, &["main.rs"], "Main change");
    git(
        repo,
        &["merge", "-q", "--no-ff", "feature", "-m", "Merge feature"],
    );
    let merge_sha = head_sha(repo);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 4, "{report:?}");

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let merge = list_items(&commits)
        .iter()
        .find(|note| note["properties"]["sha"] == merge_sha)
        .expect("merge commit");
    assert_eq!(
        merge["properties"]["changed_paths"],
        json!(["feature.rs"]),
        "merge path set is the diff against its first parent"
    );
}

/// Issue #1604: `[]` is the canonical shape for a genuinely empty commit
/// (ADR-088: "the git ingester always supplies it, including an empty array
/// for an empty commit"). Exercise the `--allow-empty` path end to end so
/// the contract cannot silently rot.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_records_empty_changed_paths_for_empty_commits() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "empty-commit-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "base.rs", "pub fn base() {}\n");
    commit(repo, &["base.rs"], "Base");
    git(
        repo,
        &["commit", "-q", "--allow-empty", "-m", "Empty marker"],
    );
    let empty_sha = head_sha(repo);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 2, "{report:?}");

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let empty = list_items(&commits)
        .iter()
        .find(|note| note["properties"]["sha"] == empty_sha)
        .expect("empty commit");
    assert_eq!(
        empty["properties"]["changed_paths"],
        json!([]),
        "an empty commit carries an empty changed-path array"
    );
}

/// Issue #1604: rename detection is pinned off, so a rename surfaces as the
/// delete + add path pair that keeps `changed_paths` an exact join key for
/// ADR-085's filesystem-derived `source_path`. Without the pin, whether the
/// old path appears in `--name-only` output would depend on the rename
/// detection settings of whatever git build runs the ingest.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_records_both_sides_of_a_rename() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(&registry, json!({"kind": "project", "name": "rename-repo"})).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "old.rs", "pub fn renamed() {}\n");
    commit(repo, &["old.rs"], "Add old.rs");
    git(repo, &["mv", "old.rs", "new.rs"]);
    git(repo, &["commit", "-q", "-m", "Rename old.rs to new.rs"]);
    let rename_sha = head_sha(repo);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 2, "{report:?}");

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let rename = list_items(&commits)
        .iter()
        .find(|note| note["properties"]["sha"] == rename_sha)
        .expect("rename commit");
    assert_eq!(
        rename["properties"]["changed_paths"],
        json!(["new.rs", "old.rs"]),
        "a rename records both the deleted and the added path"
    );
}

/// A walked commit with no touched-path entry must never be stored with a
/// fabricated `[]`: the run warns naming the commit, stalls the cursor at
/// the last contiguous successful commit, and still completes (see
/// docs/api/ingest.md#changed-paths-and-code-module-annotations).
///
/// The shim below builds the REAL orphan-token shape, not a whole-record
/// deletion: its `tr '\0' '\n'` round-trip splits git's combined
/// `<header>\n<first-path>` token into two lines, so `grep -av` removes
/// only the middle commit's header LINE while its path token survives in
/// the stream. Because the `--name-only` walk is newest-first, the parser
/// attaches that orphan token to the most recent header — the NEWER (third)
/// commit — and the walk-commits pass (unshimmed) still walks all three
/// shas. Containment is therefore per-record, not per-stream: the middle
/// commit never lands (no path-set entry), the cursor stalls, but the
/// orphaned token pollutes the stored newer commit's immutable
/// `changed_paths` — asserted explicitly below, because commit notes are
/// immutable and the pollution persists until re-ingest.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_stalls_cursor_for_commit_missing_touched_paths() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "touched-gap-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "first.rs", "pub fn first() {}\n");
    commit(repo, &["first.rs"], "First");
    let first_sha = head_sha(repo);
    write(repo, "second.rs", "pub fn second() {}\n");
    commit(repo, &["second.rs"], "Second");
    let second_sha = head_sha(repo);
    write(repo, "third.rs", "pub fn third() {}\n");
    commit(repo, &["third.rs"], "Third");
    let third_sha = head_sha(repo);

    // The `--name-only` pass is the sole authority for touched paths. This
    // PATH-shadowing shim deletes ONLY the middle commit's header line from
    // that one stream — the `tr` round-trip puts the header and its first
    // path on separate lines, so `grep -av` strips just the header and the
    // middle commit's path token stays in the stream as an orphan — making
    // the two snapshot passes disagree exactly the way the degradation
    // branch handles; every other invocation delegates to the real git
    // binary (same technique as `src/recovery_tests.rs`).
    let real_git = resolve_real_git();
    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");
    std::fs::write(
        bin_dir.join("git"),
        format!(
            r#"#!/bin/sh
REAL_GIT="{real_git}"
case " $* " in
  *" --name-only "*)
    "$REAL_GIT" "$@" | tr '\0' '\n' | grep -av '{second_sha}' | tr '\n' '\0'
    exit 0
    ;;
esac
exec "$REAL_GIT" "$@"
"#,
            real_git = real_git,
            second_sha = second_sha,
        ),
    )
    .expect("write git shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(bin_dir.join("git"))
            .expect("shim metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(bin_dir.join("git"), perms).expect("chmod shim");
    }
    let _path_guard = PathGuard::install(&bin_dir);

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
    .expect("the run completes despite the touched-path gap");
    assert!(
        !report.done,
        "a touched-path gap stalls the cursor, so the pass must not report done: {report:?}"
    );
    assert!(
        report.cursor_stalled,
        "the touched-path gap must surface as a cursor stall: {report:?}"
    );
    assert!(
        report.warnings.iter().any(|w| {
            w.contains("no touched-path set recorded")
                && w.contains(&second_sha)
                && w.contains(&third_sha)
        }),
        "the gap warning must name both the stalled commit and the newer polluted recipient: {:?}",
        report.warnings
    );
    assert_eq!(
        report.commits_ingested, 2,
        "the two commits with recorded path sets still land: {report:?}"
    );

    let cursor = read_git_cursor(&rt, project_id, "commits")
        .await
        .expect("cursor must be written");
    assert_eq!(
        cursor, first_sha,
        "the cursor must stall at the last contiguous successful commit, \
         never advance past the gap"
    );

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let commit_notes = list_items(&commits);
    let stored_shas: Vec<&str> = commit_notes
        .iter()
        .map(|note| note["properties"]["sha"].as_str().expect("sha"))
        .collect();
    assert!(
        !stored_shas.contains(&second_sha.as_str()),
        "the gap commit must never be stored with a fabricated []: {stored_shas:?}"
    );
    assert!(
        stored_shas.contains(&third_sha.as_str()),
        "a commit after the gap is still attempted and lands: {stored_shas:?}"
    );
    let first_note = commit_notes
        .iter()
        .find(|note| note["properties"]["sha"].as_str() == Some(first_sha.as_str()))
        .expect("the first commit is stored");
    assert_eq!(
        first_note["properties"]["changed_paths"],
        json!(["first.rs"]),
        "the newest-first walk attaches the orphan token to the most \
         recent header (the NEWER commit), so the older commit keeps \
         exactly its own paths"
    );
    let third_note = commit_notes
        .iter()
        .find(|note| note["properties"]["sha"].as_str() == Some(third_sha.as_str()))
        .expect("the newer commit is stored");
    assert_eq!(
        third_note["properties"]["changed_paths"],
        json!(["second.rs", "third.rs"]),
        "the orphaned path token of the deleted header rides into the \
         stored NEWER commit's changed_paths: containment is per-record \
         (the missing sha never lands, the cursor stalls), never \
         per-stream — commit notes are immutable, so the pollution \
         persists (a re-ingest skips the stored sha)"
    );
}
