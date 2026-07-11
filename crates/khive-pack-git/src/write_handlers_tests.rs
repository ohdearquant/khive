//! Handler-level tests for `git.commit` / `git.branch` / `git.push`
//! (ADR-108, amended by the ADR-108 Amendment), driven through the real
//! `pub(crate)` handler surface against a scratch git repo initialized
//! fresh in a `tempfile::tempdir()` for every test. Never touches
//! `~/.khive` or any production store: `KhiveRuntime` is always an
//! in-memory instance (`KhiveRuntime::memory()`), and the git repo under
//! test is always a throwaway tempdir, never this workspace.
//!
//! Every test holds `cache::ENV_MUTEX` for its full body: `crate::cache`'s
//! and `crate::recovery_tests`' tests shadow the process-global `PATH` (and
//! other env vars) to inject fake `git` binaries, which would otherwise race
//! against every `Command::new("git")` spawn here (both this module's own
//! `git_command` helper and the handler code under test resolve `git` via
//! `PATH` at spawn time). The same guard also covers `KHIVE_CONFIG`, which
//! every test in this module sets explicitly (see [`set_policy`] /
//! [`set_no_policy`]): the write handlers now load a `[git_write]` policy
//! from the standard khive config discovery chain, and an unset
//! `KHIVE_CONFIG` would fall through to a real `~/.khive/config.toml` on
//! the machine running the tests -- exactly the ambient-state leak this
//! module's doc comment above promises never happens. Setting `KHIVE_CONFIG`
//! to an explicit tier-1 path (present or deliberately absent) always short
//! circuits that fallback.

use std::path::Path;
use std::process::Command;

use serde_json::json;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken};

use crate::GitPack;

/// Writes a `[git_write]` policy TOML allowlisting `repo` for `branches`
/// and points `KHIVE_CONFIG` at it (tier-1 override, wins over every other
/// discovery tier). Caller must hold `cache::ENV_MUTEX` for the whole test.
fn set_policy(config_dir: &Path, repo: &Path, branches: &[&str]) {
    let branches_toml = branches
        .iter()
        .map(|b| format!("{b:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let content = format!(
        "[[git_write.allowed]]\nrepo = {:?}\nbranches = [{branches_toml}]\n",
        repo.display().to_string()
    );
    let path = config_dir.join("git-write-config.toml");
    std::fs::write(&path, content).unwrap();
    std::env::set_var("KHIVE_CONFIG", &path);
}

/// Points `KHIVE_CONFIG` at a path that does not exist -- `KhiveConfig::load`
/// treats a missing explicit path as "no config", which resolves to the
/// empty (fail-closed) policy without ever touching tiers 2-4.
fn set_no_policy(config_dir: &Path) {
    std::env::set_var("KHIVE_CONFIG", config_dir.join("does-not-exist.toml"));
}

/// Builds a `git` invocation hardened against ambient host state: a global
/// `core.hooksPath`, `init.templateDir`, or system/global config on the
/// machine running the tests must never be able to run a hook or inject
/// config into a scratch repo under test. Mirrors `crates/khive-pack-git/
/// src/cache.rs`'s hardened invocations, applied to every git call this test
/// module makes (not just the `run` helper).
fn git_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            dir.join(".khive-test-no-hooks").display()
        ))
        .arg("-C")
        .arg(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TEMPLATE_DIR", "")
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

fn run(dir: &Path, args: &[&str]) {
    let out = git_command(dir).args(args).output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Initializes a throwaway repo with one commit on `main`, a configured
/// identity, and a bare "remote" (also a tempdir) wired as `origin` so
/// `git.push` has somewhere real to push to. Returns `(repo_tempdir,
/// remote_tempdir)` — both must stay alive for the caller's test body.
fn init_repo_with_remote() -> (tempfile::TempDir, tempfile::TempDir) {
    let remote_dir = tempfile::tempdir().expect("remote tempdir");
    run(remote_dir.path(), &["init", "-q", "--bare", "-b", "main"]);

    let repo_dir = tempfile::tempdir().expect("repo tempdir");
    run(repo_dir.path(), &["init", "-q", "-b", "main"]);
    run(
        repo_dir.path(),
        &["config", "user.email", "test@example.com"],
    );
    run(repo_dir.path(), &["config", "user.name", "Test User"]);
    std::fs::write(repo_dir.path().join("a.txt"), b"hello").unwrap();
    run(repo_dir.path(), &["add", "a.txt"]);
    run(repo_dir.path(), &["commit", "-q", "-m", "initial"]);
    run(
        repo_dir.path(),
        &[
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().unwrap(),
        ],
    );
    run(repo_dir.path(), &["push", "-q", "origin", "main"]);

    (repo_dir, remote_dir)
}

async fn pack_and_token() -> (GitPack, NamespaceToken) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    (GitPack::new(rt), token)
}

// -- git.commit ---------------------------------------------------------------

#[tokio::test]
async fn commit_with_no_paths_commits_all_tracked_changes() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_policy(config_dir.path(), repo.path(), &["main"]);

    std::fs::write(repo.path().join("a.txt"), b"changed").unwrap();

    let result = pack
        .handle_commit(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "message": "update a.txt" }),
        )
        .await
        .expect("commit succeeds");

    let sha = result
        .get("sha")
        .and_then(|v| v.as_str())
        .expect("sha present");
    assert_eq!(sha.len(), 40, "sha must be a full 40-char hex commit id");

    let log = git_command(repo.path())
        .args(["log", "-1", "--pretty=%s"])
        .output()
        .expect("git log");
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "update a.txt");
}

