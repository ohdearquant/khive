//! Batch, cursor-based git-history ingester (ADR-088 §5). One-shot: walks
//! local git history plus (optionally) `gh`-fetched issues and pull
//! requests, and writes `commit` / `issue` / `pull_request` notes through
//! the standard `create` verb. See crates/khive-pack-git/docs/api/ingest.md
//! and crates/khive-pack-git/docs/ingest.md for the full design notes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncRead;
use tokio::process::Command as AsyncCommand;
use uuid::Uuid;

use khive_runtime::bounded_read::read_to_end_bounded_async;
use khive_runtime::{
    secret_gate, KhiveRuntime, LinkSpec, NamespaceToken, RuntimeError, VerbRegistry,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::EdgeRelation;

use crate::hook;
use crate::refs;
use crate::source::remote_url_to_slug;
use crate::sql::{sql, EntityRows};
use khive_runtime::process_retry::{
    spawn_retrying_executable_busy, spawn_retrying_executable_busy_async,
    EXECUTABLE_BUSY_BACKOFF_MS,
};

mod commit_history;
mod commit_ingest;
mod github_ingest;
mod github_records;

use github_ingest::*;

use github_records::*;

pub(crate) use commit_ingest::*;

pub(crate) use commit_history::*;

#[cfg(test)]
#[path = "commit_text_tests.rs"]
mod commit_text_tests;

fn mask_git_ingest(text: &str) -> std::borrow::Cow<'_, str> {
    secret_gate::mask_for_redaction_surface(secret_gate::RedactionSurface::GitIngest, text)
}

/// The GitHub CLI is an optional network dependency, so neither a stalled
/// process nor an unbounded response may occupy an ingest worker indefinitely.
const GH_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const GH_STDOUT_LIMIT: usize = 32 * 1024 * 1024;
const GH_STDERR_LIMIT: usize = 64 * 1024;
const ORIGIN_STDOUT_LIMIT: usize = 16 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum IngestCommandError {
    NotFound,
    CouldNotStart,
    TimedOut,
    StdoutTooLarge,
    StderrTooLarge,
    Io,
}

impl std::fmt::Display for IngestCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotFound => "command not found on PATH",
            Self::CouldNotStart => "command could not be started",
            Self::TimedOut => "command timed out",
            Self::StdoutTooLarge => "command stdout exceeded limit",
            Self::StderrTooLarge => "command stderr exceeded limit",
            Self::Io => "command I/O failed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for IngestCommandError {}

struct IngestCommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
}

async fn read_command_pipe(
    pipe: impl AsyncRead + Unpin,
    limit: usize,
    overflow: IngestCommandError,
) -> std::result::Result<Vec<u8>, IngestCommandError> {
    read_to_end_bounded_async(pipe, limit as u64)
        .await
        .map_err(|_| IngestCommandError::Io)?
        .ok_or(overflow)
}

async fn run_ingest_command(
    mut command: AsyncCommand,
    timeout: Duration,
    stdout_limit: usize,
) -> std::result::Result<IngestCommandOutput, IngestCommandError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child =
        spawn_retrying_executable_busy_async(&EXECUTABLE_BUSY_BACKOFF_MS, || command.spawn())
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    IngestCommandError::NotFound
                } else {
                    IngestCommandError::CouldNotStart
                }
            })?;
    let stdout = child.stdout.take().ok_or(IngestCommandError::Io)?;
    let stderr = child.stderr.take().ok_or(IngestCommandError::Io)?;
    tokio::time::timeout(timeout, async {
        let (stdout, _stderr, status) = tokio::try_join!(
            read_command_pipe(stdout, stdout_limit, IngestCommandError::StdoutTooLarge),
            read_command_pipe(stderr, GH_STDERR_LIMIT, IngestCommandError::StderrTooLarge),
            async { child.wait().await.map_err(|_| IngestCommandError::Io) }
        )?;
        Ok(IngestCommandOutput { status, stdout })
    })
    .await
    .map_err(|_| IngestCommandError::TimedOut)?
}

/// Which record kinds a `run_ingest` pass processes. `Default` selects all
/// three — the CLI's historical behavior and the `git.digest` verb's default
/// (ADR-088 Amendment 1).
#[derive(Debug, Clone, Copy)]
pub struct IngestInclude {
    pub commits: bool,
    pub issues: bool,
    pub pull_requests: bool,
}

/// Whether `run_ingest_inner` may fall back to the command working
/// directory's configured `origin` remote to derive a GitHub repository
/// identity when the parsed source carries none of its own. A remote
/// issues/pull-requests-only pass has no repository checkout that `origin`
/// could even name (see the remote branch in
/// `crates/khive-pack-git/src/handlers.rs`) -- deriving from it there would
/// silently borrow whatever repository the daemon process happens to be
/// running inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OriginIdentity {
    /// Fall back to the working directory's `origin` remote.
    DeriveFromCwd,
    /// Never consult `origin`; an absent source identity is a hard failure.
    Never,
}

impl Default for IngestInclude {
    fn default() -> Self {
        Self {
            commits: true,
            issues: true,
            pull_requests: true,
        }
    }
}

/// Options for one ingest pass.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Local path to the git repository to walk, or merely the working
    /// directory for source-bound `gh` commands when commits are excluded.
    pub repo: PathBuf,
    /// Expected GitHub `owner/repo` derived from the caller's canonical
    /// remote source. Local-path and administrative callers leave this
    /// unset, and the ingest core derives the same identity from the
    /// checkout's configured `origin`. The value is an identity constraint,
    /// never a capability hint: `gh` is always invoked with it explicitly.
    pub expected_github_repo: Option<String>,
    /// The repo-anchor `project` entity — full UUID or an 8+ hex prefix.
    pub project: String,
    /// Bounded work per call, counted across commits + issues + PRs
    /// (ADR-088 Amendment 1). `None` means unbounded — the CLI's historical
    /// one-shot behavior.
    pub max_items: Option<u64>,
    /// Which record kinds to ingest this pass.
    pub include: IngestInclude,
}

impl IngestOptions {
    /// Convenience constructor for callers that want the CLI's historical
    /// unbounded, all-kinds behavior.
    pub fn unbounded(repo: PathBuf, project: String) -> Self {
        Self {
            repo,
            expected_github_repo: None,
            project,
            max_items: None,
            include: IngestInclude::default(),
        }
    }
}

