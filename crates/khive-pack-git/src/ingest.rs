//! Batch, cursor-based git-history ingester (ADR-088 §5).
//!
//! One-shot: walks local git history plus (optionally) `gh`-fetched issues
//! and pull requests, and writes `commit` / `issue` / `pull_request` notes
//! through the standard `create` verb (so `KindHook` validation and
//! `annotates` edge creation run exactly as they would for any other
//! caller). Reuses ADR-087's operational pattern (cursor table, secret
//! masking on ingest, cursor advances only on success) — NOT a daemon loop,
//! NOT a webhook, NOT a poller: one pass per invocation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use khive_runtime::{secret_gate, KhiveRuntime, NamespaceToken, VerbRegistry};
use khive_storage::types::{SqlStatement, SqlValue};

use crate::refs;

/// Which record kinds a `run_ingest` pass processes. `Default` selects all
/// three — the CLI's historical behavior and the `git.digest` verb's default
/// (ADR-088 Amendment 1).
#[derive(Debug, Clone, Copy)]
pub struct IngestInclude {
    pub commits: bool,
    pub issues: bool,
    pub pull_requests: bool,
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
    /// Local path to the git repository to walk.
    pub repo: PathBuf,
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
            project,
            max_items: None,
            include: IngestInclude::default(),
        }
    }
}

/// Bounds the number of new-record creation attempts across a `run_ingest`
/// pass (ADR-088 Amendment 1 `max_items`). Only creation attempts (success or
/// failure) consume budget — cheap natural-key "already exists" skips do not,
/// since they are not the work the bound exists to limit.
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

/// A newly created note this pass, retained so the post-ingestion reference-
/// extraction sweep (`link_references`) can resolve cross-references between
/// records created in the *same* pass regardless of ingestion order (PRs and
/// issues are ingested before commits) without re-reading them from storage.
struct NewRecordForRef {
    id: Uuid,
    text: String,
}

/// Outcome of one ingest pass. Serializable so CLI callers can emit it as JSON.
#[derive(Debug, Default, Serialize)]
pub struct IngestReport {
    pub commits_ingested: u64,
    pub commits_skipped_existing: u64,
    pub issues_ingested: u64,
    pub issues_skipped_existing: u64,
    pub prs_ingested: u64,
    pub prs_skipped_existing: u64,
    /// `false` when the `gh` CLI was not found on PATH — issues/PRs were
    /// skipped but commits still ingested (ADR-088 §5 graceful-absence rule).
    pub gh_available: bool,
    pub warnings: Vec<String>,
    /// `false` when `max_items` was exhausted before this pass reached the
    /// end of every included kind's history — callers loop until `true`
    /// (ADR-088 Amendment 1). Always `true` for an unbounded
    /// (`max_items: None`) pass.
    pub done: bool,
    /// The repo-anchor `project` entity id this pass resolved (or the
    /// verb-level caller created).
    pub project_id: Option<String>,
    /// `true` when the `git.digest` verb auto-created the `project` anchor
    /// because none was found (ADR-088 Amendment 1) — never set by
    /// `run_ingest` itself, only by the verb handler after it returns.
    pub project_created: bool,
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
}

/// Run one ingest pass: issues + PRs first (via `gh`, when available), then
/// commits (via local `git log`). PRs are ingested before commits so a
/// commit's `annotates` list can reference an already-created merging-PR
/// note (the generic `create` verb validates `annotates` targets exist
/// before it writes — see `khive-runtime::operations::create_note_inner`).
///
/// Delegates to `run_ingest_with_commit_recovery` with a recovery callback
/// that never repairs anything — the CLI and any local-path caller has no
/// disposable remote cache to repair (issue #765 self-heal is remote-URL
/// mode only, ADR-088 Amendment 1), so a classified commit-snapshot failure
/// here surfaces as an ordinary error, exactly as before this pass gained
/// recovery support.
pub async fn run_ingest(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
) -> Result<IngestReport> {
    run_ingest_with_commit_recovery(runtime, token, registry, opts, |_repo, _err| Ok(None)).await
}

/// Same one-shot ingest pass as `run_ingest`, but a classified missing-
/// promisor-object failure while loading the commit-history snapshot
/// (`GitLogError::is_missing_promisor_object`) is retried through `recover`
/// instead of aborting the whole pass (issue #765). Issues and PRs still run
/// exactly once regardless of whether recovery is later needed or invoked —
/// only commit-snapshot acquisition (`walk_commits` + `touched_files`) is
/// retried, inside this same invocation's `Budget`, `IngestReport`, PR/merge
/// maps, and reference candidates (`new_records`); a repair never resets or
/// replays any of them, and there is no second `run_ingest` pass hiding
/// behind this one.
pub(crate) async fn run_ingest_with_commit_recovery(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
    mut recover: impl FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>> + Send,
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

    // Graceful degradation covers both "gh is not on PATH" and "gh is present
    // but this repo has no usable GitHub remote" (e.g. a synthetic/local-only
    // repo) — either way, issues/PRs are skipped with a warning and commits
    // still ingest (ADR-088 §5). A hard `gh` failure must never abort the
    // whole pass.
    if opts.include.issues || opts.include.pull_requests {
        if gh_available(&opts.repo) {
            report.gh_available = true;
            if opts.include.pull_requests && !budget.exhausted() {
                match ingest_prs(
                    runtime,
                    token,
                    registry,
                    &opts.repo,
                    project_id,
                    &mut report,
                    &mut merge_sha_to_pr,
                    &mut number_to_pr,
                    &mut budget,
                    &mut new_records,
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => report
                        .warnings
                        .push(format!("gh pr list failed, skipping pull requests: {e}")),
                }
            }
            if opts.include.issues && !budget.exhausted() {
                if let Err(e) = ingest_issues(
                    runtime,
                    token,
                    registry,
                    &opts.repo,
                    project_id,
                    &mut report,
                    &mut budget,
                    &mut new_records,
                )
                .await
                {
                    report
                        .warnings
                        .push(format!("gh issue list failed, skipping issues: {e}"));
                }
            }
        } else {
            report.gh_available = false;
            report.warnings.push(
                "gh CLI not found on PATH; skipped issues and pull requests — commits still ingest"
                    .to_string(),
            );
        }
    }

    if opts.include.commits && !budget.exhausted() {
        ingest_commits(
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
            &mut recover,
        )
        .await?;
    }

    if budget.exhausted() {
        report.done = false;
    }

    link_references(
        runtime,
        token,
        registry,
        project_id,
        &new_records,
        &mut report,
    )
    .await;

    Ok(report)
}