#[tokio::test]
async fn commit_with_paths_scopes_to_those_paths() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_policy(config_dir.path(), repo.path(), &["main"]);

    std::fs::write(repo.path().join("a.txt"), b"changed-a").unwrap();
    std::fs::write(repo.path().join("b.txt"), b"new-b").unwrap();

    let result = pack
        .handle_commit(
            &token,
            json!({
                "repo": repo.path().to_str().unwrap(),
                "message": "add b only",
                "paths": ["b.txt"],
            }),
        )
        .await
        .expect("commit succeeds");
    assert!(result.get("sha").is_some());

    // a.txt was modified but not in `paths` -- must still show as dirty.
    let status = git_command(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    let status_str = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_str.contains("a.txt"),
        "a.txt must remain uncommitted/dirty: {status_str}"
    );
    assert!(
        !status_str.contains("b.txt"),
        "b.txt must be committed and clean: {status_str}"
    );
}

#[tokio::test]
async fn commit_rejects_empty_message() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_commit(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "message": "" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("message"), "{err}");
}

#[tokio::test]
async fn commit_rejects_injection_shaped_path() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_commit(
            &token,
            json!({
                "repo": repo.path().to_str().unwrap(),
                "message": "msg",
                "paths": ["--upload-pack=evil"],
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("start with"), "{err}");
}

#[tokio::test]
async fn commit_rejects_non_repo_path() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_commit(
            &token,
            json!({ "repo": dir.path().to_str().unwrap(), "message": "msg" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains(".git"), "{err}");
}

#[tokio::test]
async fn commit_denied_when_no_policy_configured() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    std::fs::write(repo.path().join("a.txt"), b"changed").unwrap();

    let err = pack
        .handle_commit(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "message": "update a.txt" }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("git-write policy is configured"),
        "{err}"
    );
}

#[tokio::test]
async fn commit_denied_for_non_allowlisted_repo() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (other_repo, _other_remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    // Allowlists a different repo than the one being committed to.
    set_policy(config_dir.path(), other_repo.path(), &["main"]);

    std::fs::write(repo.path().join("a.txt"), b"changed").unwrap();

    let err = pack
        .handle_commit(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "message": "update a.txt" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("allowlist"), "{err}");
}

#[tokio::test]
async fn commit_denied_for_branch_outside_patterns() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    // Allowlists the repo, but only for a branch pattern that does not
    // match "main", the branch actually checked out by `init_repo_with_remote`.
    set_policy(config_dir.path(), repo.path(), &["release-*"]);

    std::fs::write(repo.path().join("a.txt"), b"changed").unwrap();

    let err = pack
        .handle_commit(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "message": "update a.txt" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("branch"), "{err}");
}

