/// Presence on PATH is insufficient: an installed but unauthenticated (or
/// repo-incompatible) `gh` reports `gh_available:false`, skips requested
/// remote sources, never starts a walker, and does not echo probe stderr.
#[cfg(unix)]
#[tokio::test]
async fn digest_remote_issues_only_never_invokes_git_clone() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let bin_dir = dir.path().join("bin");
    let scratch = dir.path().join("scratch");
    let git_log = dir.path().join("git-args.log");
    let gh_log = dir.path().join("gh-args.log");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");

    let git_script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 97\n",
        git_log.display()
    );
    let gh_script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1 $2" in
  "repo view")
    echo '{{"nameWithOwner":"fixture/repository","url":"https://github.com/fixture/repository"}}'
    ;;
  "issue list")
    echo '[]'
    ;;
  *)
    exit 98
    ;;
esac
"#,
        gh_log.display()
    );
    for (name, script) in [("git", git_script), ("gh", gh_script)] {
        let path = bin_dir.join(name);
        std::fs::write(&path, script).expect("write command stub");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .expect("stub metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod stub");
    }

    let _path_guard = PathGuard::install(&bin_dir);
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", &scratch);
    let result = registry
        .dispatch(
            "git.digest",
            json!({
                "source": "https://github.com/fixture/repository",
                "include": ["issues"]
            }),
        )
        .await;
    std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");

    let response = result.expect("issues-only remote digest must not require a clone");
    assert_eq!(response["gh_available"], true, "{response}");
    assert_eq!(response["history_exhausted"], true, "{response}");
    assert_eq!(response["sources"]["issues"]["state"], "completed");
    assert!(response["sources"]["commits"].is_null(), "{response}");
    assert!(
        !git_log.exists(),
        "issues-only remote digest must never invoke git: {}",
        std::fs::read_to_string(&git_log).unwrap_or_default()
    );
    let gh_invocations = std::fs::read_to_string(gh_log).expect("gh invocation log");
    let lines: Vec<&str> = gh_invocations.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.first().copied(),
        Some("repo view fixture/repository --json nameWithOwner,url"),
        "the probe must never delegate repository selection to gh: {lines:?}"
    );
    for line in lines.iter().skip(1) {
        assert_eq!(
            repo_flag_value(line),
            Some("fixture/repository"),
            "every issue list call must retain the source-bound repo, exactly: {line:?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_installed_but_unusable_gh_is_reported_false_without_leaking_stderr() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");
    git(
        &repo,
        &[
            "config",
            "remote.origin.url",
            "https://github.com/fixture/repository.git",
        ],
    );

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let args_log = dir.path().join("gh-args.log");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  repo)
    echo 'authentication failed for token GH_SECRET_DO_NOT_PERSIST' 1>&2
    exit 1
    ;;
  *)
    echo 'listing must not run after a failed repo probe' 1>&2
    exit 99
    ;;
esac
"#,
        args_log.display()
    );
    let script_path = bin_dir.join("gh");
    std::fs::write(&script_path, script).expect("write gh stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script_path)
            .expect("stub metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script_path, permissions).expect("chmod stub");
    }
    let _path_guard = PathGuard::install(&bin_dir);

    let response = registry
        .dispatch(
            "git.digest",
            json!({
                "source": repo.to_str().unwrap(),
                "include": ["commits", "issues", "pull_requests"]
            }),
        )
        .await
        .expect("unusable gh degrades in-band");

    assert_eq!(response["gh_available"], false, "{response}");
    assert_eq!(response["commits_ingested"], 1, "{response}");
    assert_eq!(response["sources"]["issues"]["state"], "skipped");
    assert_eq!(response["sources"]["pull_requests"]["state"], "skipped");
    assert!(
        response["sources"]["issues"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("could not resolve")),
        "structured reason describes repository usability: {response}"
    );
    assert!(
        !response.to_string().contains("GH_SECRET_DO_NOT_PERSIST"),
        "gh stderr must never enter the report or durable receipt: {response}"
    );
    let invocations = std::fs::read_to_string(args_log).expect("gh args log");
    assert_eq!(
        invocations.lines().collect::<Vec<_>>(),
        vec!["repo view fixture/repository --json nameWithOwner,url"],
        "failed capability probe must prevent every listing call"
    );
}

/// `gh repo set-default` can select an alternate remote for argument-less
/// commands. The capability probe must instead derive `origin` and pass that
/// repository as argv, then pin the same value on every list call.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_multi_remote_configured_default_cannot_redirect_github_ingest() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "source-bound-gh-probe"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/fixture/repository.git",
        ],
    );
    git(
        &repo,
        &[
            "remote",
            "add",
            "upstream",
            "https://github.com/alternate/wrong-repository.git",
        ],
    );
    // Mirrors a checkout where gh has marked the alternate remote as its
    // local default. The fake below deliberately returns that wrong repo if
    // `repo view` is ever invoked without the explicit source identity.
    git(&repo, &["config", "remote.upstream.gh-resolved", "base"]);

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let args_log = dir.path().join("gh-args.log");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  repo)
    if [ "$3" = "fixture/repository" ]; then
      echo '{{"nameWithOwner":"fixture/repository","url":"https://github.com/fixture/repository"}}'
    else
      echo '{{"nameWithOwner":"alternate/wrong-repository","url":"https://github.com/alternate/wrong-repository"}}'
    fi
    ;;
  pr|issue)
    echo '[]'
    ;;
  *)
    exit 2
    ;;
