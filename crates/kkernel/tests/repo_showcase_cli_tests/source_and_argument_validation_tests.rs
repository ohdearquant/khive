#[test]
fn build_refuses_store_reuse_and_missing_commits() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let work = tmp.path().join("work");
    let output = tmp.path().join("bundle.json");
    let first = build(&home, &repo, &work, &output, "commits");
    assert!(first.status.success());

    let reuse = build(&home, &repo, &work, &output, "commits");
    assert!(!reuse.status.success());
    assert!(String::from_utf8_lossy(&reuse.stderr).contains("already exists"));

    let no_commits = build(
        &home,
        &repo,
        &tmp.path().join("no-commits"),
        &tmp.path().join("no-commits.json"),
        "issues",
    );
    assert!(!no_commits.status.success());
    assert!(String::from_utf8_lossy(&no_commits.stderr).contains("requires commits"));

    let inside = build(
        &home,
        &repo,
        &repo.join(".showcase-work"),
        &tmp.path().join("inside.json"),
        "commits",
    );
    assert!(!inside.status.success());
    assert!(String::from_utf8_lossy(&inside.stderr).contains("must be outside"));
    assert!(!repo.join(".showcase-work").exists());
}

#[test]
fn local_revision_must_be_head_and_generated_at_must_follow_it() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let repo = fixture_repo(tmp.path());
    let first_revision = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("src/greet.rs"),
        "pub fn greeting() -> &'static str { \"hello, v2\" }\n",
    )
    .expect("update fixture module");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "update greeting"]);

    let revision_work = tmp.path().join("revision-work");
    let revision_bundle = tmp.path().join("revision.json");
    let revision_run = isolated_command(&home)
        .args([
            "repo",
            "build",
            "--source",
            repo.to_str().unwrap(),
            "--revision",
            &first_revision,
            "--work-dir",
            revision_work.to_str().unwrap(),
            "--include",
            "commits",
            "--generated-at",
            "2026-08-07T16:00:00Z",
            "--out",
            revision_bundle.to_str().unwrap(),
        ])
        .output()
        .expect("run pinned local build");
    assert!(!revision_run.status.success());
    assert!(String::from_utf8_lossy(&revision_run.stderr).contains("local source HEAD"));

    let timestamp_work = tmp.path().join("timestamp-work");
    let timestamp_bundle = tmp.path().join("timestamp.json");
    let timestamp_run = isolated_command(&home)
        .args([
            "repo",
            "build",
            "--source",
            repo.to_str().unwrap(),
            "--work-dir",
            timestamp_work.to_str().unwrap(),
            "--include",
            "commits",
            "--generated-at",
            "2026-08-07T14:59:59Z",
            "--out",
            timestamp_bundle.to_str().unwrap(),
        ])
        .output()
        .expect("run build with pre-snapshot timestamp");
    assert!(!timestamp_run.status.success());
    assert!(String::from_utf8_lossy(&timestamp_run.stderr).contains("predates HEAD commit time"));
}

#[test]
fn remote_urls_reject_sensitive_components_before_clone_resolution() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    let cases = [
        (
            "https://agent:sensitive-password@example.invalid/owner/repository",
            "userinfo or credentials",
        ),
        (
            "https://example.invalid/owner/repository?token=sensitive-query",
            "query component",
        ),
        (
            "https://example.invalid/owner/repository#sensitive-fragment",
            "fragment component",
        ),
    ];

    for (index, (source, expected)) in cases.into_iter().enumerate() {
        let work = tmp.path().join(format!("work-{index}"));
        let bundle = tmp.path().join(format!("bundle-{index}.json"));
        let output = isolated_command(&home)
            .args([
                "repo",
                "build",
                "--source",
                source,
                "--work-dir",
                work.to_str().unwrap(),
                "--include",
                "commits",
                "--generated-at",
                "2026-08-07T16:00:00Z",
                "--out",
                bundle.to_str().unwrap(),
            ])
            .output()
            .expect("run build with unsafe remote URL");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "expected {expected:?} rejection, got {stderr}"
        );
        assert!(!stderr.contains("sensitive-"), "secret leaked: {stderr}");
        assert!(
            !work.exists(),
            "unsafe URL reached worktree/cache materialization"
        );
        assert!(!bundle.exists());
    }
}
