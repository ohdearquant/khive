//! Handler-level tests for `git.commit` / `git.branch` / `git.push`
//! (ADR-108), driven through the real `pub(crate)` handler surface against a
//! scratch git repo initialized fresh in a `tempfile::tempdir()` for every
//! test. Never touches `~/.khive` or any production store: `KhiveRuntime`
//! is always an in-memory instance (`KhiveRuntime::memory()`), and the git
//! repo under test is always a throwaway tempdir, never this workspace.

use std::path::Path;
use std::process::Command;

use serde_json::json;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken};

use crate::GitPack;

fn run(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
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
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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

    let log = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["log", "-1", "--pretty=%s"])
        .output()
        .expect("git log");
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "update a.txt");
}

#[tokio::test]
async fn commit_with_paths_scopes_to_those_paths() {
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
    let status = Command::new("git")
        .arg("-C")
        .arg(repo.path())
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
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
    let dir = tempfile::tempdir().expect("tempdir");
    let (pack, token) = pack_and_token().await;

    let err = pack
        .handle_commit(
            &token,
            json!({ "repo": dir.path().to_str().unwrap(), "message": "msg" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains(".git"), "{err}");
}

// -- git.branch -----------------------------------------------------------

#[tokio::test]
async fn branch_creates_from_head_by_default() {
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

    let result = pack
        .handle_branch(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "name": "feat/x" }),
        )
        .await
        .expect("branch succeeds");
    assert_eq!(result.get("name").and_then(|v| v.as_str()), Some("feat/x"));

    let branches = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["branch", "--list", "feat/x"])
        .output()
        .expect("git branch --list");
    assert!(String::from_utf8_lossy(&branches.stdout).contains("feat/x"));
}

#[tokio::test]
async fn branch_rejects_injection_shaped_name() {
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

    let err = pack
        .handle_branch(
            &token,
            json!({ "repo": repo.path().to_str().unwrap(), "name": "../../etc/passwd" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains(".."), "{err}");
}

// -- git.push -----------------------------------------------------------

#[tokio::test]
async fn push_sends_branch_to_remote() {
    let (repo, remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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

    let branches = Command::new("git")
        .arg("-C")
        .arg(remote.path())
        .args(["branch", "--list", "feat/pushme"])
        .output()
        .expect("git branch --list on remote");
    assert!(String::from_utf8_lossy(&branches.stdout).contains("feat/pushme"));
}

#[tokio::test]
async fn push_rejects_explicit_force_true() {
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
    let (repo, _remote) = init_repo_with_remote();
    let (pack, token) = pack_and_token().await;

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