/// Resolve a full UUID or an 8+ hex prefix to a full UUID, unfiltered by
/// namespace (matches the by-ID resolution contract used by `get`/`update`).
async fn resolve_id(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    raw: &str,
) -> Result<Option<Uuid>> {
    if let Ok(u) = Uuid::parse_str(raw) {
        return Ok(Some(u));
    }
    runtime
        .resolve_prefix_unfiltered(raw)
        .await
        .map_err(|e| anyhow!("{e}"))
}

/// Public wrapper for the `git.digest` verb handler, which needs to resolve
/// (but never auto-create) an explicitly supplied `project` argument the same
/// way `run_ingest` does.
pub async fn resolve_project_id(runtime: &KhiveRuntime, raw: &str) -> Result<Option<Uuid>> {
    if let Ok(u) = Uuid::parse_str(raw) {
        return Ok(Some(u));
    }
    runtime
        .resolve_prefix_unfiltered(raw)
        .await
        .map_err(|e| anyhow!("{e}"))
}

/// Find an existing `issue` or `pull_request` note by `properties.number`
/// within `project_id` — GitHub numbers a repository's issues and PRs from
/// one shared sequence, so a `#N` reference can resolve to either kind.
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

/// Post-ingestion sweep (ADR-088 Amendment 1 ingest enrichment): extract
/// GitHub reference-grammar mentions from every note created *this pass*
/// (commits, issues, PRs — order-independent, since all three are already in
/// `new_records` by the time this runs) and materialize `annotates` edges to
/// the referenced issue/PR note, carrying `ref_kind` ("closes" | "mentions")
/// as edge metadata. Fail-open throughout: a malformed or unresolvable
/// reference is skipped and counted, never aborts the pass.
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
            match registry
                .dispatch(
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

/// `true` when `gh` is on PATH and can run inside `repo` (a lightweight
/// `gh --version` probe — cheap and does not require network access).
fn gh_available(repo: &Path) -> bool {
    Command::new("gh")
        .arg("--version")
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Look up an existing `commit` note by its `properties.sha` (natural-key
/// idempotence — dedupe-before-create, matching the precedent used elsewhere
/// in this codebase for `json_extract`-keyed lookups).
async fn find_commit_by_sha(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    sha: &str,
) -> Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM notes WHERE kind='commit' AND namespace=?1 \
                  AND deleted_at IS NULL AND json_extract(properties,'$.sha')=?2 LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(sha.to_string()),
            ],
            label: Some("git_ingest_find_commit_by_sha".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| row_uuid(&r)))
}

/// Look up an existing `issue`/`pull_request` note by its `properties.number`
/// (natural-key idempotence, scoped by kind + namespace + `project_id` —
/// GitHub issue/PR numbers are repository-scoped, so a bare `kind`+`number`
/// filter would incorrectly collide two different repos' `#1`).
async fn find_by_number(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    kind: &str,
    project_id: Uuid,
    number: u64,
) -> Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM notes WHERE kind=?1 AND namespace=?2 \
                  AND deleted_at IS NULL AND json_extract(properties,'$.number')=?3 \
                  AND json_extract(properties,'$.project_id')=?4 LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(kind.to_string()),
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Integer(number as i64),
                SqlValue::Text(project_id.to_string()),
            ],
            label: Some("git_ingest_find_by_number".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| row_uuid(&r)))
}

fn row_uuid(row: &khive_storage::types::SqlRow) -> Option<Uuid> {
    match row.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    }
}

/// Find an existing `document` entity whose `properties.source_uri` or `name`
/// matches `path` (ADR-086 keying convention). Returns `None` when no match —
/// v0 never creates documents on the ingester's behalf (skip the edge).
async fn find_document_for_path(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    path: &str,
) -> Result<Option<Uuid>> {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(path);
    let like_pattern = format!("%{path}");
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='document' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND (json_extract(properties,'$.source_uri')=?2 \
                       OR json_extract(properties,'$.source_uri') LIKE ?3 \
                       OR name=?4) \
                  LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(path.to_string()),
                SqlValue::Text(like_pattern),
                SqlValue::Text(file_name.to_string()),
            ],
            label: Some("git_ingest_find_document_for_path".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| row_uuid(&r)))
}

/// Read the last-ingested cursor value for `(project_id, kind)`, if any.
async fn read_cursor(
    runtime: &KhiveRuntime,
    project_id: Uuid,
    kind: &str,
) -> Result<Option<String>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind=?2"
                .into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text(kind.to_string()),
            ],
            label: Some("git_ingest_read_cursor".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| match r.get("cursor_value") {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }))
}