/// Bounds fresh record visits, including existing records and failed attempts. See
/// crates/khive-pack-git/docs/api/ingest.md#budget.
struct Budget {
    remaining: Option<u64>,
}

impl Budget {
    fn try_consume(&mut self) -> bool {
        match &mut self.remaining {
            None => true,
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
        }
    }

    fn exhausted(&self) -> bool {
        matches!(self.remaining, Some(0))
    }
}

/// Publish the stall at its origin: a later fallible read or checkpoint must
/// not erase the already-observed record failure from the structured report.
fn stall_cursor(cursor_stalled: &mut bool, report: &mut IngestReport) {
    *cursor_stalled = true;
    report.cursor_stalled = true;
    report.done = false;
}

/// A newly created note this pass, for `link_references`'s
/// same-pass cross-reference resolution. See
/// crates/khive-pack-git/docs/api/ingest.md#newrecordforref.
struct NewRecordForRef {
    id: Uuid,
    text: String,
}

/// Caller-visible detail for one content write the secret gate refused.
///
/// The record key is a trusted natural key (commit SHA or GitHub number),
/// and the secret itself is represented only by [`secret_gate::SecretMatch`]'s
/// detector name and masked excerpt. The rejected content is never copied
/// into the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngestWriteRefusal {
    pub verb: String,
    pub record_kind: String,
    pub record_key: String,
    pub detector: String,
    pub masked: String,
    /// Present only for a GitHub replacement attempt; true after a real UUID returns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

/// Machine-readable state of one ingest source (`commits`, `issues`,
/// `pull_requests`) after a pass — issue #1617. A reader no longer has to
/// infer coverage from `done` (a budget-cursor statement) or parse prose
/// out of `warnings[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum IngestSourceState {
    /// The source was walked to the end of its history this pass.
    Completed,
    /// The source was visited but not exhausted: the budget ran out, a
    /// paging window was left incomplete, or a per-record write failure
    /// froze the cursor (`cursor_stalled`). The reason names the cause.
    StoppedEarly(String),
    /// The source was never walked this pass. The reason names the cause:
    /// the budget was already exhausted before the source was reached, or
    /// the source-bound `gh repo view` probe could not resolve an authenticated
    /// GitHub repository for this checkout, or `gh` itself failed before the
    /// walk began. A local cursor/database read failure before remote listing
    /// is also `Skipped`, with a distinct local-failure reason. A failure
    /// after the walk began is `StoppedEarly`, never `Skipped`.
    Skipped(String),
}

/// Per-source ingest coverage for one pass (issue #1617): which sources
/// were walked to completion, which stopped early and why, and which were
/// never reached. Written by the walk paths themselves, not reconstructed
/// at report time, so the states stay truthful when a new walk path is
/// added later.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct IngestSourceStatus {
    pub commits: Option<IngestSourceState>,
    pub issues: Option<IngestSourceState>,
    pub pull_requests: Option<IngestSourceState>,
}

