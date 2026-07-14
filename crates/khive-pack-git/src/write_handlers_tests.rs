//! Handler-level tests for `git.commit` / `git.branch` / `git.push`
//! (ADR-108, amended by the ADR-108 Amendment), driven through the real
//! `pub(crate)` handler surface against a scratch git repo initialized
//! fresh in a `tempfile::tempdir()` for every test. Never touches
//! `~/.khive` or any production store: `KhiveRuntime` is always an
//! in-memory instance, and the git repo under test is always a throwaway
//! tempdir, never this workspace.
//!
//! The `[git_write]` policy is threaded in directly via
//! `RuntimeConfig::git_write` (see [`pack_and_token_with_policy`]) rather
//! than through `KHIVE_CONFIG`/file discovery: the write handlers read an
//! already-resolved policy from `RuntimeConfig`, they no longer re-run
//! config discovery themselves (ADR-108 review r2 finding -- a handler-level
//! reload ignored an explicit `--config` path not also exported as
//! `KHIVE_CONFIG`). This also means these tests carry zero ambient-env risk
//! for policy resolution.
//!
//! Every test still holds `cache::ENV_MUTEX` for its full body:
//! `crate::cache`'s and `crate::recovery_tests`' tests shadow the
//! process-global `PATH` to inject fake `git` binaries, which would
//! otherwise race against every `Command::new("git")` spawn here (both this
//! module's own `git_command` helper and the handler code under test
//! resolve `git` via `PATH` at spawn time).

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use serde_json::json;

use khive_runtime::engine_config::{GitWriteEntryConfig, GitWriteSectionConfig};
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig};
use khive_types::EventOutcome;

use crate::GitPack;

/// Restores one process-global environment variable when dropped. Callers
/// must hold [`crate::cache::ENV_MUTEX`] for the guard's full lifetime.
struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Builds a `[git_write]` section allowlisting `repo` for `branches`.
fn policy(repo: &Path, branches: &[&str]) -> GitWriteSectionConfig {
    GitWriteSectionConfig {
        allowed: vec![GitWriteEntryConfig {
            repo: repo.display().to_string(),
            branches: branches.iter().map(|b| b.to_string()).collect(),
        }],
    }
}

/// Constructs an in-memory `GitPack` carrying the given `[git_write]`
/// section directly in `RuntimeConfig` -- no `KHIVE_CONFIG` env var, no
/// file I/O, no discovery. An empty `GitWriteSectionConfig::default()`
/// reproduces the fail-closed "not configured" state.
async fn pack_and_token_with_policy(git_write: GitWriteSectionConfig) -> (GitPack, NamespaceToken) {
    let config = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: None,
        git_write,
        ..RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new(config).expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    (GitPack::new(rt), token)
}

async fn audit_event(
    pack: &GitPack,
    token: &NamespaceToken,
    verb: &str,
) -> khive_storage::event::Event {
    let events_store = pack.runtime().events(token).expect("events store");
    let page = events_store
        .query_events(
            khive_storage::event::EventFilter {
                verbs: vec![verb.to_string()],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");
    page.items
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{verb} audit event present"))
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

// -- git.commit ---------------------------------------------------------------

#[tokio::test]
async fn commit_with_no_paths_commits_all_tracked_changes() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;

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

/// Regression for review round 6: the handler command used by this test
/// suite must not inherit a developer's global/system Git configuration.
/// Enabling commit signing with a nonexistent signer makes that ambient
/// configuration deterministically hostile to `git commit`.
#[tokio::test]
async fn commit_ignores_hostile_ambient_global_config_in_tests() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;
    let config_dir = tempfile::tempdir().expect("global config tempdir");
    let global_config = config_dir.path().join("gitconfig");
    std::fs::write(
        &global_config,
        format!(
            "[commit]\n\tgpgSign = true\n[gpg]\n\tprogram = {}\n",
            config_dir.path().join("missing-gpg").display()
        ),
    )
    .expect("write hostile global config");
    let _global_config = EnvVarGuard::set("GIT_CONFIG_GLOBAL", &global_config);
    let _system_config = EnvVarGuard::set("GIT_CONFIG_SYSTEM", "/dev/null");

    std::fs::write(repo.path().join("a.txt"), b"changed").unwrap();

    pack.handle_commit(
        &token,
        json!({ "repo": repo.path().to_str().unwrap(), "message": "update a.txt" }),
    )
    .await
    .expect("handler test ignores hostile ambient git config");
}

#[tokio::test]
async fn commit_with_paths_scopes_to_those_paths() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;

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
async fn commit_literalizes_pathspec_magic_in_caller_path() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;

    std::fs::write(repo.path().join(":(top)"), b"literal").unwrap();
    std::fs::write(
        repo.path().join("unrelated.txt"),
        b"must remain uncommitted",
    )
    .unwrap();

    pack.handle_commit(
        &token,
        json!({
            "repo": repo.path().to_str().unwrap(),
            "message": "commit literal magic name",
            "paths": [":(top)"],
        }),
    )
    .await
    .expect("caller pathspec magic is treated as a literal filename");

    let status = git_command(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.contains("unrelated.txt"), "{status}");
    assert!(!status.contains(":(top)"), "{status}");
}

#[tokio::test]
async fn commit_accepts_special_and_unicode_literal_filename() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;
    let path = "docs/[draft]*?café.md";
    std::fs::create_dir_all(repo.path().join("docs")).unwrap();
    std::fs::write(repo.path().join(path), b"literal").unwrap();

    pack.handle_commit(
        &token,
        json!({
            "repo": repo.path().to_str().unwrap(),
            "message": "commit special literal name",
            "paths": [path],
        }),
    )
    .await
    .expect("special and unicode filename commits literally");

    let tree = git_command(repo.path())
        .args([
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ])
        .output()
        .expect("git ls-tree");
    assert!(String::from_utf8_lossy(&tree.stdout).contains(path));
}

