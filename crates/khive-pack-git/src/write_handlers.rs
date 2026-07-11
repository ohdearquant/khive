//! `git.commit` / `git.branch` / `git.push` verb handlers (ADR-108, amended
//! by the ADR-108 Amendment).
//!
//! Thin, 1:1 wrappers over system `git`, shelled via
//! `std::process::Command::args` (never a shell string) with every
//! caller-supplied value validated and assembled into an argv vector by
//! `crate::write_argv` before it reaches the process boundary. `repo` is an
//! ordinary verb argument like every other khive verb — the Gate (ADR-018)
//! still decides allow/deny before any of these functions run, but it is no
//! longer the only enforcement point: `enforce_write_policy` below is a
//! handler-level precondition, fail-closed independent of Gate
//! configuration, resolved by `crate::write_policy` against the operator's
//! `[git_write]` allowlist (ADR-108 Amendment).
//!
//! Every successful write additionally appends a `git.write`-shaped `Event`
//! (kind `Audit`, ADR-108 rule 2) carrying `repo`/`branch`/`sha` beyond what
//! the dispatch-level gate-check audit already records for every verb.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::event::Event;
use khive_types::{EventKind, EventOutcome, SubstrateKind};

use crate::write_argv::{
    build_add_argv, build_branch_argv, build_commit_argv, build_push_argv, reject_force,
    validate_repo_path, GitArgError,
};
use crate::write_policy::{load_git_write_policy, GitWritePolicyError};
use crate::GitPack;

fn to_invalid_input(e: GitArgError) -> RuntimeError {
    RuntimeError::InvalidInput(e.to_string())
}

fn to_policy_denied(e: GitWritePolicyError) -> RuntimeError {
    RuntimeError::InvalidInput(e.to_string())
}

fn parse_repo_param(params: &Value) -> Result<PathBuf, RuntimeError> {
    let raw = params
        .get("repo")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::InvalidInput("repo is required".into()))?;
    Ok(PathBuf::from(raw))
}

fn parse_paths_param(params: &Value) -> Result<Vec<String>, RuntimeError> {
    match params.get("paths") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    RuntimeError::InvalidInput("paths entries must be strings".into())
                })
            })
            .collect(),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "paths must be an array of strings, got {other:?}"
        ))),
    }
}

/// Parses the `force` argument. `true` is caught by [`reject_force`]
/// downstream; any non-boolean value (a string, number, array, object) is
/// rejected loudly here rather than silently coerced to `false` — an
/// explicit but malformed `force` argument must never be interpreted as "no
/// force requested" (ADR-108: "an explicit force arg is rejected loudly").
fn parse_force_param(params: &Value) -> Result<Option<bool>, RuntimeError> {
    match params.get("force") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "force must be a boolean, got {other:?}; force-push is never permitted through this verb"
        ))),
    }
}