/// Outcome of one ingest pass. Serializable so CLI callers can emit it as JSON.
#[derive(Debug, Default, Serialize)]
pub struct IngestReport {
    /// Durable audit-event receipt for this exact successful `git.digest`
    /// response. The runtime dispatch seam fills this after the ingest report
    /// has been serialized and before returning it to the caller; direct
    /// `run_ingest` users do not receive a receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    /// Caller-supplied `max_items` before verb-level clamping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_items_requested: Option<i64>,
    /// Per-pass item budget after verb-level clamping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_items_effective: Option<u64>,
    pub commits_ingested: u64,
    pub commits_skipped_existing: u64,
    pub issues_ingested: u64,
    pub issues_skipped_existing: u64,
    pub prs_ingested: u64,
    pub prs_skipped_existing: u64,
    /// `Some(false)` when the source-bound `gh repo view` probe ran but could
    /// not resolve an authenticated GitHub repository — issues/PRs were
    /// skipped but commits still ingested (ADR-088 §5 graceful-absence rule).
    /// `Some(true)` only when that probe returned a usable `owner/repo`, which
    /// is then passed explicitly to every `gh ... list --repo` call. `None`
    /// when this pass never probed: the probe runs only when `include`
    /// requests issues or pull requests, and a commits-only pass says nothing
    /// about `gh` either way (issue #1645).
    pub gh_available: Option<bool>,
    pub warnings: Vec<String>,
    /// Number of per-record content writes refused by the runtime secret gate
    /// during this pass. Callers can assert this is zero independently of
    /// unrelated warnings.
    pub writes_refused: u64,
    /// Safe structured details for every entry counted by `writes_refused`.
    /// Each item names the refused verb and record without echoing the
    /// rejected content.
    pub write_refusals: Vec<IngestWriteRefusal>,
    /// `false` when `max_items` was exhausted before this pass reached the
    /// end of every included kind's history — callers loop until `true`
    /// (ADR-088 Amendment 1). Always `true` for an unbounded
    /// (`max_items: None`) pass.
    pub done: bool,
    /// `true` when a per-record write failure pinned the commits cursor at
    /// the last contiguous success this pass. The failed record is retried
    /// (and its warning re-surfaced) on every subsequent pass until it is
    /// fixed upstream. `done` is forced `false` whenever this is set: commits
    /// beyond the stall were still attempted this pass, but history cannot be
    /// called exhausted while the cursor cannot advance (issue #1645).
    pub cursor_stalled: bool,
    /// The repo-anchor `project` entity id this pass resolved (or the
    /// verb-level caller created).
    pub project_id: Option<String>,
    /// `true` when the `git.digest` verb auto-created the `project` anchor
    /// because none was found (ADR-088 Amendment 1) — never set by
    /// `run_ingest` itself, only by the verb handler after it returns.
    pub project_created: bool,
    /// `true` when `project_created` fired over a soft-deleted anchor that
    /// still had a live corpus annotating it (issue #1173) — the resolved
    /// repo identity matched no LIVE `project` entity, but did match a
    /// soft-deleted one with `annotates` edges still pointing at it. This
    /// distinguishes "first ingest of a new repo" from "anchor lost,
    /// corpus about to duplicate under a fresh anchor" — never set by
    /// `run_ingest` itself, only by the verb handler after it returns.
    pub orphaned_corpus_detected: bool,
    /// The dangling anchor id `orphaned_corpus_detected` was found under.
    /// `None` unless `orphaned_corpus_detected` is `true`.
    pub orphaned_project_id: Option<String>,
    /// Count of `commit`/`issue`/`pull_request` notes still `annotates`-
    /// linked to `orphaned_project_id`. `0` unless `orphaned_corpus_detected`
    /// is `true`.
    pub orphaned_note_count: u64,
    /// `annotates` edges created from a `Closes/Fixes/Resolves #N` or bare
    /// `#N` reference in a commit message or issue/PR body to the referenced
    /// issue/PR note (ADR-088 Amendment 1 ingest enrichment).
    pub reference_edges_created: u64,
    /// References that named a number this pass could not resolve to an
    /// ingested issue/PR note within the same project — skipped, not an
    /// error (fail-open).
    pub reference_edges_unresolved: u64,
    /// `precedes` edges created from a commit's `parents[]` to the commit
    /// itself (ADR-088 Amendment 1 ingest enrichment).
    pub parent_edges_created: u64,
    /// Commits whose masked content exceeded `MAX_COMMIT_EMBED_BYTES`: the
    /// full commit note was stored and FTS-indexed unchanged, but the vector
    /// embedding input was truncated to a UTF-8-safe head prefix at the cap
    /// (issue #764). Only incremented for successfully created commits.
    pub commit_embeddings_truncated: u64,
    /// Total `commit` notes annotating this pass's project in the database
    /// AFTER this pass completes — a row count, not an in-memory delta
    /// (issue #1045). `commits_ingested` only counts creations THIS process
    /// observed; if the daemon respawns mid-digest and drops a response
    /// after the write already landed, that response's `commits_ingested`
    /// is lost from any cumulative sum a caller keeps across calls, even
    /// though the row was durably written. This field is derived by
    /// querying the DB after the walk, so it survives that loss and is safe
    /// to compare directly against an independent source of truth (e.g.
    /// `git rev-list --count <ref>`).
    pub commits_total_in_db: u64,
    /// Touched paths the `--name-only` pass recorded but `changed_paths`
    /// storage cannot carry: valid Unix filenames that violate the hook's
    /// canonical shape (a `\` byte, an `X:` drive prefix, a leading `/`,
    /// or an empty/`.`/`..` component). Only actual predicate rejections
    /// count — dedup/post-masking collisions in the stored array are not
    /// drops. Dropped before create rather than failing the whole commit —
    /// see
    /// crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations.
    pub changed_paths_filtered_noncanonical: u64,
    /// Changed paths whose `(source_revision, source_path)` module binding
    /// was unusable and therefore received no code-module annotation. Two
    /// shapes fold into this counter: an ambiguous key (two or more live
    /// rows, whether two or more have parseable ids or at most one has a
    /// parseable id) and the single-row sub-case whose one row's id does not
    /// parse (not ambiguous — just no bindable candidate). Counted only when
    /// an ingested commit's path actually hits the key, so unusable keys
    /// untouched by this pass never inflate the count.
    pub code_module_ambiguous_path_skips: u64,
    /// Per-source ingest coverage for this pass (issue #1617): each
    /// included source reports `completed`, `stopped_early { reason }`, or
    /// `skipped { reason }`, so a gate refusal, budget exhaustion, and a
    /// `gh`/remote skip are distinguishable without parsing `warnings[]`.
    /// Additive companion to `done`/`cursor_stalled`, which keep their
    /// existing budget-cursor meaning.
    pub sources: IngestSourceStatus,
    /// `true` only when every included source was walked to the end of its
    /// history this pass — "silence means nothing left", as opposed to
    /// "stopped before the end" (budget exhausted, incomplete paging
    /// window, or a frozen cursor). Unlike `done`, this is a coverage
    /// statement, not a resume-loop signal (issue #1617). Vacuously `true`
    /// when `include` is empty: no source was requested, so nothing can
    /// count against it — it is a statement about the REQUESTED sources,
    /// not about the repository.
    pub history_exhausted: bool,
}

fn record_write_failure(
    report: &mut IngestReport,
    verb: &str,
    record_kind: &str,
    record_key: String,
    error: RuntimeError,
) {
    if let RuntimeError::SecretDetected(secret) = error.refusal_source() {
        report.writes_refused += 1;
        report.write_refusals.push(IngestWriteRefusal {
            verb: verb.to_string(),
            record_kind: record_kind.to_string(),
            record_key: record_key.clone(),
            detector: secret.detector.to_string(),
            masked: secret.masked.clone(),
            redacted: None,
        });
    }
    report
        .warnings
        .push(format!("{verb} {record_kind} {record_key}: {error}"));
}

struct GithubCreateResult {
    response: Value,
    content: String,
}

