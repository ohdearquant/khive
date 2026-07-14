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
//! `enforce_write_policy` returns the **canonical** repo path on success, and
//! every git invocation for that call uses it from that point on — never the
//! raw caller-supplied `repo` (ADR-108 review r2 High finding: reusing the
//! raw path after only canonicalizing it for the comparison is a symlink
//! TOCTOU). The check and the mutation are additionally serialized per-repo
//! via the private `repo_write_lock` helper so a concurrent khive-mediated write to the same
//! repo cannot interleave between the policy check and the git command it
//! guards.
//!
//! Every write attempt — allowed or denied, whether git itself then
//! succeeds or fails — appends exactly one `git.write`-shaped `Event` (kind
//! `Audit`, ADR-108 rule 2) via `emit_write_audit`, carrying
//! `repo`/`branch`/`decision` and, on success, `sha`. This is in addition
//! to, not a replacement for, the dispatch-level gate-check audit that
//! fires for every verb.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::event::Event;
use khive_types::{EventKind, EventOutcome, SubstrateKind};

use crate::write_argv::{
    build_add_argv, build_branch_argv, build_commit_argv, build_push_argv, reject_force,
    validate_repo_path, GitArgError,
};
use crate::write_policy::{GitWritePolicy, GitWritePolicyError};
use crate::GitPack;

fn to_invalid_input(e: GitArgError) -> RuntimeError {
    RuntimeError::InvalidInput(e.to_string())
}

fn to_policy_denied(e: GitWritePolicyError) -> RuntimeError {
    RuntimeError::InvalidInput(e.to_string())
}

/// Process-wide per-repo advisory lock registry, keyed by canonical repo
/// path. Guards the window between `enforce_write_policy`'s check and the
/// mutating git command it authorizes, so two concurrent khive-mediated
/// writes to the same repo (e.g. two overlapping `git.commit` calls) cannot
/// interleave a check from one call with the mutation of another (ADR-108
/// review r2 High finding).
static REPO_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();

fn repo_write_lock(repo: &Path) -> Arc<AsyncMutex<()>> {
    let key = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let registry = REPO_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
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

struct WritePreflightError {
    error: RuntimeError,
    branch: Option<String>,
    outcome: EventOutcome,
}

impl WritePreflightError {
    fn denied(error: RuntimeError, branch: Option<&str>) -> Self {
        Self {
            error,
            branch: branch.map(str::to_string),
            outcome: EventOutcome::Denied,
        }
    }

    fn runtime(error: RuntimeError) -> Self {
        Self {
            error,
            branch: None,
            outcome: EventOutcome::Error,
        }
    }
}

struct CommitPreflight {
    branch: String,
    add_argv: Option<Vec<String>>,
    commit_argv: Vec<String>,
}

fn prepare_commit(repo: &Path, params: &Value) -> Result<CommitPreflight, WritePreflightError> {
    validate_repo_path(repo)
        .map_err(to_invalid_input)
        .map_err(|e| WritePreflightError::denied(e, None))?;
    let branch = current_branch(repo).map_err(WritePreflightError::runtime)?;
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::InvalidInput("git.commit requires message".into()))
        .map_err(|e| WritePreflightError::denied(e, Some(&branch)))?;
    let paths =
        parse_paths_param(params).map_err(|e| WritePreflightError::denied(e, Some(&branch)))?;
    let author = params.get("author").and_then(Value::as_str);
    let add_argv = if paths.is_empty() {
        None
    } else {
        Some(
            build_add_argv(&paths)
                .map_err(to_invalid_input)
                .map_err(|e| WritePreflightError::denied(e, Some(&branch)))?,
        )
    };
    let commit_argv = build_commit_argv(message, &paths, author)
        .map_err(to_invalid_input)
        .map_err(|e| WritePreflightError::denied(e, Some(&branch)))?;
    Ok(CommitPreflight {
        branch,
        add_argv,
        commit_argv,
    })
}

struct BranchPreflight {
    name: String,
    from: Option<String>,
    argv: Vec<String>,
}

fn prepare_branch(repo: &Path, params: &Value) -> Result<BranchPreflight, WritePreflightError> {
    validate_repo_path(repo)
        .map_err(to_invalid_input)
        .map_err(|e| WritePreflightError::denied(e, None))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::InvalidInput("git.branch requires name".into()))
        .map_err(|e| WritePreflightError::denied(e, None))?;
    let from = params.get("from").and_then(Value::as_str);
    let argv = build_branch_argv(name, from)
        .map_err(to_invalid_input)
        .map_err(|e| WritePreflightError::denied(e, Some(name)))?;
    Ok(BranchPreflight {
        name: name.to_string(),
        from: from.map(str::to_string),
        argv,
    })
}