/// Advance the `(project_id, kind)` cursor. Called once per section
/// (commits/prs/issues) after that section's loop finishes, with a value
/// that stops advancing at the first per-record create failure (see the
/// `cursor_stalled` handling in each `ingest_*` loop) — so the next pass
/// re-walks from before the failure and retries it, while records that
/// already landed (including ones ingested later in a stalled pass) are
/// no-ops via natural-key dedupe.
async fn write_cursor(
    runtime: &KhiveRuntime,
    project_id: Uuid,
    kind: &str,
    value: &str,
) -> Result<()> {
    let sql = runtime.sql();
    let mut w = sql.writer().await.map_err(|e| anyhow!("{e}"))?;
    w.execute(SqlStatement {
        sql: "INSERT INTO git_mirror_cursor(project_id, kind, cursor_value, updated_at) \
              VALUES(?1, ?2, ?3, ?4) \
              ON CONFLICT(project_id, kind) DO UPDATE SET \
                cursor_value=excluded.cursor_value, \
                updated_at=excluded.updated_at"
            .into(),
        params: vec![
            SqlValue::Text(project_id.to_string()),
            SqlValue::Text(kind.to_string()),
            SqlValue::Text(value.to_string()),
            SqlValue::Integer(Utc::now().timestamp_micros()),
        ],
        label: Some("git_ingest_write_cursor".into()),
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

// ── commits ─────────────────────────────────────────────────────────────────

const RECORD_SEP: char = '\u{1e}';
const FIELD_SEP: char = '\u{1f}';

struct RawCommit {
    sha: String,
    short_sha: String,
    author: String,
    author_email: String,
    committed_at: String,
    parents: Vec<String>,
    subject: String,
    body: String,
}

/// Which `git log` pass a classified failure came from (issue #765): the
/// two passes fail independently (`walk_commits`'s plain metadata pass can
/// succeed via cached commit data while `touched_files`'s `--name-only` pass
/// needs a tree that the promisor cache dropped, or vice versa), so recovery
/// needs to know which one to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitLogPhase {
    Metadata,
    TouchedFiles,
}

/// A non-zero-exit `git log` failure, carrying its phase and raw stderr so
/// `is_missing_promisor_object` can classify it without losing the
/// underlying diagnostic (surfaced verbatim in the final error when
/// recovery is unavailable or exhausted).
#[derive(Debug)]
pub(crate) struct GitLogError {
    phase: GitLogPhase,
    stderr: String,
}

impl std::fmt::Display for GitLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cmd = match self.phase {
            GitLogPhase::Metadata => "git log",
            GitLogPhase::TouchedFiles => "git log --name-only",
        };
        write!(f, "{cmd} failed: {}", self.stderr)
    }
}

impl std::error::Error for GitLogError {}

impl GitLogError {
    /// `true` for exactly the class of failure issue #765 authorizes
    /// self-healing for: a missing-object diagnostic that names a promisor
    /// remote. Deliberately narrow (ASCII-case-insensitive `promisor` plus
    /// either `not in the object database` or `missing object`) so ordinary
    /// auth/network/`bad object`/spawn/local-source failures are never
    /// treated as corrupt-cache and never trigger a destructive repair.
    pub(crate) fn is_missing_promisor_object(&self) -> bool {
        let lower = self.stderr.to_ascii_lowercase();
        lower.contains("promisor")
            && (lower.contains("not in the object database") || lower.contains("missing object"))
    }
}