/// Rebuild the replacement from trusted identity and a closed set of typed fields.
/// Remote prose, labels, actors and branch names never carry through this path.
fn redacted_github_payload(kind: &str, number: u64, project_id: Uuid, original: &Value) -> Value {
    let properties = original.get("properties");
    let timestamp = |key: &str| {
        properties
            .and_then(|p| p.get(key))
            .and_then(Value::as_str)
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|dt| khive_runtime::presentation::format_utc_rfc3339_seconds(&dt))
    };
    let mut replacement = json!({
        "number": number,
        "project_id": project_id.to_string(),
        "title": "GitHub content redacted",
        "author": null,
        "created_at": timestamp("created_at"),
        "closed_at": timestamp("closed_at"),
    });
    if kind == "pull_request" {
        replacement["merged_at"] = json!(timestamp("merged_at"));
        replacement["base_ref"] = Value::Null;
        replacement["head_ref"] = Value::Null;
    } else {
        replacement["labels"] = json!([]);
        if let StateReasonField::Valid(reason) = canonical_issue_state_reason(
            properties
                .and_then(|p| p.get("state_reason"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        ) {
            replacement["state_reason"] = json!(reason);
        }
    }
    json!({
        "kind": kind,
        "name": format!("#{number} [redacted]"),
        "content": "GitHub content was redacted after a secret-gate refusal.",
        "properties": replacement,
        "annotates": [project_id.to_string()],
    })
}

/// Both GitHub walkers use this single production dispatch instantiation.
async fn create_github_record(
    registry: &VerbRegistry,
    token: &NamespaceToken,
    report: &mut IngestReport,
    kind: &str,
    number: u64,
    project_id: Uuid,
    payload: Value,
) -> Option<GithubCreateResult> {
    let fallback = redacted_github_payload(kind, number, project_id, &payload);
    create_github_record_with(report, kind, number, payload, fallback, |params| {
        crate::dispatch_from_token(registry, token, "create", params)
    })
    .await
}

/// A typed refusal alone permits one replacement; every other failure still stalls.
/// The closure is the real token-bound dispatch in production, with no fault flag.
async fn create_github_record_with<F, Fut>(
    report: &mut IngestReport,
    kind: &str,
    number: u64,
    payload: Value,
    fallback: Value,
    mut dispatch: F,
) -> Option<GithubCreateResult>
where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<Value, RuntimeError>>,
{
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match dispatch(payload).await {
        Ok(response) => Some(GithubCreateResult { response, content }),
        Err(error) => {
            let retry = matches!(error.refusal_source(), RuntimeError::SecretDetected(_));
            let first_refusal = report.write_refusals.len();
            record_write_failure(report, "create", kind, format!("#{number}"), error);
            if !retry {
                return None;
            }
            report.write_refusals.get_mut(first_refusal)?.redacted = Some(false);
            let content = fallback
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            match dispatch(fallback).await {
                Ok(response) => {
                    if response
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| Uuid::parse_str(id).ok())
                        .is_some()
                    {
                        report.write_refusals[first_refusal].redacted = Some(true);
                    }
                    Some(GithubCreateResult { response, content })
                }
                Err(error) => {
                    let next_refusal = report.write_refusals.len();
                    record_write_failure(report, "create", kind, format!("#{number}"), error);
                    if let Some(refusal) = report.write_refusals.get_mut(next_refusal) {
                        refusal.redacted = Some(false);
                    }
                    None
                }
            }
        }
    }
}

/// Overwrite a walker's source slot with `StoppedEarly(reason)` at an
/// end-of-walk arm. See
/// crates/khive-pack-git/docs/ingest.md#seed-invariant-for-pin_stopped_early.
fn pin_stopped_early(slot: &mut Option<IngestSourceState>, reason: String) {
    match slot {
        Some(state) => *state = IngestSourceState::StoppedEarly(reason),
        None => {
            debug_assert!(
                false,
                "walker reached an end-of-walk arm without seeding its source state"
            );
            *slot = Some(IngestSourceState::StoppedEarly(reason));
        }
    }
}

/// Run one ingest pass over `opts.repo`: issues + PRs first (via `gh`, when
/// available), then commits (via local `git log`), each bounded by
/// `opts.max_items` and cursor-resumable (call again while the returned
/// `IngestReport.done` is `false`). Returns an error only for a failure that
/// aborts the whole pass (e.g. an unresolvable `opts.project`); per-record
/// failures are collected in `IngestReport.warnings` instead. See
/// crates/khive-pack-git/docs/api/ingest.md#run_ingest_with_commit_recovery for
/// why this has no self-healing recovery (unlike the verb-handler path).
pub async fn run_ingest(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
) -> Result<IngestReport> {
    run_ingest_inner(
        runtime,
        token,
        registry,
        opts,
        OriginIdentity::DeriveFromCwd,
        |_repo, _err| Ok(None),
    )
    .await
}

/// Run a remote issues/pull-requests-only pass without consulting the
/// command working directory's `origin` when the parsed source has no
/// GitHub identity. The handler supplies a source-bound identity whenever
/// one exists; an absent one must degrade safely rather than borrow an
/// unrelated checkout identity from the daemon's current directory.
pub(crate) async fn run_remote_api_ingest(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
) -> Result<IngestReport> {
    if opts.include.commits {
        return Err(anyhow!(
            "remote API-only ingest cannot include commit history"
        ));
    }
    run_ingest_inner(
        runtime,
        token,
        registry,
        opts,
        OriginIdentity::Never,
        |_repo, _err| Ok(None),
    )
    .await
}

/// Same one-shot ingest pass as `run_ingest`, but a classified
/// missing-promisor-object failure is retried through `recover` (issue
/// #765) instead of aborting the whole pass. See
/// crates/khive-pack-git/docs/api/ingest.md#run_ingest_with_commit_recovery.
pub(crate) async fn run_ingest_with_commit_recovery(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
    recover: impl FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>> + Send + 'static,
) -> Result<IngestReport> {
    run_ingest_inner(
        runtime,
        token,
        registry,
        opts,
        OriginIdentity::DeriveFromCwd,
        recover,
    )
    .await
}