#[tokio::test]
async fn commit_rejects_empty_message() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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
async fn commit_treats_flag_shaped_path_as_literal() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;
    std::fs::write(repo.path().join("--upload-pack=evil"), b"literal").unwrap();

    pack.handle_commit(
        &token,
        json!({
            "repo": repo.path().to_str().unwrap(),
            "message": "msg",
            "paths": ["--upload-pack=evil"],
        }),
    )
    .await
    .expect("flag-shaped filename is a literal path after --");
}

#[tokio::test]
async fn commit_invalid_path_emits_deny_audit() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;

    pack.handle_commit(
        &token,
        json!({
            "repo": repo.path().to_str().unwrap(),
            "message": "invalid path",
            "paths": ["../outside"],
        }),
    )
    .await
    .unwrap_err();

    let audit = audit_event(&pack, &token, "git.commit").await;
    assert_eq!(audit.outcome, EventOutcome::Denied);
    assert_eq!(audit.payload["decision"], "deny");
    assert_eq!(audit.payload["branch"], "main");
}

#[tokio::test]
async fn commit_rejects_non_repo_path() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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
    let (pack, token) = pack_and_token_with_policy(policy(other_repo.path(), &["main"])).await;

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
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["release-*"])).await;

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
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;

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
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["feat/*"])).await;

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
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

    let err = pack
        .handle_branch(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "name": "../../etc/passwd" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains(".."), "{err}");

    let audit = audit_event(&pack, &token, "git.branch").await;
    assert_eq!(audit.outcome, EventOutcome::Denied);
    assert_eq!(audit.payload["decision"], "deny");
    assert_eq!(audit.payload["branch"], "../../etc/passwd");
}

#[tokio::test]
async fn branch_denied_when_no_policy_configured() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["feat/*"])).await;

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
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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

    let audit = audit_event(&pack, &token, "git.push").await;
    assert_eq!(audit.outcome, EventOutcome::Denied);
    assert_eq!(audit.payload["decision"], "deny");
    assert_eq!(audit.payload["branch"], "main");
}

#[tokio::test]
async fn push_allows_force_false() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;

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
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["*"])).await;

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
    let (pack, token) = pack_and_token_with_policy(GitWriteSectionConfig::default()).await;

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
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["release-*"])).await;

    let err = pack
        .handle_push(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "branch": "main" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("branch"), "{err}");
}

// -- symlink TOCTOU (ADR-108 review r2 High finding) -----------------------