/// Runs `git -C <repo> <argv...>`, argv-only (no shell), returning stdout on
/// success or a `RuntimeError` carrying git's stderr on failure.
///
/// Every invocation disables repo-configured hooks via
/// `-c core.hooksPath=/dev/null` (ADR-108 Amendment), mirroring
/// `crate::cache`'s hardened clone/fetch invocations: this function runs in
/// the daemon's own credential context, so a hook script committed into an
/// allowlisted repo (e.g. `.git/hooks/pre-commit`) must never get a chance
/// to execute as a side effect of a khive-mediated write. `GIT_CONFIG_GLOBAL`
/// / `GIT_CONFIG_SYSTEM` are deliberately left untouched here, unlike the
/// test harness's hermetic `git_command` helper: these are real,
/// operator-owned repos, and a commit/push needs the operator's actual
/// author identity and credential helpers (SSH keys, `credential.helper`)
/// configured in global/system git config to work at all — neutralizing
/// that config would break the legitimate write path along with the attack
/// surface it does not itself pose (hooks are the RCE risk; identity/
/// credential config is not).
fn run_git(repo: &Path, argv: &[String]) -> Result<String, RuntimeError> {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-C")
        .arg(repo)
        .args(argv)
        .output()
        .map_err(|e| RuntimeError::InvalidInput(format!("spawning git {argv:?}: {e}")))?;
    if !output.status.success() {
        return Err(RuntimeError::InvalidInput(format!(
            "git {argv:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads the repo's currently checked-out branch via `git symbolic-ref`.
/// `git.commit` has no explicit branch argument — the branch it actually
/// writes to is whatever is checked out — so this is what
/// `enforce_write_policy` checks the allowlist against for that verb.
/// Errors (e.g. detached HEAD) surface as an ordinary handler error.
fn current_branch(repo: &Path) -> Result<String, RuntimeError> {
    let out = run_git(
        repo,
        &[
            "symbolic-ref".to_string(),
            "--short".to_string(),
            "HEAD".to_string(),
        ],
    )?;
    Ok(out.trim().to_string())
}

impl GitPack {
    /// Handler-level fail-closed precondition (ADR-108 Amendment), enforced
    /// before any of the three write verbs mutate a repository: the write
    /// verbs are unavailable unless a `[git_write]` policy is configured,
    /// and even then `repo`/`branch` must resolve to an allowlisted entry.
    /// This runs in addition to, not instead of, the Gate (ADR-018) —
    /// deliberately not dependent on Gate configuration, the same
    /// enforcement class as [`crate::write_argv::reject_force`]'s
    /// unconditional force-push denial.
    fn enforce_write_policy(&self, repo: &Path, branch: &str) -> Result<(), RuntimeError> {
        let db_path = self.runtime().config().db_path.as_deref();
        let policy = load_git_write_policy(db_path)
            .map_err(|e| RuntimeError::InvalidInput(format!("loading [git_write] policy: {e}")))?;
        policy.check(repo, branch).map_err(to_policy_denied)
    }

    pub(crate) async fn handle_commit(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let repo = parse_repo_param(&params)?;
        validate_repo_path(&repo).map_err(to_invalid_input)?;

        let message = params
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("git.commit requires message".into()))?;
        let paths = parse_paths_param(&params)?;
        let author = params.get("author").and_then(Value::as_str);

        let add_argv = if paths.is_empty() {
            None
        } else {
            Some(build_add_argv(&paths).map_err(to_invalid_input)?)
        };
        let commit_argv = build_commit_argv(message, &paths, author).map_err(to_invalid_input)?;

        let branch = current_branch(&repo)?;
        self.enforce_write_policy(&repo, &branch)?;

        if let Some(add_argv) = add_argv {
            run_git(&repo, &add_argv)?;
        }
        run_git(&repo, &commit_argv)?;

        let sha = run_git(&repo, &["rev-parse".to_string(), "HEAD".to_string()])?
            .trim()
            .to_string();

        self.emit_write_audit(token, "git.commit", &repo, None, Some(&sha))
            .await;

        Ok(json!({
            "repo": repo.display().to_string(),
            "sha": sha,
        }))
    }

    pub(crate) async fn handle_branch(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let repo = parse_repo_param(&params)?;
        validate_repo_path(&repo).map_err(to_invalid_input)?;

        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("git.branch requires name".into()))?;
        let from = params.get("from").and_then(Value::as_str);

        let argv = build_branch_argv(name, from).map_err(to_invalid_input)?;

        self.enforce_write_policy(&repo, name)?;

        run_git(&repo, &argv)?;

        self.emit_write_audit(token, "git.branch", &repo, Some(name), None)
            .await;

        Ok(json!({
            "repo": repo.display().to_string(),
            "name": name,
            "from": from,
        }))
    }

    pub(crate) async fn handle_push(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let repo = parse_repo_param(&params)?;
        validate_repo_path(&repo).map_err(to_invalid_input)?;

        let branch = params
            .get("branch")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("git.push requires branch".into()))?;
        let remote = params
            .get("remote")
            .and_then(Value::as_str)
            .unwrap_or("origin");

        let force = parse_force_param(&params)?;
        reject_force(force).map_err(to_invalid_input)?;

        let argv = build_push_argv(remote, branch).map_err(to_invalid_input)?;

        self.enforce_write_policy(&repo, branch)?;

        run_git(&repo, &argv)?;

        self.emit_write_audit(token, "git.push", &repo, Some(branch), None)
            .await;

        Ok(json!({
            "repo": repo.display().to_string(),
            "remote": remote,
            "branch": branch,
        }))
    }

    /// Appends a supplementary audit `Event` (ADR-108 rule 2) carrying
    /// `repo`/`branch`/`sha` — fields the dispatch-level gate-check audit
    /// (`khive-runtime::pack::VerbRegistry::dispatch_with_identity`, fired
    /// automatically for every verb) does not itself carry. Best-effort: a
    /// store failure is logged and swallowed, exactly like every other audit
    /// write in this codebase (ADR-018 "audit storage failures don't
    /// propagate") — it must never fail a write that git itself completed.
    async fn emit_write_audit(
        &self,
        token: &NamespaceToken,
        verb: &str,
        repo: &Path,
        branch: Option<&str>,
        sha: Option<&str>,
    ) {
        let Ok(store) = self.runtime().events(token) else {
            return;
        };
        let mut payload = json!({
            "repo": repo.display().to_string(),
            "decision": "allow",
        });
        if let Some(b) = branch {
            payload["branch"] = json!(b);
        }
        if let Some(s) = sha {
            payload["sha"] = json!(s);
        }
        let event = Event::new(
            token.namespace().as_str(),
            verb,
            EventKind::Audit,
            SubstrateKind::Event,
            token.actor().id.clone(),
        )
        .with_outcome(EventOutcome::Success)
        .with_payload(payload);
        if let Err(e) = store.append_event(event).await {
            tracing::warn!(
                verb,
                error = %e,
                "git write audit event store write failed (non-fatal)"
            );
        }
    }
}