async fn run_ingest_inner(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
    origin_identity: OriginIdentity,
    recover: impl FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>> + Send + 'static,
) -> Result<IngestReport> {
    let mut report = IngestReport {
        done: true,
        ..IngestReport::default()
    };

    let project_id = resolve_id(runtime, token, &opts.project)
        .await?
        .ok_or_else(|| anyhow!("--project {:?} did not resolve to an entity", opts.project))?;
    report.project_id = Some(project_id.to_string());

    let mut merge_sha_to_pr: HashMap<String, Uuid> = HashMap::new();
    let mut number_to_pr: HashMap<u64, Uuid> = HashMap::new();
    let mut budget = Budget {
        remaining: opts.max_items,
    };
    let mut new_records: Vec<NewRecordForRef> = Vec::new();
    // Per-source completion flags folded into `report.sources` at the end of
    // the pass (issue #1617). The flags are seeded by the failure/walk paths
    // themselves (`ingest_commits`/`ingest_prs`/`ingest_issues` set them
    // false on a budget break, incomplete paging window, or frozen cursor),
    // never reconstructed from counts at report time.
    let mut commits_complete = false;
    let mut prs_complete = false;
    let mut issues_complete = false;
    // Set when a gh walker returned Err AFTER recording a walk state
    // (walked-then-failed): the source states stay as the walker left them
    // (possibly `completed`), but the pass itself failed, so
    // `history_exhausted` must not claim total coverage on their strength
    // alone.
    let mut gh_walk_failed_after_walk = false;

    // Graceful degradation covers both "gh is not on PATH" and "gh is present
    // but cannot resolve an authenticated GitHub repository for this checkout"
    // (e.g. a non-GitHub or local-only repo). Either way, requested issues/PRs
    // are skipped with a structured reason and commits still ingest (ADR-088
    // §5). The successful probe returns the exact owner/repo later passed via
    // `--repo`, so PATH presence is never mistaken for remote usability.
    if opts.include.issues || opts.include.pull_requests {
        let gh_probe = probe_gh_repository(
            &opts.repo,
            opts.expected_github_repo.as_deref(),
            origin_identity,
        )
        .await;
        if let Ok(gh_repo) = &gh_probe {
            report.gh_available = Some(true);
            if opts.include.pull_requests && !budget.exhausted() {
                match ingest_prs(
                    runtime,
                    token,
                    registry,
                    &opts.repo,
                    gh_repo,
                    project_id,
                    &mut report,
                    &mut merge_sha_to_pr,
                    &mut number_to_pr,
                    &mut budget,
                    &mut new_records,
                    &mut prs_complete,
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        // Distinguishes a never-walked source (first-page
                        // failure) from walked-then-failed. See
                        // crates/khive-pack-git/docs/ingest.md#walker-exit-states.
                        let walked = matches!(
                            report.sources.pull_requests.as_ref(),
                            Some(IngestSourceState::Completed | IngestSourceState::StoppedEarly(_))
                        );
                        if walked {
                            // Nothing was "skipped" once the walk began — name
                            // how far it got before the pass failed.
                            let visited = report.prs_ingested + report.prs_skipped_existing;
                            let noun = if visited == 1 { "record" } else { "records" };
                            report.warnings.push(format!(
                                "gh pr list failed after walking {visited} {noun} — \
                                 stopped early, not skipped: {e}"
                            ));
                        } else if report.sources.pull_requests.is_none() {
                            report
                                .warnings
                                .push(format!("gh pr list failed, skipping pull requests: {e}"));
                        } else {
                            report.warnings.push(format!(
                                "pull request ingest failed before remote listing; local source failure retained: {e}"
                            ));
                        }
                        if !walked && report.sources.pull_requests.is_none() {
                            report.sources.pull_requests = Some(IngestSourceState::Skipped(
                                format!("gh pr list failed: {e}"),
                            ));
                        } else if walked {
                            match report.sources.pull_requests.as_mut() {
                                Some(IngestSourceState::StoppedEarly(reason)) => {
                                    reason.push_str(&format!(
                                        "; pass then failed after the walk: {e}"
                                    ));
                                }
                                // A completed walk whose pass then fails is
                                // downgraded to stopped-early so `completed`
                                // never outlives an unproven pass.
                                Some(state @ IngestSourceState::Completed) => {
                                    *state = IngestSourceState::StoppedEarly(format!(
                                        "walk completed but the pass then failed: {e}"
                                    ));
                                }
                                Some(IngestSourceState::Skipped(_)) | None => {}
                            }
                            // Walked-then-failed leaves durability unproven,
                            // so the resume loop must not treat the source as
                            // finished; a first-fetch failure pins nothing.
                            report.done = false;
                            gh_walk_failed_after_walk = true;
                        }
                        prs_complete = false;
                    }
                }
            } else if opts.include.pull_requests {
                report.sources.pull_requests = Some(IngestSourceState::Skipped(
                    "budget exhausted before pull requests were reached".to_string(),
                ));
            }
            if opts.include.issues && !budget.exhausted() {
                if let Err(e) = ingest_issues(
                    runtime,
                    token,
                    registry,
                    &opts.repo,
                    gh_repo,
                    project_id,
                    &mut report,
                    &mut budget,
                    &mut new_records,
                    &mut issues_complete,
                )
                .await
                {
                    // See the PR arm above: "skipping" is only accurate for
                    // a first-fetch failure; a walked-then-failed source
                    // names how far the walk got.
                    let walked = matches!(
                        report.sources.issues.as_ref(),
                        Some(IngestSourceState::Completed | IngestSourceState::StoppedEarly(_))
                    );
                    if walked {
                        let visited = report.issues_ingested + report.issues_skipped_existing;
                        let noun = if visited == 1 { "record" } else { "records" };
                        report.warnings.push(format!(
                            "gh issue list failed after walking {visited} {noun} — \
                             stopped early, not skipped: {e}"
                        ));
                    } else if report.sources.issues.is_none() {
                        report
                            .warnings
                            .push(format!("gh issue list failed, skipping issues: {e}"));
                    } else {
                        report.warnings.push(format!(
                            "issue ingest failed before remote listing; local source failure retained: {e}"
                        ));
                    }
                    // See the PR arm above: `Skipped` only when the walk
                    // never began (the walker signals that by leaving its
                    // state unset); a walked-then-failed source keeps the
                    // state the walker already wrote.
                    if !walked && report.sources.issues.is_none() {
                        report.sources.issues = Some(IngestSourceState::Skipped(format!(
                            "gh issue list failed: {e}"
                        )));
                    } else if walked {
                        match report.sources.issues.as_mut() {
                            Some(IngestSourceState::StoppedEarly(reason)) => {
                                reason.push_str(&format!("; pass then failed after the walk: {e}"));
                            }
                            // See the PR arm above: a completed walk whose
                            // pass then fails is downgraded to
                            // stopped-early so the failure is never
                            // invisible next to a `completed` claim.
                            Some(state @ IngestSourceState::Completed) => {
                                *state = IngestSourceState::StoppedEarly(format!(
                                    "walk completed but the pass then failed: {e}"
                                ));
                            }
                            Some(IngestSourceState::Skipped(_)) | None => {}
                        }
                        // See the PR arm above: walked-then-failed is not a
                        // finished source; a first-fetch failure pins
                        // nothing.
                        report.done = false;
                        gh_walk_failed_after_walk = true;
                    }
                    issues_complete = false;
                }
            } else if opts.include.issues {
                report.sources.issues = Some(IngestSourceState::Skipped(
                    "budget exhausted before issues were reached".to_string(),
                ));
            }
        } else {
            let reason = gh_probe
                .as_ref()
                .expect_err("successful probe handled in the preceding branch");
            report.gh_available = Some(false);
            report.warnings.push(format!(
                "{reason}; skipped requested GitHub sources — commits still ingest"
            ));
            if opts.include.pull_requests {
                report.sources.pull_requests = Some(IngestSourceState::Skipped(reason.to_string()));
            }
            if opts.include.issues {
                report.sources.issues = Some(IngestSourceState::Skipped(reason.to_string()));
            }
        }
    }

    if opts.include.commits && !budget.exhausted() {
        match ingest_commits(
            runtime,
            token,
            registry,
            &opts.repo,
            project_id,
            &merge_sha_to_pr,
            &number_to_pr,
            &mut report,
            &mut budget,
            &mut new_records,
            recover,
            &mut commits_complete,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                // A `Some` slot beside an Err means the walk ran and the pass
                // failed after it; a `None` slot is a pre-walk hard error. See
                // crates/khive-pack-git/docs/ingest.md#walker-exit-states.
                if report.sources.commits.is_some() {
                    match report.sources.commits.as_mut() {
                        Some(IngestSourceState::StoppedEarly(reason)) => {
                            reason.push_str(&format!("; pass then failed after the walk: {e}"));
                        }
                        // See the gh arms above: a completed walk whose pass
                        // then fails is downgraded so the failure is never
                        // invisible next to a `completed` claim.
                        Some(state @ IngestSourceState::Completed) => {
                            *state = IngestSourceState::StoppedEarly(format!(
                                "walk completed but the pass then failed: {e}"
                            ));
                        }
                        Some(IngestSourceState::Skipped(_)) | None => {}
                    }
                    report
                        .warnings
                        .push(format!("commit ingest failed after the walk: {e}"));
                    report.done = false;
                    commits_complete = false;
                } else {
                    return Err(e);
                }
            }
        }
    } else if opts.include.commits {
        report.sources.commits = Some(IngestSourceState::Skipped(
            "budget exhausted before commits were reached".to_string(),
        ));
    }

    if budget.exhausted() {
        report.done = false;
    }

    // Release-mode belt-and-braces fallback for a walker that returned
    // without recording its source state — an instrumentation gap, not a
    // normal stop. See crates/khive-pack-git/docs/ingest.md#walker-exit-states
    // for the fabrication risk this guards against.
    if report.sources.pull_requests.is_none() && opts.include.pull_requests {
        debug_assert!(
            false,
            "pull_requests walker returned without recording its source state"
        );
        report.sources.pull_requests = Some(if prs_complete {
            IngestSourceState::Completed
        } else {
            IngestSourceState::StoppedEarly(
                "state not recorded by walker; stopped before the PR history was exhausted".into(),
            )
        });
    }
    if report.sources.issues.is_none() && opts.include.issues {
        debug_assert!(
            false,
            "issues walker returned without recording its source state"
        );
        report.sources.issues = Some(if issues_complete {
            IngestSourceState::Completed
        } else {
            IngestSourceState::StoppedEarly(
                "state not recorded by walker; stopped before the issue history was exhausted"
                    .into(),
            )
        });
    }
    if report.sources.commits.is_none() && opts.include.commits {
        debug_assert!(
            false,
            "commits walker returned without recording its source state"
        );
        report.sources.commits = Some(if commits_complete {
            IngestSourceState::Completed
        } else {
            IngestSourceState::StoppedEarly(
                "state not recorded by walker; stopped before the commit history was exhausted"
                    .into(),
            )
        });
    }
    // A source left `None` was not requested by `include`, so it cannot
    // count against exhaustion.
    report.history_exhausted = !gh_walk_failed_after_walk
        && [
            &report.sources.commits,
            &report.sources.issues,
            &report.sources.pull_requests,
        ]
        .into_iter()
        .all(|s| {
            s.as_ref()
                .is_none_or(|state| matches!(state, IngestSourceState::Completed))
        });

    link_references(
        runtime,
        token,
        registry,
        project_id,
        &new_records,
        &mut report,
    )
    .await;

    // Derived fresh from the database on every pass (regardless of which
    // branch above ran, or whether this pass ingested anything new), so it
    // stays a truthful completeness signal across daemon restarts — see
    // `IngestReport::commits_total_in_db` (issue #1045).
    report.commits_total_in_db = count_commit_notes_for_project(runtime, token, project_id).await?;

    Ok(report)
}