esac
"#,
        args_log.display()
    );
    let script_path = bin_dir.join("gh");
    std::fs::write(&script_path, script).expect("write gh stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script_path)
            .expect("stub metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script_path, permissions).expect("chmod stub");
    }
    let _path_guard = PathGuard::install(&bin_dir);

    let response = registry
        .dispatch(
            "git.digest",
            json!({
                "source": repo.to_str().unwrap(),
                "project": project_id,
                "include": ["issues", "pull_requests"]
            }),
        )
        .await
        .expect("source-bound GitHub ingest succeeds");
    assert_eq!(response["gh_available"], true, "{response}");
    assert_eq!(response["history_exhausted"], true, "{response}");

    let invocations = std::fs::read_to_string(args_log).expect("gh args log");
    let lines: Vec<&str> = invocations.lines().collect();
    assert_eq!(
        lines.first().copied(),
        Some("repo view fixture/repository --json nameWithOwner,url"),
        "the probe must never delegate repository selection to gh: {lines:?}"
    );
    for line in lines.iter().skip(1) {
        assert_eq!(
            repo_flag_value(line),
            Some("fixture/repository"),
            "every list call must retain the source-bound repo, exactly: {line:?}"
        );
    }
}

/// A `gh pr list`/`gh issue list` failure on the FIRST fetch — the stub
/// answers the source-bound probe but exits 1 on listing — leaves both
/// sources `skipped` with the failure in the reason: the source was never
/// walked, which is exactly what `skipped` means (issue #1617).
/// Commits are unaffected (ADR-088 §5 graceful absence).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_pr_issue_sources_skipped_on_gh_list_failure() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");
    git(
        &repo,
        &[
            "config",
            "remote.origin.url",
            "https://github.com/fixture/repository.git",
        ],
    );

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");

    // Resolve a repository successfully, then fail every listing.
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
    let script_path = bin_dir.join("gh");
    std::fs::write(&script_path, script).expect("write failing gh stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .expect("stub metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod stub");
    }
    let _path_guard = PathGuard::install(&bin_dir);

    let resp = registry
        .dispatch(
            "git.digest",
            json!({
                "source": repo.to_str().unwrap(),
                "include": ["commits", "issues", "pull_requests"]
            }),
        )
        .await
        .expect("a gh listing failure must stay an in-band digest result");

    assert_eq!(resp["commits_ingested"].as_u64().unwrap(), 1, "{resp:?}");
    for source in ["pull_requests", "issues"] {
        assert_eq!(
            resp["sources"][source]["state"], "skipped",
            "{source}: a first-fetch failure means the source was never walked: {resp:?}"
        );
        let reason = resp["sources"][source]["reason"]
            .as_str()
            .unwrap_or_else(|| panic!("{source}: skipped carries a reason: {resp:?}"));
        assert!(
            reason.contains("list failed") && !reason.contains("simulated listing outage"),
            "{source}: the reason names the failed operation without gh stderr: {reason:?}"
        );
    }
    assert!(
        !resp.to_string().contains("simulated listing outage"),
        "listing stderr must not enter the report or durable receipt: {resp:?}"
    );
    assert_eq!(
        resp["sources"]["commits"],
        json!({"state": "completed"}),
        "commits are unaffected by a gh listing failure: {resp:?}"
    );
    assert!(
        !resp["history_exhausted"].as_bool().unwrap(),
        "skipped sources mean the pass did not cover everything requested: {resp:?}"
    );
    assert_eq!(
        resp["done"], true,
        "a first-fetch listing failure does not consume the budget-cursor signal: {resp:?}"
    );
    let warnings: Vec<&str> = resp["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        warnings.iter().any(|w| w.contains("gh pr list failed"))
            && warnings.iter().any(|w| w.contains("gh issue list failed")),
        "both walker failures surface as warnings: {warnings:?}"
    );
}

/// Source validation surfaces as a normal verb error (ssh:// rejected,
/// ADR-088 Amendment 1 security posture) rather than panicking or silently
/// no-op'ing.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_verb_rejects_ssh_source() {
    let (_rt, _token, registry) = fixture().await;
    let err = registry
        .dispatch(
            "git.digest",
            json!({"source": "ssh://git@github.com/org/repo.git"}),
        )
        .await
        .expect_err("ssh source must be rejected");
    assert!(format!("{err}").contains("SSH"), "{err}");
}