/// Walk local git history via `git log` with a stable, machine-parseable
/// format (v0 choice per ADR-088 §5 — `git2`/`gix` are not workspace
/// dependencies today, so shelling out avoids a new heavy dependency).
fn walk_commits(repo: &Path, since_sha: Option<&str>) -> Result<Vec<RawCommit>> {
    // Raw control-byte separators embedded directly in the format string
    // (not git's `%xHH` escape syntax) — passed as a single argv element
    // (never through a shell), so the literal bytes survive intact and git's
    // pretty-format engine emits any non-`%` character verbatim.
    let format = format!("%H{FIELD_SEP}%h{FIELD_SEP}%an{FIELD_SEP}%ae{FIELD_SEP}%cI{FIELD_SEP}%P{FIELD_SEP}%s{FIELD_SEP}%b{RECORD_SEP}");
    let mut args = vec![
        "log".to_string(),
        "--reverse".to_string(),
        format!("--pretty=format:{format}"),
    ];
    if let Some(sha) = since_sha {
        args.push(format!("{sha}..HEAD"));
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .output()
        .context("spawning git log")?;
    if !output.status.success() {
        return Err(anyhow::Error::new(GitLogError {
            phase: GitLogPhase::Metadata,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut commits = Vec::new();
    for record in text.split(RECORD_SEP) {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.splitn(8, FIELD_SEP).collect();
        if fields.len() < 8 {
            continue;
        }
        let sha = fields[0].to_string();
        let short_sha = fields[1].to_string();
        let author = fields[2].to_string();
        let author_email = fields[3].to_string();
        let committed_at = fields[4].to_string();
        let parents = fields[5]
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let subject = fields[6].to_string();
        let body = fields[7].trim_end_matches('\n').to_string();
        commits.push(RawCommit {
            sha,
            short_sha,
            author,
            author_email,
            committed_at,
            parents,
            subject,
            body,
        });
    }
    Ok(commits)
}

/// `sha -> [touched paths]` for every commit in `repo`'s history, via a
/// separate `--name-only` pass (kept apart from `walk_commits`'s custom
/// `--pretty=format` — interleaving file-name lines with the metadata format
/// has no clean, unambiguous delimiter).
fn touched_files(repo: &Path) -> Result<HashMap<String, Vec<String>>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("log")
        .arg("--name-only")
        .arg(format!("--pretty=format:{RECORD_SEP}%H"))
        .output()
        .context("spawning git log --name-only")?;
    if !output.status.success() {
        return Err(anyhow::Error::new(GitLogError {
            phase: GitLogPhase::TouchedFiles,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for block in text.split(RECORD_SEP) {
        let mut lines = block.lines().filter(|l| !l.trim().is_empty());
        let Some(sha) = lines.next() else { continue };
        let files: Vec<String> = lines.map(str::to_string).collect();
        map.insert(sha.trim().to_string(), files);
    }
    Ok(map)
}

/// The two `git log` passes a commit-ingest phase needs, loaded together so
/// a classified failure in either one can be retried as a single unit
/// (issue #765).
struct CommitSnapshot {
    commits: Vec<RawCommit>,
    files_by_sha: HashMap<String, Vec<String>>,
}

/// Load one commit-history snapshot. Mirrors `ingest_commits`'s original
/// inline sequencing: `touched_files` (a second, unscoped `git log
/// --name-only` pass over the whole history) is skipped entirely when
/// `walk_commits` found no new commits, since there is nothing new to
/// annotate with touched paths.
fn load_commit_snapshot(repo: &Path, since_sha: Option<&str>) -> Result<CommitSnapshot> {
    let commits = walk_commits(repo, since_sha)?;
    if commits.is_empty() {
        return Ok(CommitSnapshot {
            commits,
            files_by_sha: HashMap::new(),
        });
    }
    let files_by_sha = touched_files(repo)?;
    Ok(CommitSnapshot {
        commits,
        files_by_sha,
    })
}

/// Which repair `RemoteCommitRecovery` (`handlers.rs`) performed, so
/// `recover_commit_snapshot` can report exactly one truthful success warning
/// once the commit phase completes (issue #765).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheRepairStrategy {
    Refetch,
    Reclone,
}

/// The repo path and strategy a `recover` callback used to repair a
/// classified `GitLogError` -- `recover_commit_snapshot` retries the
/// snapshot load against `repo` (the same cache slot for both strategies in
/// `cache.rs`, but callers are not required to keep it identical).
pub(crate) struct RecoveredRepo {
    pub(crate) repo: PathBuf,
    pub(crate) strategy: CacheRepairStrategy,
}

fn cache_repair_warning(strategy: CacheRepairStrategy) -> String {
    match strategy {
        CacheRepairStrategy::Refetch => {
            "repaired corrupt remote git cache by refetching missing promisor objects".to_string()
        }
        CacheRepairStrategy::Reclone => {
            "repaired corrupt remote git cache by replacing the owned clone".to_string()
        }
    }
}

/// Load a commit-history snapshot, retrying through `recover` when the
/// failure is a classified missing-promisor-object error (issue #765).
///
/// Bounded entirely by `recover`'s own return value: `Ok(Some(_))` retries
/// the snapshot load against the recovered repo path, `Ok(None)` surfaces
/// the original classified error (no more repair available), and any other
/// error (including an unclassified `GitLogError` or a non-`GitLogError`
/// failure) is returned immediately without ever calling `recover`. A later
/// repair attempt's strategy replaces the pending warning rather than
/// accumulating one per attempt, so exactly one success warning is ever
/// returned -- describing the *last* repair that was needed, not every one
/// tried.
fn recover_commit_snapshot(
    repo: &Path,
    since_sha: Option<&str>,
    mut recover: impl FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>>,
) -> Result<(CommitSnapshot, Option<String>)> {
    let mut repo_path = repo.to_path_buf();
    let mut recovery_warning: Option<String> = None;
    loop {
        match load_commit_snapshot(&repo_path, since_sha) {
            Ok(snapshot) => return Ok((snapshot, recovery_warning)),
            Err(e) => {
                let classified = e
                    .downcast_ref::<GitLogError>()
                    .filter(|g| g.is_missing_promisor_object());
                let Some(git_log_err) = classified else {
                    return Err(e);
                };
                match recover(&repo_path, git_log_err)? {
                    Some(recovered) => {
                        repo_path = recovered.repo;
                        recovery_warning = Some(cache_repair_warning(recovered.strategy));
                    }
                    None => return Err(e),
                }
            }
        }
    }
}

/// Squash-merge subject suffix `"... (#123)"` -> `123`.
fn squash_merge_pr_number(subject: &str) -> Option<u64> {
    let trimmed = subject.trim_end();
    let close = trimmed.strip_suffix(')')?;
    let open = close.rfind("(#")?;
    close[open + 2..].parse::<u64>().ok()
}

/// Max characters for the `name` field the amendment's readable-names rider
/// sets on newly ingested notes (issues/PRs: `"#<number> <title>"`; commits:
/// `"<short_sha> <subject>"`).
const NAME_MAX_CHARS: usize = 120;

#[allow(clippy::too_many_arguments)]
async fn ingest_commits(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    repo: &Path,
    project_id: Uuid,
    merge_sha_to_pr: &HashMap<String, Uuid>,
    number_to_pr: &HashMap<u64, Uuid>,
    report: &mut IngestReport,
    budget: &mut Budget,
    new_records: &mut Vec<NewRecordForRef>,
    recover: &mut (dyn FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>> + Send),
) -> Result<()> {
    let since = read_cursor(runtime, project_id, "commits").await?;
    let (snapshot, recovery_warning) = recover_commit_snapshot(repo, since.as_deref(), recover)?;
    let CommitSnapshot {
        commits,
        files_by_sha,
    } = snapshot;
    if commits.is_empty() {
        if let Some(warning) = recovery_warning {
            report.warnings.push(warning);
        }
        return Ok(());
    }

    // `cursor_stalled` freezes `last_sha` at the last contiguous successfully
    // processed commit: once a record fails to create, later records in this
    // same pass are still attempted (so a run surfaces every failure it can,
    // not just the first) but the cursor no longer advances past them. That
    // guarantees a failed record is retried — and its warning re-surfaced —
    // on every subsequent pass until it is fixed upstream, rather than being
    // silently skipped forever because the cursor moved past it. Records that
    // do succeed after a stall are still written (idempotent via the
    // sha natural key), so a retried pass never double-creates them.
    let mut last_sha: Option<String> = since;
    let mut cursor_stalled = false;
    // Parent SHA -> note id for commits created earlier THIS pass (walked
    // oldest-first) — combined with `find_commit_by_sha`'s DB lookup below,
    // this resolves parent edges regardless of which pass the parent landed
    // in.
    let mut local_sha_to_id: HashMap<String, Uuid> = HashMap::new();
    for c in &commits {
        if let Some(existing) = find_commit_by_sha(runtime, token, &c.sha).await? {
            local_sha_to_id.insert(c.sha.clone(), existing);
            report.commits_skipped_existing += 1;
            if !cursor_stalled {
                last_sha = Some(c.sha.clone());
            }
            continue;
        }

        if budget.exhausted() {
            break;
        }

        let raw_content = if c.body.trim().is_empty() {
            c.subject.clone()
        } else {
            format!("{}\n\n{}", c.subject, c.body)
        };
        let content = secret_gate::mask_secrets(&raw_content).into_owned();

        let mut annotates = vec![project_id.to_string()];

        if let Some(paths) = files_by_sha.get(&c.sha) {
            for p in paths {
                if !p.starts_with("docs/adr/") {
                    continue;
                }
                if let Some(doc_id) = find_document_for_path(runtime, token, p).await? {
                    annotates.push(doc_id.to_string());
                }
            }
        }

        // Merge-commit sha mapping and squash-merge suffix parsing are both
        // scoped to PRs discovered THIS pass; also fall back to a direct
        // by-number lookup so a commit can still resolve its merging PR when
        // that PR was ingested in an earlier pass (its note already exists,
        // but this run's `number_to_pr` in-memory map starts empty).
        let pr_id = match merge_sha_to_pr.get(&c.sha).copied() {
            Some(id) => Some(id),
            None => match squash_merge_pr_number(&c.subject) {
                Some(n) => match number_to_pr.get(&n).copied() {
                    Some(id) => Some(id),
                    None => find_by_number(runtime, token, "pull_request", project_id, n).await?,
                },
                None => None,
            },
        };
        if let Some(pr_id) = pr_id {
            annotates.push(pr_id.to_string());
        }

        let properties = json!({
            "sha": c.sha,
            "short_sha": c.short_sha,
            "author": c.author,
            "author_email": c.author_email,
            "committed_at": c.committed_at,
            "parents": c.parents,
        });

        let name = refs::truncate_chars(&format!("{} {}", c.short_sha, c.subject), NAME_MAX_CHARS);

        budget.try_consume();
        match registry
            .dispatch(
                "create",
                json!({
                    "kind": "commit",
                    "name": name,
                    "content": content,
                    "properties": properties,
                    "annotates": annotates,
                }),
            )
            .await
        {
            Ok(v) => {
                report.commits_ingested += 1;
                if !cursor_stalled {
                    last_sha = Some(c.sha.clone());
                }
                if let Some(id) = v
                    .get("id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    local_sha_to_id.insert(c.sha.clone(), id);
                    new_records.push(NewRecordForRef {
                        id,
                        text: content.clone(),
                    });
                    // Parent -> child `precedes` edges (ADR-088 Amendment 1
                    // ingest enrichment). Fail-open: an unresolved or
                    // failing parent link is skipped/warned, never aborts
                    // the pass.
                    for parent_sha in &c.parents {
                        let parent_id = match local_sha_to_id.get(parent_sha).copied() {
                            Some(pid) => Some(pid),
                            None => find_commit_by_sha(runtime, token, parent_sha).await?,
                        };
                        let Some(parent_id) = parent_id else {
                            continue;
                        };
                        if parent_id == id {
                            continue;
                        }
                        match registry
                            .dispatch(
                                "link",
                                json!({
                                    "source_id": parent_id.to_string(),
                                    "target_id": id.to_string(),
                                    "relation": "precedes",
                                }),
                            )
                            .await
                        {
                            Ok(_) => report.parent_edges_created += 1,
                            Err(e) => report.warnings.push(format!(
                                "linking parent {parent_sha} -> {} precedes: {e}",
                                c.sha
                            )),
                        }
                    }
                }
            }
            Err(e) => {
                report
                    .warnings
                    .push(format!("create commit {}: {e}", c.sha));
                cursor_stalled = true;
            }
        }
    }

    if let Some(sha) = last_sha {
        write_cursor(runtime, project_id, "commits", &sha).await?;
    }
    if let Some(warning) = recovery_warning {
        report.warnings.push(warning);
    }
    Ok(())
}

// ── issues + PRs (gh CLI) ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GhAuthor {
    login: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    author: Option<GhAuthor>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "closedAt")]
    closed_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    labels: Option<Vec<GhLabel>>,
    #[serde(rename = "stateReason")]
    state_reason: Option<String>,
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhMergeCommit {
    oid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhPr {
    number: u64,
    title: String,
    author: Option<GhAuthor>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
    #[serde(rename = "closedAt")]
    closed_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    #[serde(rename = "baseRefName")]
    base_ref_name: Option<String>,
    #[serde(rename = "headRefName")]
    head_ref_name: Option<String>,
    #[serde(rename = "mergeCommit")]
    merge_commit: Option<GhMergeCommit>,
    body: Option<String>,
}

fn gh_json(repo: &Path, args: &[&str]) -> Result<String> {
    // gh has no `-C` flag (unlike git) — repo targeting is via working directory.
    let output = Command::new("gh")
        .current_dir(repo)
        .args(args)
        .output()
        .context("spawning gh")?;
    if !output.status.success() {
        return Err(anyhow!(
            "gh {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Per-page fetch cap for both PR and issue paging (ADR-088 Amendment 1 fix
/// round, Issue High-1). `gh {pr,issue} list --search` is backed by GitHub's
/// search API, which never returns more than this many results for a single
/// query regardless of `--limit` — paging works around that ceiling by
/// advancing an `updated:>=` floor between calls, not by requesting more
/// than one page can hold.
const PAGE_LIMIT: usize = 1000;

/// What a paging loop should do after processing one fetched page. Pure and
/// unit-testable independent of `gh`, the database, or async machinery — the
/// entire "was the remote window proven exhausted" decision lives here
/// (ADR-088 Amendment 1 fix-round High-1: a single hard-coded `--limit 1000`
/// fetch could previously report `done: true` while a repo's remaining
/// PRs/issues past position 1000 were never seen).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PageOutcome {
    /// The page held fewer than `PAGE_LIMIT` items: the remote window is
    /// proven exhausted regardless of local budget state.
    WindowComplete,
    /// The page was full (`PAGE_LIMIT` items) and the local budget is
    /// exhausted: stop paging, but the window is NOT proven exhausted.
    StopBudgetExhausted,
    /// The page was full and the last item's `updated_at` did not advance
    /// past the current floor (more than `PAGE_LIMIT` records share one
    /// timestamp — an unresolvable pathological case): stop paging rather
    /// than loop forever. The window is NOT proven exhausted.
    StopFloorStalled,
    /// The page was full, the budget is not exhausted, and the floor
    /// advanced: fetch the next page starting at this floor.
    Continue(String),
}

fn decide_page_outcome(
    page_len: usize,
    current_floor: Option<&str>,
    last_updated_at: Option<&str>,
    budget_exhausted: bool,
) -> PageOutcome {
    if page_len < PAGE_LIMIT {
        return PageOutcome::WindowComplete;
    }
    if budget_exhausted {
        return PageOutcome::StopBudgetExhausted;
    }
    match last_updated_at {
        Some(next) if Some(next) != current_floor => PageOutcome::Continue(next.to_string()),
        _ => PageOutcome::StopFloorStalled,
    }
}

/// `PageOutcome::WindowComplete` is the only outcome under which `done` can
/// stay `true` on the local-budget question alone; every other outcome means
/// more remote records may exist past the last fetched page. Test-only
/// helper — production code matches on `PageOutcome` directly (see
/// `ingest_prs`/`ingest_issues`'s paging loops).
#[cfg(test)]
fn page_outcome_proves_window_complete(outcome: PageOutcome) -> bool {
    matches!(outcome, PageOutcome::WindowComplete)
}

fn search_query(floor: Option<&str>) -> String {
    match floor {
        Some(f) => format!("sort:updated-asc updated:>={f}"),
        None => "sort:updated-asc".to_string(),
    }
}

const PR_FIELDS: &str = "number,title,author,createdAt,mergedAt,closedAt,updatedAt,baseRefName,headRefName,mergeCommit,body";
const ISSUE_FIELDS: &str =
    "number,title,author,createdAt,closedAt,updatedAt,labels,stateReason,body";

fn fetch_pr_page(repo: &Path, floor: Option<&str>) -> Result<Vec<GhPr>> {
    let search = search_query(floor);
    let raw = gh_json(
        repo,
        &[
            "pr",
            "list",
            "--state",
            "all",
            "--search",
            search.as_str(),
            "--limit",
            "1000",
            "--json",
            PR_FIELDS,
        ],
    )?;
    serde_json::from_str(&raw).context("parsing gh pr list --json")
}

fn fetch_issue_page(repo: &Path, floor: Option<&str>) -> Result<Vec<GhIssue>> {
    let search = search_query(floor);
    let raw = gh_json(
        repo,
        &[
            "issue",
            "list",
            "--state",
            "all",
            "--search",
            search.as_str(),
            "--limit",
            "1000",
            "--json",
            ISSUE_FIELDS,
        ],
    )?;
    serde_json::from_str(&raw).context("parsing gh issue list --json")
}

#[allow(clippy::too_many_arguments)]
async fn ingest_prs(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    repo: &Path,
    project_id: Uuid,
    report: &mut IngestReport,
    merge_sha_to_pr: &mut HashMap<String, Uuid>,
    number_to_pr: &mut HashMap<u64, Uuid>,
    budget: &mut Budget,
    new_records: &mut Vec<NewRecordForRef>,
) -> Result<()> {
    let since = read_cursor(runtime, project_id, "prs").await?;

    // `cursor_stalled` mirrors `ingest_commits`: once one PR fails to create,
    // later PRs in this pass are still attempted (so every failure surfaces
    // in this pass's warnings), but `max_updated` no longer advances past the
    // stall point — the next pass re-fetches from before the failure and
    // retries it, while already-landed PRs are no-ops via the natural key.
    let mut max_updated: Option<String> = since.clone();
    let mut cursor_stalled = false;
    let mut floor = since.clone();
    let mut window_complete = true;

    'paging: loop {
        let mut page = fetch_pr_page(repo, floor.as_deref())?;
        let page_len = page.len();
        // Each page is already `sort:updated-asc` server-side, but `--search`
        // makes no hard ordering guarantee across ties — re-sort defensively
        // so the frozen-cursor invariant (records walked in nondecreasing
        // `updated_at` order) holds regardless. `is_new` below is inclusive
        // (`updated >= cursor`) for exactly the tie reason documented at
        // length in the pre-pagination version of this function (a
        // successful and a failing record sharing one `updated_at` must both
        // be re-examined next pass until the cursor moves past that tie).
        page.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        let last_updated_at = page.last().and_then(|pr| pr.updated_at.clone());

        for pr in page {
            let is_new = since
                .as_deref()
                .zip(pr.updated_at.as_deref())
                .map(|(cursor, updated)| updated >= cursor)
                .unwrap_or(true);

            if let Some(existing) =
                find_by_number(runtime, token, "pull_request", project_id, pr.number).await?
            {
                number_to_pr.insert(pr.number, existing);
                if let Some(oid) = pr.merge_commit.as_ref().and_then(|m| m.oid.clone()) {
                    merge_sha_to_pr.insert(oid, existing);
                }
                report.prs_skipped_existing += 1;
                if !cursor_stalled {
                    if let Some(u) = &pr.updated_at {
                        if max_updated
                            .as_deref()
                            .map(|m| u.as_str() > m)
                            .unwrap_or(true)
                        {
                            max_updated = Some(u.clone());
                        }
                    }
                }
                continue;
            }
            if !is_new {
                continue;
            }
            if budget.exhausted() {
                break;
            }

            let raw_body = pr.body.unwrap_or_default();
            let content = secret_gate::mask_secrets(&raw_body).into_owned();
            let properties = json!({
                "number": pr.number,
                "title": pr.title,
                "author": pr.author.and_then(|a| a.login),
                "created_at": pr.created_at,
                "merged_at": pr.merged_at,
                "closed_at": pr.closed_at,
                "base_ref": pr.base_ref_name,
                "head_ref": pr.head_ref_name,
                "project_id": project_id.to_string(),
            });
            let name =
                refs::truncate_chars(&format!("#{} {}", pr.number, pr.title), NAME_MAX_CHARS);

            budget.try_consume();
            let result = match registry
                .dispatch(
                    "create",
                    json!({
                        "kind": "pull_request",
                        "name": name,
                        "content": content,
                        "properties": properties,
                        "annotates": [project_id.to_string()],
                    }),
                )
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    report
                        .warnings
                        .push(format!("create pull_request #{}: {e}", pr.number));
                    cursor_stalled = true;
                    continue;
                }
            };

            if let Some(id) = result
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                number_to_pr.insert(pr.number, id);
                if let Some(oid) = pr.merge_commit.and_then(|m| m.oid) {
                    merge_sha_to_pr.insert(oid, id);
                }
                new_records.push(NewRecordForRef {
                    id,
                    text: content.clone(),
                });
            }
            report.prs_ingested += 1;
            if !cursor_stalled {
                if let Some(u) = &pr.updated_at {
                    if max_updated
                        .as_deref()
                        .map(|m| u.as_str() > m)
                        .unwrap_or(true)
                    {
                        max_updated = Some(u.clone());
                    }
                }
            }
        }

        match decide_page_outcome(
            page_len,
            floor.as_deref(),
            last_updated_at.as_deref(),
            budget.exhausted(),
        ) {
            PageOutcome::WindowComplete => break 'paging,
            PageOutcome::StopBudgetExhausted | PageOutcome::StopFloorStalled => {
                window_complete = false;
                break 'paging;
            }
            PageOutcome::Continue(next_floor) => floor = Some(next_floor),
        }
    }

    if !window_complete {
        // The remote window may hold more PRs than this pass ever fetched
        // (ADR-088 Amendment 1 fix-round High-1) — the local budget alone is
        // not a complete signal; report `done = false` regardless of budget
        // state so the caller's resume loop keeps going.
        report.done = false;
    }

    if let Some(cursor) = max_updated {
        write_cursor(runtime, project_id, "prs", &cursor).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn ingest_issues(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    repo: &Path,
    project_id: Uuid,
    report: &mut IngestReport,
    budget: &mut Budget,
    new_records: &mut Vec<NewRecordForRef>,
) -> Result<()> {
    let since = read_cursor(runtime, project_id, "issues").await?;

    // `cursor_stalled` mirrors `ingest_commits`/`ingest_prs`: a per-record
    // create failure is aggregated as a warning and later records in this
    // pass are still attempted, but `max_updated` freezes at the stall point
    // so the next pass retries the failed record instead of skipping it
    // forever; already-landed records are no-ops via the natural key.
    let mut max_updated: Option<String> = since.clone();
    let mut cursor_stalled = false;
    let mut floor = since.clone();
    let mut window_complete = true;

    'paging: loop {
        let mut page = fetch_issue_page(repo, floor.as_deref())?;
        let page_len = page.len();
        // See `ingest_prs`: the frozen-cursor retry guarantee requires
        // walking records in nondecreasing updated_at order, which `--search
        // sort:updated-asc` does not itself guarantee across ties — sort
        // defensively.
        page.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        let last_updated_at = page.last().and_then(|i| i.updated_at.clone());

        for issue in page {
            let is_new = since
                .as_deref()
                .zip(issue.updated_at.as_deref())
                .map(|(cursor, updated)| updated >= cursor)
                .unwrap_or(true);

            if find_by_number(runtime, token, "issue", project_id, issue.number)
                .await?
                .is_some()
            {
                report.issues_skipped_existing += 1;
                if !cursor_stalled {
                    if let Some(u) = &issue.updated_at {
                        if max_updated
                            .as_deref()
                            .map(|m| u.as_str() > m)
                            .unwrap_or(true)
                        {
                            max_updated = Some(u.clone());
                        }
                    }
                }
                continue;
            }
            if !is_new {
                continue;
            }
            if budget.exhausted() {
                break;
            }

            let raw_body = issue.body.unwrap_or_default();
            let content = secret_gate::mask_secrets(&raw_body).into_owned();
            let labels: Vec<String> = issue
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect();
            let mut properties = json!({
                "number": issue.number,
                "title": issue.title,
                "author": issue.author.and_then(|a| a.login),
                "created_at": issue.created_at,
                "closed_at": issue.closed_at,
                "labels": labels,
                "project_id": project_id.to_string(),
            });
            // gh reports stateReason as "" for open issues and UPPERCASE enum values
            // (NOT_PLANNED) for closed ones; the kind hook governs any PRESENT value
            // against the lowercase GitHub stateReason enum, so normalize case and encode
            // "open / no reason" as absent.
            if let Some(reason) = issue.state_reason.as_deref().filter(|r| !r.is_empty()) {
                properties["state_reason"] = json!(reason.to_ascii_lowercase());
            }
            let name = refs::truncate_chars(
                &format!("#{} {}", issue.number, issue.title),
                NAME_MAX_CHARS,
            );

            budget.try_consume();
            let result = match registry
                .dispatch(
                    "create",
                    json!({
                        "kind": "issue",
                        "name": name,
                        "content": content,
                        "properties": properties,
                        "annotates": [project_id.to_string()],
                    }),
                )
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    report
                        .warnings
                        .push(format!("create issue #{}: {e}", issue.number));
                    cursor_stalled = true;
                    continue;
                }
            };
            if let Some(id) = result
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                new_records.push(NewRecordForRef {
                    id,
                    text: content.clone(),
                });
            }

            report.issues_ingested += 1;
            if !cursor_stalled {
                if let Some(u) = &issue.updated_at {
                    if max_updated
                        .as_deref()
                        .map(|m| u.as_str() > m)
                        .unwrap_or(true)
                    {
                        max_updated = Some(u.clone());
                    }
                }
            }
        }

        match decide_page_outcome(
            page_len,
            floor.as_deref(),
            last_updated_at.as_deref(),
            budget.exhausted(),
        ) {
            PageOutcome::WindowComplete => break 'paging,
            PageOutcome::StopBudgetExhausted | PageOutcome::StopFloorStalled => {
                window_complete = false;
                break 'paging;
            }
            PageOutcome::Continue(next_floor) => floor = Some(next_floor),
        }
    }

    if !window_complete {
        report.done = false;
    }

    if let Some(cursor) = max_updated {
        write_cursor(runtime, project_id, "issues", &cursor).await?;
    }
    Ok(())
}

#[cfg(test)]
mod paging_tests {
    use super::*;

    #[test]
    fn search_query_omits_updated_qualifier_with_no_floor() {
        assert_eq!(search_query(None), "sort:updated-asc");
    }

    #[test]
    fn search_query_includes_inclusive_updated_floor() {
        assert_eq!(
            search_query(Some("2024-01-01T00:00:00Z")),
            "sort:updated-asc updated:>=2024-01-01T00:00:00Z"
        );
    }

    #[test]
    fn short_page_proves_window_complete_regardless_of_budget() {
        let outcome = decide_page_outcome(42, None, Some("2024-01-01T00:00:00Z"), false);
        assert_eq!(outcome, PageOutcome::WindowComplete);
        assert!(page_outcome_proves_window_complete(outcome));

        // Even a page that runs out of budget mid-way is still a proof of
        // completeness if the page itself was short — the loop always
        // finishes sorting/processing the whole (short) page first.
        let outcome = decide_page_outcome(0, None, None, true);
        assert_eq!(outcome, PageOutcome::WindowComplete);
    }

    /// This is the exact ADR-088 Amendment 1 fix-round High-1 scenario: a
    /// full (`PAGE_LIMIT`-sized) page came back, but the local budget was
    /// NOT exhausted (e.g. every record in the page already existed and
    /// consumed no budget) and paging is still forced to stop because the
    /// floor didn't move. `done` must be false here — the remote window is
    /// not proven exhausted just because the local budget wasn't hit.
    #[test]
    fn full_page_with_stalled_floor_is_not_window_complete_even_with_budget_left() {
        let outcome = decide_page_outcome(PAGE_LIMIT, Some("X"), Some("X"), false);
        assert_eq!(outcome, PageOutcome::StopFloorStalled);
        assert!(!page_outcome_proves_window_complete(outcome));
    }

    #[test]
    fn full_page_with_advancing_floor_and_budget_left_continues() {
        let outcome = decide_page_outcome(PAGE_LIMIT, Some("A"), Some("B"), false);
        assert_eq!(outcome, PageOutcome::Continue("B".to_string()));
        assert!(!page_outcome_proves_window_complete(outcome));
    }

    #[test]
    fn full_page_with_exhausted_budget_stops_without_proving_completeness() {
        let outcome = decide_page_outcome(PAGE_LIMIT, Some("A"), Some("B"), true);
        assert_eq!(outcome, PageOutcome::StopBudgetExhausted);
        assert!(!page_outcome_proves_window_complete(outcome));
    }

    #[test]
    fn full_page_with_no_updated_at_stalls_rather_than_looping_forever() {
        let outcome = decide_page_outcome(PAGE_LIMIT, Some("A"), None, false);
        assert_eq!(outcome, PageOutcome::StopFloorStalled);
    }
}

/// Issue #765: `GitLogError` classification and the `recover_commit_snapshot`
/// retry loop. Pure/synchronous (no runtime, no database) -- these fields
/// are private to this module, so this lives here rather than in the
/// sibling `recovery_tests` module (which drives the DB-backed acceptance
/// scenarios through the `pub(crate)` surface instead).
#[cfg(test)]
mod recovery_classifier_tests {
    use super::*;

    fn err(phase: GitLogPhase, stderr: &str) -> GitLogError {
        GitLogError {
            phase,
            stderr: stderr.to_string(),
        }
    }

    const REAL_WORLD_MESSAGE: &str = "fatal: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef is in \
         the commit graph file, but not in the object database\nfatal: unable to parse commit: \
         deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\nfatal: could not fetch from promisor remote";

    #[test]
    fn classifies_real_world_missing_promisor_object_message_on_either_phase() {
        assert!(err(GitLogPhase::TouchedFiles, REAL_WORLD_MESSAGE).is_missing_promisor_object());
        assert!(err(GitLogPhase::Metadata, REAL_WORLD_MESSAGE).is_missing_promisor_object());
    }

    #[test]
    fn classifies_missing_object_wording_case_insensitively() {
        assert!(err(
            GitLogPhase::TouchedFiles,
            "FATAL: MISSING OBJECT abc123; PROMISOR remote unavailable"
        )
        .is_missing_promisor_object());
    }

    #[test]
    fn does_not_classify_bad_object_without_promisor() {
        assert!(!err(GitLogPhase::Metadata, "fatal: bad object HEAD").is_missing_promisor_object());
    }

    #[test]
    fn does_not_classify_auth_or_network_failures() {
        assert!(!err(
            GitLogPhase::Metadata,
            "fatal: Authentication failed for 'https://example.com/org/repo.git/'"
        )
        .is_missing_promisor_object());
        assert!(!err(
            GitLogPhase::TouchedFiles,
            "fatal: unable to access 'https://example.com/org/repo.git/': Could not resolve host"
        )
        .is_missing_promisor_object());
    }

    #[test]
    fn does_not_classify_promisor_mention_without_missing_object_wording() {
        // "promisor" alone (e.g. a config-dump or unrelated log line) must
        // not be treated as proof of corruption -- both keyword classes are
        // required.
        assert!(!err(
            GitLogPhase::Metadata,
            "fatal: promisor remote configured but unreachable"
        )
        .is_missing_promisor_object());
    }

    /// A healthy repo: the snapshot loads on the first try, no `recover`
    /// call, no warning.
    #[test]
    fn recover_commit_snapshot_returns_no_warning_when_healthy() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_commit(dir.path());
        let mut recover_calls = 0;
        let (snapshot, warning) = recover_commit_snapshot(dir.path(), None, |_repo, _err| {
            recover_calls += 1;
            Ok(None)
        })
        .expect("healthy repo loads");
        assert_eq!(snapshot.commits.len(), 1);
        assert_eq!(warning, None);
        assert_eq!(recover_calls, 0);
    }

    /// An unclassified `git log` failure (nonexistent repo path -- a spawn-
    /// level/`bad object`-shaped failure, not a promisor one) must never
    /// reach `recover` at all, and must propagate as-is.
    #[test]
    fn recover_commit_snapshot_never_calls_recover_for_unclassified_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Not a git repo at all -- `git log` fails with a plain spawn/repo
        // error, not a classified promisor one.
        let mut recover_calls = 0;
        let result = recover_commit_snapshot(dir.path(), None, |_repo, _err| {
            recover_calls += 1;
            Ok(Some(RecoveredRepo {
                repo: dir.path().to_path_buf(),
                strategy: CacheRepairStrategy::Refetch,
            }))
        });
        assert!(result.is_err(), "a non-repo path must fail to load");
        assert_eq!(
            recover_calls, 0,
            "an unclassified failure must never invoke recover"
        );
    }

    fn init_repo_with_commit(repo: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        std::fs::write(repo.join("a.txt"), b"hello").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "initial"]);
    }
}