/// Resolve a full UUID or an 8+ hex prefix to a full UUID, unfiltered by
/// namespace.
async fn resolve_id(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    raw: &str,
) -> Result<Option<Uuid>> {
    resolve_project_id(runtime, raw).await
}

/// Resolve an explicit `git.digest(project)` UUID or 8+ hex prefix without a
/// namespace filter. A full UUID is accepted as supplied; only a prefix is
/// looked up, returning `Ok(None)` when it matches no record. This helper
/// does not check that the resolved id is a live `project` entity.
pub async fn resolve_project_id(runtime: &KhiveRuntime, raw: &str) -> Result<Option<Uuid>> {
    if let Ok(u) = Uuid::parse_str(raw) {
        return Ok(Some(u));
    }
    runtime
        .resolve_prefix_unfiltered(raw)
        .await
        .map_err(anyhow::Error::new)
}

/// Find an existing `issue` or `pull_request` note by `properties.number`
/// within `project_id`.
async fn find_issue_or_pr_by_number(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
    number: u64,
) -> Result<Option<Uuid>> {
    if let Some(id) = find_by_number(runtime, token, "issue", project_id, number).await? {
        return Ok(Some(id));
    }
    find_by_number(runtime, token, "pull_request", project_id, number).await
}

/// Post-ingestion sweep: extract GitHub reference-grammar mentions from
/// every note created this pass and materialize `annotates` edges to the
/// referenced issue/PR note. Fail-open. See
/// crates/khive-pack-git/docs/api/ingest.md#link_references.
async fn link_references(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    project_id: Uuid,
    new_records: &[NewRecordForRef],
    report: &mut IngestReport,
) {
    for record in new_records {
        let mentions = refs::dedupe_prefer_closes(refs::extract_references(&record.text));
        for mention in mentions {
            let target = match find_issue_or_pr_by_number(
                runtime,
                token,
                project_id,
                mention.number,
            )
            .await
            {
                Ok(Some(id)) => id,
                Ok(None) => {
                    report.reference_edges_unresolved += 1;
                    continue;
                }
                Err(e) => {
                    report
                        .warnings
                        .push(format!("resolving reference #{}: {e}", mention.number));
                    continue;
                }
            };
            if target == record.id {
                // A note referencing its own number (rare, e.g. a PR body
                // that quotes its own number) — not a real cross-reference.
                continue;
            }
            match crate::dispatch_from_token(
                registry,
                token,
                "link",
                json!({
                        "source_id": record.id.to_string(),
                        "target_id": target.to_string(),
                        "relation": "annotates",
                        "metadata": { "ref_kind": mention.kind.as_str() },
                }),
            )
            .await
            {
                Ok(_) => report.reference_edges_created += 1,
                Err(e) => report.warnings.push(format!(
                    "linking reference #{} from {}: {e}",
                    mention.number, record.id
                )),
            }
        }
    }
}