struct PushPreflight {
    branch: String,
    remote: String,
    argv: Vec<String>,
}

fn prepare_push(repo: &Path, params: &Value) -> Result<PushPreflight, WritePreflightError> {
    validate_repo_path(repo)
        .map_err(to_invalid_input)
        .map_err(|e| WritePreflightError::denied(e, None))?;
    let branch = params
        .get("branch")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::InvalidInput("git.push requires branch".into()))
        .map_err(|e| WritePreflightError::denied(e, None))?;
    let remote = params
        .get("remote")
        .and_then(Value::as_str)
        .unwrap_or("origin");
    let force =
        parse_force_param(params).map_err(|e| WritePreflightError::denied(e, Some(branch)))?;
    reject_force(force)
        .map_err(to_invalid_input)
        .map_err(|e| WritePreflightError::denied(e, Some(branch)))?;
    let argv = build_push_argv(remote, branch)
        .map_err(to_invalid_input)
        .map_err(|e| WritePreflightError::denied(e, Some(branch)))?;
    Ok(PushPreflight {
        branch: branch.to_string(),
        remote: remote.to_string(),
        argv,
    })
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
    ///
    /// Reads the already-resolved `[git_write]` policy from
    /// `RuntimeConfig::git_write` (threaded in at boot from `--config` /
    /// `KHIVE_CONFIG` discovery, see `crate::write_policy`'s module doc) —
    /// never re-runs config discovery itself, so an explicit `--config` path
    /// reaches this check even when not also exported as `KHIVE_CONFIG`.
    ///
    /// On success, returns the **canonical** repo path — callers must use it
    /// for every subsequent git invocation, never the raw `repo` argument
    /// (see the module doc's TOCTOU note).
    fn enforce_write_policy(&self, repo: &Path, branch: &str) -> Result<PathBuf, RuntimeError> {
        let policy = GitWritePolicy::from_config(&self.runtime().config().git_write);
        policy.check(repo, branch).map_err(to_policy_denied)
    }

    async fn audit_early_failure(
        &self,
        token: &NamespaceToken,
        verb: &str,
        repo: &Path,
        branch: Option<&str>,
        outcome: EventOutcome,
        error: RuntimeError,
    ) -> RuntimeError {
        self.emit_write_audit(token, verb, repo, branch, "deny", outcome, None)
            .await;
        error
    }

    pub(crate) async fn handle_commit(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let repo = parse_repo_param(&params)?;
        let lock = repo_write_lock(&repo);
        let _guard = lock.lock().await;
        let CommitPreflight {
            branch,
            add_argv,
            commit_argv,
        } = match prepare_commit(&repo, &params) {
            Ok(preflight) => preflight,
            Err(failure) => {
                return Err(self
                    .audit_early_failure(
                        token,
                        "git.commit",
                        &repo,
                        failure.branch.as_deref(),
                        failure.outcome,
                        failure.error,
                    )
                    .await)
            }
        };

        let canonical_repo = match self.enforce_write_policy(&repo, &branch) {
            Ok(p) => p,
            Err(e) => {
                self.emit_write_audit(
                    token,
                    "git.commit",
                    &repo,
                    Some(&branch),
                    "deny",
                    EventOutcome::Denied,
                    None,
                )
                .await;
                return Err(e);
            }
        };

        let exec: Result<String, RuntimeError> = (|| {
            if let Some(add_argv) = &add_argv {
                run_git(&canonical_repo, add_argv)?;
            }
            run_git(&canonical_repo, &commit_argv)?;
            let sha = run_git(
                &canonical_repo,
                &["rev-parse".to_string(), "HEAD".to_string()],
            )?
            .trim()
            .to_string();
            Ok(sha)
        })();

        match exec {
            Ok(sha) => {
                self.emit_write_audit(
                    token,
                    "git.commit",
                    &canonical_repo,
                    Some(&branch),
                    "allow",
                    EventOutcome::Success,
                    Some(&sha),
                )
                .await;
                Ok(json!({
                    "repo": canonical_repo.display().to_string(),
                    "sha": sha,
                }))
            }
            Err(e) => {
                self.emit_write_audit(
                    token,
                    "git.commit",
                    &canonical_repo,
                    Some(&branch),
                    "allow",
                    EventOutcome::Error,
                    None,
                )
                .await;
                Err(e)
            }
        }
    }

    pub(crate) async fn handle_branch(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let repo = parse_repo_param(&params)?;
        let lock = repo_write_lock(&repo);
        let _guard = lock.lock().await;
        let BranchPreflight { name, from, argv } = match prepare_branch(&repo, &params) {
            Ok(preflight) => preflight,
            Err(failure) => {
                return Err(self
                    .audit_early_failure(
                        token,
                        "git.branch",
                        &repo,
                        failure.branch.as_deref(),
                        failure.outcome,
                        failure.error,
                    )
                    .await)
            }
        };

        let canonical_repo = match self.enforce_write_policy(&repo, &name) {
            Ok(p) => p,
            Err(e) => {
                self.emit_write_audit(
                    token,
                    "git.branch",
                    &repo,
                    Some(&name),
                    "deny",
                    EventOutcome::Denied,
                    None,
                )
                .await;
                return Err(e);
            }
        };

        match run_git(&canonical_repo, &argv) {
            Ok(_) => {
                self.emit_write_audit(
                    token,
                    "git.branch",
                    &canonical_repo,
                    Some(&name),
                    "allow",
                    EventOutcome::Success,
                    None,
                )
                .await;
                Ok(json!({
                    "repo": canonical_repo.display().to_string(),
                    "name": name,
                    "from": from,
                }))
            }
            Err(e) => {
                self.emit_write_audit(
                    token,
                    "git.branch",
                    &canonical_repo,
                    Some(&name),
                    "allow",
                    EventOutcome::Error,
                    None,
                )
                .await;
                Err(e)
            }
        }
    }

    pub(crate) async fn handle_push(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let repo = parse_repo_param(&params)?;
        let lock = repo_write_lock(&repo);
        let _guard = lock.lock().await;
        let PushPreflight {
            branch,
            remote,
            argv,
        } = match prepare_push(&repo, &params) {
            Ok(preflight) => preflight,
            Err(failure) => {
                return Err(self
                    .audit_early_failure(
                        token,
                        "git.push",
                        &repo,
                        failure.branch.as_deref(),
                        failure.outcome,
                        failure.error,
                    )
                    .await)
            }
        };

        let canonical_repo = match self.enforce_write_policy(&repo, &branch) {
            Ok(p) => p,
            Err(e) => {
                self.emit_write_audit(
                    token,
                    "git.push",
                    &repo,
                    Some(&branch),
                    "deny",
                    EventOutcome::Denied,
                    None,
                )
                .await;
                return Err(e);
            }
        };

        match run_git(&canonical_repo, &argv) {
            Ok(_) => {
                self.emit_write_audit(
                    token,
                    "git.push",
                    &canonical_repo,
                    Some(&branch),
                    "allow",
                    EventOutcome::Success,
                    None,
                )
                .await;
                Ok(json!({
                    "repo": canonical_repo.display().to_string(),
                    "remote": remote,
                    "branch": branch,
                }))
            }
            Err(e) => {
                self.emit_write_audit(
                    token,
                    "git.push",
                    &canonical_repo,
                    Some(&branch),
                    "allow",
                    EventOutcome::Error,
                    None,
                )
                .await;
                Err(e)
            }
        }
    }

    /// Appends exactly one supplementary audit `Event` (ADR-108 rule 2) per
    /// write attempt, on every exit path — handler-allowlist-denied,
    /// git-failed, and success alike — carrying `repo`/`branch`/`decision`
    /// and, on success, `sha`. This is in addition to, not a replacement
    /// for, the dispatch-level gate-check audit
    /// (`khive-runtime::pack::VerbRegistry::dispatch_with_identity`, fired
    /// automatically for every verb): that audit only ever records the
    /// Gate's own allow/deny decision, so without this call a
    /// handler-allowlist denial left only a Gate "Allow" audit paired with
    /// an errored outcome — a misleading trail (ADR-108 review r2 finding).
    /// `decision` is `"allow"` when this handler's own precondition passed
    /// (`enforce_write_policy` returned `Ok`, regardless of whether git
    /// itself then succeeded) and `"deny"` when it did not. Best-effort: a
    /// store failure is logged and swallowed, exactly like every other audit
    /// write in this codebase (ADR-018 "audit storage failures don't
    /// propagate") — it must never fail a write that git itself completed.
    #[allow(clippy::too_many_arguments)]
    async fn emit_write_audit(
        &self,
        token: &NamespaceToken,
        verb: &str,
        repo: &Path,
        branch: Option<&str>,
        decision: &str,
        outcome: EventOutcome,
        sha: Option<&str>,
    ) {
        let Ok(store) = self.runtime().events(token) else {
            return;
        };
        let mut payload = json!({
            "repo": repo.display().to_string(),
            "decision": decision,
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
        .with_outcome(outcome)
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