/// Regression for the H3 review finding: `run_git` must disable
/// repo-configured hooks (`core.hooksPath=/dev/null`) so a hook script
/// committed into an allowlisted repo cannot execute as a side effect of a
/// khive-mediated write, in the daemon's own credential context.
#[tokio::test]
async fn commit_does_not_execute_repo_configured_hooks() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_policy(config_dir.path(), repo.path(), &["main"]);

    let sentinel = repo.path().join("hook-ran.sentinel");
    let hooks_dir = repo.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join("pre-commit");
    std::fs::write(
        &hook_path,
        format!("#!/bin/sh\ntouch {:?}\n", sentinel.display().to_string()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    std::fs::write(repo.path().join("a.txt"), b"changed").unwrap();

    pack.handle_commit(
        &token,
        json!({ "repo": repo.path().to_str().unwrap(), "message": "update a.txt" }),
    )
    .await
    .expect("commit succeeds");

    assert!(
        !sentinel.exists(),
        "pre-commit hook must not have executed during a khive-mediated commit"
    );
}

// -- git.branch -----------------------------------------------------------

#[tokio::test]
async fn branch_creates_from_head_by_default() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_policy(config_dir.path(), repo.path(), &["feat/*"]);

    let result = pack
        .handle_branch(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "name": "feat/x" }),
        )
        .await
        .expect("branch succeeds");
    assert_eq!(result.get("name").and_then(|v| v.as_str()), Some("feat/x"));

    let branches = git_command(repo.path())
        .args(["branch", "--list", "feat/x"])
        .output()
        .expect("git branch --list");
    assert!(String::from_utf8_lossy(&branches.stdout).contains("feat/x"));
}

#[tokio::test]
async fn branch_rejects_injection_shaped_name() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_branch(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "name": "--upload-pack=evil" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("start with"), "{err}");
}

#[tokio::test]
async fn branch_rejects_path_traversal_name() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_branch(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "name": "../../etc/passwd" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains(".."), "{err}");
}

#[tokio::test]
async fn branch_denied_when_no_policy_configured() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_branch(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "name": "feat/x" }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("git-write policy is configured"),
        "{err}"
    );
}

// -- git.push -----------------------------------------------------------

#[tokio::test]
async fn push_sends_branch_to_remote() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_policy(config_dir.path(), repo.path(), &["feat/*"]);

    run(repo.path(), &["checkout", "-q", "-b", "feat/pushme"]);
    std::fs::write(repo.path().join("c.txt"), b"c").unwrap();
    run(repo.path(), &["add", "c.txt"]);
    run(repo.path(), &["commit", "-q", "-m", "add c"]);

    let result = pack
        .handle_push(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "branch": "feat/pushme" }),
        )
        .await
        .expect("push succeeds");
    assert_eq!(
        result.get("remote").and_then(|v| v.as_str()),
        Some("origin")
    );

    let branches = git_command(remote.path())
        .args(["branch", "--list", "feat/pushme"])
        .output()
        .expect("git branch --list on remote");
    assert!(String::from_utf8_lossy(&branches.stdout).contains("feat/pushme"));
}

#[tokio::test]
async fn push_rejects_explicit_force_true() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_push(
            &token,
            json!({
                "repo": repo.path().to_str().unwrap(),
                "branch": "main",
                "force": true,
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("force-push"), "{err}");
}

#[tokio::test]
async fn push_allows_force_false() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_policy(config_dir.path(), repo.path(), &["main"]);

    let result = pack
        .handle_push(
            &token,
            json!({
                "repo": repo.path().to_str().unwrap(),
                "branch": "main",
                "force": false,
            }),
        )
        .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn push_rejects_non_boolean_force() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_push(
            &token,
            json!({
                "repo": repo.path().to_str().unwrap(),
                "branch": "main",
                "force": "true",
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("boolean"), "{err}");
}

#[tokio::test]
async fn push_rejects_injection_shaped_remote() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_push(
            &token,
            json!({
                "repo": repo.path().to_str().unwrap(),
                "branch": "main",
                "remote": "--upload-pack=evil",
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("start with"), "{err}");
}

#[tokio::test]
async fn push_rejects_nonexistent_branch() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_policy(config_dir.path(), repo.path(), &["*"]);

    let err = pack
        .handle_push(
            &token,
            json!({
                "repo": repo.path().to_str().unwrap(),
                "branch": "does-not-exist",
            }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("git"), "{err}");
}

#[tokio::test]
async fn push_denied_when_no_policy_configured() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_no_policy(config_dir.path());

    let err = pack
        .handle_push(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "branch": "main" }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("git-write policy is configured"),
        "{err}"
    );
}

#[tokio::test]
async fn push_denied_for_branch_outside_patterns() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;
    let config_dir = tempfile::tempdir().expect("config tempdir");
    set_policy(config_dir.path(), repo.path(), &["release-*"]);

    let err = pack
        .handle_push(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "branch": "main" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("branch"), "{err}");
}