/// Resolve the exact GitHub `owner/repo` that `gh` can access for this
/// checkout, passed explicitly to `gh repo view` (never argument-less
/// selection). Failure strings are stable and credential-safe — see the
/// module overview in crates/khive-pack-git/docs/api/ingest.md.
#[derive(Debug, PartialEq, Eq)]
enum GhProbeError {
    Reason(&'static str),
    GhTimedOut,
    OriginTimedOut,
}

impl std::fmt::Display for GhProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reason(reason) => f.write_str(reason),
            Self::GhTimedOut => f.write_str("gh CLI repository probe timed out"),
            Self::OriginTimedOut => f.write_str("git origin repository probe timed out"),
        }
    }
}

impl std::error::Error for GhProbeError {}

async fn probe_gh_repository(
    repo: &Path,
    expected: Option<&str>,
    origin_identity: OriginIdentity,
) -> std::result::Result<String, GhProbeError> {
    probe_gh_repository_with_command(
        repo,
        expected,
        origin_identity,
        Path::new("gh"),
        GH_COMMAND_TIMEOUT,
    )
    .await
}

async fn probe_gh_repository_with_command(
    repo: &Path,
    expected: Option<&str>,
    origin_identity: OriginIdentity,
    gh_program: &Path,
    timeout: Duration,
) -> std::result::Result<String, GhProbeError> {
    let expected = match (expected, origin_identity) {
        (Some(expected), _) => validate_owner_repo(expected).map_err(GhProbeError::Reason)?,
        (None, OriginIdentity::DeriveFromCwd) => github_repository_from_origin(repo).await?,
        (None, OriginIdentity::Never) => {
            return Err(GhProbeError::Reason(
                "remote source has no usable github.com repository identity",
            ))
        }
    };
    let mut command = AsyncCommand::new(gh_program);
    command
        .args([
            "repo",
            "view",
            expected.as_str(),
            "--json",
            "nameWithOwner,url",
        ])
        .current_dir(repo)
        // Process-global GH_REPO/GH_HOST overrides could otherwise make this
        // checkout appear usable by probing a different repo or host.
        .env_remove("GH_REPO")
        .env_remove("GH_HOST")
        .env("GH_PROMPT_DISABLED", "1");
    let output = run_ingest_command(command, timeout, ORIGIN_STDOUT_LIMIT)
        .await
        .map_err(|error| match error {
            IngestCommandError::TimedOut => GhProbeError::GhTimedOut,
            IngestCommandError::NotFound => GhProbeError::Reason("gh CLI not found on PATH"),
            IngestCommandError::CouldNotStart => {
                GhProbeError::Reason("gh CLI could not be started")
            }
            IngestCommandError::StdoutTooLarge | IngestCommandError::StderrTooLarge => {
                GhProbeError::Reason("gh CLI repository probe output exceeded limit")
            }
            IngestCommandError::Io => GhProbeError::Reason("gh CLI repository probe I/O failed"),
        })?;
    if !output.status.success() {
        return Err(GhProbeError::Reason(
            "gh CLI could not resolve an authenticated GitHub repository for this checkout",
        ));
    }
    parse_gh_repository_identity(&output.stdout, &expected).map_err(GhProbeError::Reason)
}

/// Derive a GitHub `owner/repo` from the checkout's fetch identity. Only the
/// configured `origin` is authoritative: another remote, or `gh`'s own local
/// default, must never select the issue/PR source for this ingest.
async fn github_repository_from_origin(repo: &Path) -> std::result::Result<String, GhProbeError> {
    let mut command = AsyncCommand::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", "origin"])
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = run_ingest_command(command, GH_COMMAND_TIMEOUT, ORIGIN_STDOUT_LIMIT)
        .await
        .map_err(|error| match error {
            IngestCommandError::TimedOut => GhProbeError::OriginTimedOut,
            _ => {
                GhProbeError::Reason("git CLI could not resolve this checkout's origin repository")
            }
        })?;
    if !output.status.success() {
        return Err(GhProbeError::Reason(
            "checkout has no usable github.com origin repository",
        ));
    }
    let origin = std::str::from_utf8(&output.stdout)
        .map(str::trim)
        .map_err(|_| GhProbeError::Reason("checkout has no usable github.com origin repository"))?;
    let slug = remote_url_to_slug(origin).ok_or(GhProbeError::Reason(
        "checkout has no usable github.com origin repository",
    ))?;
    let mut segments = slug.split('/');
    let (Some(host), Some(owner), Some(name), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(GhProbeError::Reason(
            "checkout has no usable github.com origin repository",
        ));
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return Err(GhProbeError::Reason(
            "checkout has no usable github.com origin repository",
        ));
    }
    validate_owner_repo(&format!("{owner}/{name}")).map_err(GhProbeError::Reason)
}

fn validate_owner_repo(slug: &str) -> std::result::Result<String, &'static str> {
    let Some((owner, name)) = slug.split_once('/') else {
        return Err("invalid expected GitHub repository identity");
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || slug.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("invalid expected GitHub repository identity");
    }
    Ok(slug.to_string())
}

/// Validate the repository identity returned by `gh repo view` without
/// trusting either field in isolation. Keeping this pure makes the
/// non-GitHub and mismatched-identity boundaries deterministic to test.
fn parse_gh_repository_identity(
    stdout: &[u8],
    expected: &str,
) -> std::result::Result<String, &'static str> {
    let payload: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|_| "gh CLI returned an invalid repository identity")?;
    let slug = payload
        .get("nameWithOwner")
        .and_then(serde_json::Value::as_str)
        .ok_or("gh CLI returned an invalid repository identity")?;
    if validate_owner_repo(slug).is_err() {
        return Err("gh CLI returned an invalid repository identity");
    }
    let url = payload
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or("gh CLI returned an invalid repository identity")?;
    let url_slug =
        remote_url_to_slug(url).ok_or("gh CLI returned an invalid repository identity")?;
    if !url_slug.eq_ignore_ascii_case(&format!("github.com/{slug}")) {
        return Err("gh CLI did not resolve a github.com repository for this checkout");
    }
    if !slug.eq_ignore_ascii_case(expected) {
        return Err("gh CLI resolved a different repository than the digest source");
    }
    Ok(slug.to_string())
}