/// A symlink pointing at the allowlisted repo when `handle_commit` starts,
/// retargeted to a decoy (unallowlisted) repo mid-flight, must not let the
/// commit land in the decoy: the handler must operate only on the canonical
/// path resolved at check time, never re-traverse the caller-supplied
/// symlink for the actual git invocations.
/// `handle_commit` invoked through a symlink must resolve and report the
/// canonical repo path, and land the commit in the real repo -- not the
/// symlink's own (mutable) path string. This is the property that closes
/// the TOCTOU: every git invocation after the policy check uses the
/// canonical path returned by `enforce_write_policy`, so a later symlink
/// retarget (see `write_policy::tests::
/// canonical_path_returned_at_check_time_is_immune_to_later_retarget` for
/// the direct check-level regression) cannot redirect an in-flight write.
#[cfg(unix)]
#[tokio::test]
async fn commit_via_symlink_resolves_and_lands_in_canonical_repo() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (real_repo, _real_remote) = init_repo_with_remote();
    let parent = tempfile::tempdir().expect("parent tempdir");
    let link = parent.path().join("repo-link");
    std::os::unix::fs::symlink(real_repo.path(), &link).unwrap();

    let (pack, token) = pack_and_token_with_policy(policy(real_repo.path(), &["main"])).await;

    std::fs::write(real_repo.path().join("a.txt"), b"changed-via-link").unwrap();

    let result = pack
        .handle_commit(
            &token,
            json!({ "repo": link.to_str().unwrap(), "message": "via symlink" }),
        )
        .await
        .expect("commit succeeds against the canonical repo");

    let reported_repo = result.get("repo").and_then(|v| v.as_str()).unwrap();
    assert_eq!(
        Path::new(reported_repo),
        std::fs::canonicalize(real_repo.path()).unwrap(),
        "handler must report the canonical repo path, not the symlink path"
    );

    let real_log = git_command(real_repo.path())
        .args(["log", "-1", "--pretty=%s"])
        .output()
        .expect("git log on real repo");
    assert_eq!(
        String::from_utf8_lossy(&real_log.stdout).trim(),
        "via symlink",
        "the commit must have landed in the real repo"
    );
}

// -- audit completeness (ADR-108 review r2 High finding) -------------------

/// `handle_commit` discovers the checked-out branch internally -- the
/// resulting audit event must carry it, not `None` (the pre-fix bug).
#[tokio::test]
async fn commit_audit_captures_resolved_branch_and_sha() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;

    std::fs::write(repo.path().join("a.txt"), b"changed").unwrap();
    let result = pack
        .handle_commit(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "message": "audit check" }),
        )
        .await
        .expect("commit succeeds");
    let sha = result
        .get("sha")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let events_store = pack.runtime().events(&token).expect("events store");
    let page = events_store
        .query_events(
            khive_storage::event::EventFilter {
                verbs: vec!["git.commit".to_string()],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");
    let audit = page.items.first().expect("git.commit audit event present");
    assert_eq!(
        audit.payload.get("branch").and_then(|v| v.as_str()),
        Some("main")
    );
    assert_eq!(
        audit.payload.get("decision").and_then(|v| v.as_str()),
        Some("allow")
    );
    assert_eq!(
        audit.payload.get("sha").and_then(|v| v.as_str()),
        Some(sha.as_str())
    );
}

/// A handler-allowlist denial must itself emit a `deny` decision audit
/// event, not leave the trail entirely to the dispatch-level Gate audit
/// (which records only its own Allow/Deny, paired with an errored outcome).
#[tokio::test]
async fn commit_denial_emits_deny_decision_audit() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["release-*"])).await;

    std::fs::write(repo.path().join("a.txt"), b"changed").unwrap();
    let err = pack
        .handle_commit(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "message": "denied" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("branch"));

    let events_store = pack.runtime().events(&token).expect("events store");
    let page = events_store
        .query_events(
            khive_storage::event::EventFilter {
                verbs: vec!["git.commit".to_string()],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");
    let audit = page
        .items
        .first()
        .expect("git.commit audit event present even on denial");
    assert_eq!(
        audit.payload.get("decision").and_then(|v| v.as_str()),
        Some("deny")
    );
    assert_eq!(
        audit.payload.get("branch").and_then(|v| v.as_str()),
        Some("main")
    );
}

#[tokio::test]
async fn detached_head_commit_failure_emits_error_audit() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token_with_policy(policy(repo.path(), &["main"])).await;
    run(repo.path(), &["checkout", "-q", "--detach", "HEAD"]);
    std::fs::write(repo.path().join("a.txt"), b"detached change").unwrap();

    pack.handle_commit(
        &token,
        json!({ "repo": repo.path().to_str().unwrap(), "message": "detached" }),
    )
    .await
    .unwrap_err();

    let audit = audit_event(&pack, &token, "git.commit").await;
    assert_eq!(audit.outcome, EventOutcome::Error);
    assert_eq!(audit.payload["decision"], "deny");
    assert!(audit.payload.get("branch").is_none());
}
