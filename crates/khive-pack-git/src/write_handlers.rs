//! `git.commit` / `git.branch` / `git.push` verb handlers (ADR-108).
//!
//! Thin, 1:1 wrappers over system `git`, shelled via
//! `std::process::Command::args` (never a shell string) with every
//! caller-supplied value validated and assembled into an argv vector by
//! `crate::write_argv` before it reaches the process boundary. `repo` is an
//! ordinary verb argument, exactly like every other khive verb — the Gate
//! (ADR-018) decides allow/deny before any of these functions run; nothing
//! here re-implements authorization.
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
use crate::GitPack;

fn to_invalid_input(e: GitArgError) -> RuntimeError {
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
fn run_git(repo: &Path, argv: &[String]) -> Result<String, RuntimeError> {
    let output = Command::new("git")
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

impl GitPack {
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

        if !paths.is_empty() {
            let add_argv = build_add_argv(&paths).map_err(to_invalid_input)?;
            run_git(&repo, &add_argv)?;
        }

        let commit_argv = build_commit_argv(message, &paths, author).map_err(to_invalid_input)?;
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