#[cfg(test)]
#[path = "ingest/gh_repository_identity_tests.rs"]
mod gh_repository_identity_tests;

#[cfg(all(test, unix))]
#[path = "ingest/gh_command_tests.rs"]
mod gh_command_tests;

#[cfg(test)]
#[path = "ingest/commit_framing_tests.rs"]
mod commit_framing_tests;

/// `sha -> [touched paths]` for only the selected page, via separate
/// NUL-delimited `--name-only` passes. See
/// crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations.
fn touched_files(repo: &Path, page_shas: &[String]) -> Result<HashMap<String, Vec<String>>> {
    // Keep argv bounded for the unbounded internal ingest API. The caller
    // supplies this page newest-first, preserving the parser's orphan-header
    // containment rule even when one page spans several commands.
    const SHAS_PER_COMMAND: usize = 256;
    let mut files_by_sha = HashMap::new();
    for shas in page_shas.chunks(SHAS_PER_COMMAND) {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .arg("log")
            .arg("--no-walk=unsorted")
            .arg("-z")
            .arg("--name-only")
            .arg("--no-renames")
            .arg("--diff-merges=first-parent")
            // Git paths are always repository-relative, so no tracked path
            // token can start with `/`. This absolute-looking prefix is an
            // unambiguous header sentinel in the NUL-delimited stream.
            .arg(format!("--pretty=format:/{RECORD_SEP}%H"))
            .args(shas)
            .arg("--")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output =
            spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || command.spawn())
                .and_then(|child| child.wait_with_output())
                .context("spawning git log --name-only")?;
        if !output.status.success() {
            return Err(anyhow::Error::new(GitLogError {
                phase: GitLogPhase::TouchedFiles,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }));
        }
        files_by_sha.extend(parse_touched_files(&output.stdout)?);
    }
    Ok(files_by_sha)
}

/// Decode the `-z --name-only` stream [`touched_files`] produces. See
/// crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations
/// for the header-loss ambiguity this parser deliberately does not resolve.
fn parse_touched_files(bytes: &[u8]) -> Result<HashMap<String, Vec<String>>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    // With `-z`, git NUL-terminates each token; the header and its first
    // path share one token, separated by exactly one newline.
    let mut current_sha: Option<String> = None;
    for token in bytes.split(|byte| *byte == 0) {
        if token.is_empty() {
            continue;
        }
        if let Some(header) = token.strip_prefix(TOUCHED_HEADER_PREFIX) {
            if header.len() < 40 || !header[..40].iter().all(u8::is_ascii_hexdigit) {
                let lossy = String::from_utf8_lossy(header);
                let masked = mask_git_ingest(lossy.as_ref());
                let display = refs::truncate_chars(masked.as_ref(), 80);
                return Err(anyhow!(
                    "git log --name-only output contains a malformed commit header {:?}",
                    display
                ));
            }
            let sha = String::from_utf8_lossy(&header[..40]).into_owned();
            let files = map.entry(sha.clone()).or_default();
            if let Some(first_path) = header[40..].strip_prefix(b"\n") {
                if !first_path.is_empty() {
                    files.push(String::from_utf8_lossy(first_path).into_owned());
                }
            } else if !header[40..].is_empty() {
                // Anything after the SHA that does not start with the one
                // newline separator is a shape git never emits; guessing at
                // it could drop or fabricate a first path.
                return Err(anyhow!(
                    "git log --name-only header for commit {sha} carries a \
                     malformed first-path separator"
                ));
            }
            current_sha = Some(sha);
            continue;
        }
        let Some(sha) = &current_sha else {
            // Path bytes are attacker/repo-controlled, so the error snippet
            // is secret-masked the same way stored changed_paths are —
            // never raw token bytes in a log/error path.
            let lossy = String::from_utf8_lossy(token);
            let masked = mask_git_ingest(lossy.as_ref());
            let display = refs::truncate_chars(masked.as_ref(), 80);
            return Err(anyhow!(
                "git log --name-only output contains a path token before any \
                 commit header: {display:?}"
            ));
        };
        map.get_mut(sha)
            .expect("current SHA was inserted with its header")
            .push(String::from_utf8_lossy(token).into_owned());
    }
    Ok(map)
}

#[cfg(test)]
#[path = "ingest/touched_file_parser_tests.rs"]
mod touched_file_parser_tests;

#[path = "ingest/gh_page.rs"]
mod gh_page;
use gh_page::fetch_page;

#[cfg(test)]
#[path = "ingest/paging_tests.rs"]
mod paging_tests;

/// Issue #765: `GitLogError` classification + `recover_commit_snapshot`
/// retry loop (pure/synchronous). See
/// crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[cfg(test)]
#[path = "ingest/recovery_classifier_tests.rs"]
mod recovery_classifier_tests;

#[cfg(test)]
#[path = "ingest/truncation_tests.rs"]
mod truncation_tests;

/// PR #816: `resolve_id`/`resolve_project_id` LIKE-wildcard-injection
/// regression tests. See crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[cfg(test)]
#[path = "ingest/compact_prefix_resolver_tests.rs"]
mod compact_prefix_resolver_tests;

/// PR #816: `find_document_for_path` LIKE-escaping + exact-match-ordering
/// regression tests. See crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[cfg(test)]
#[path = "ingest/find_document_for_path_tests.rs"]
mod find_document_for_path_tests;

/// `load_code_modules_by_snapshot_path` ambiguity and error-surface
/// regression tests. See
/// crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[cfg(test)]
#[path = "ingest/module_index_loader_tests.rs"]
mod module_index_loader_tests;

#[cfg(test)]
#[path = "ingest/property_index_tests.rs"]
mod property_index_tests;

#[cfg(all(test, unix))]
#[path = "ingest/github_refusal_tests.rs"]
mod github_refusal_tests;
