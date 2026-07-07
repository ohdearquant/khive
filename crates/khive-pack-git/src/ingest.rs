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

/// Options for one ingest pass.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Local path to the git repository to walk.
    pub repo: PathBuf,
    /// The repo-anchor `project` entity — full UUID or an 8+ hex prefix.
    pub project: String,
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
}

/// Run one ingest pass: issues + PRs first (via `gh`, when available), then
/// commits (via local `git log`). PRs are ingested before commits so a
/// commit's `annotates` list can reference an already-created merging-PR
/// note (the generic `create` verb validates `annotates` targets exist
/// before it writes — see `khive-runtime::operations::create_note_inner`).
pub async fn run_ingest(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
) -> Result<IngestReport> {
    let mut report = IngestReport::default();

    let project_id = resolve_id(runtime, token, &opts.project)
        .await?
        .ok_or_else(|| anyhow!("--project {:?} did not resolve to an entity", opts.project))?;

    let mut merge_sha_to_pr: HashMap<String, Uuid> = HashMap::new();
    let mut number_to_pr: HashMap<u64, Uuid> = HashMap::new();

    // Graceful degradation covers both "gh is not on PATH" and "gh is present
    // but this repo has no usable GitHub remote" (e.g. a synthetic/local-only
    // repo) — either way, issues/PRs are skipped with a warning and commits
    // still ingest (ADR-088 §5). A hard `gh` failure must never abort the
    // whole pass.
    if gh_available(&opts.repo) {
        report.gh_available = true;
        match ingest_prs(
            runtime,
            token,
            registry,
            &opts.repo,
            project_id,
            &mut report,
            &mut merge_sha_to_pr,
            &mut number_to_pr,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => report
                .warnings
                .push(format!("gh pr list failed, skipping pull requests: {e}")),
        }
        if let Err(e) = ingest_issues(
            runtime,
            token,
            registry,
            &opts.repo,
            project_id,
            &mut report,
        )
        .await
        {
            report
                .warnings
                .push(format!("gh issue list failed, skipping issues: {e}"));
        }
    } else {
        report.gh_available = false;
        report.warnings.push(
            "gh CLI not found on PATH; skipped issues and pull requests — commits still ingest"
                .to_string(),
        );
    }

    ingest_commits(
        runtime,
        token,
        registry,
        &opts.repo,
        project_id,
        &merge_sha_to_pr,
        &number_to_pr,
        &mut report,
    )
    .await?;

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
/// (natural-key idempotence, scoped by kind + project so numbers from
/// different repos under the same project never collide).
async fn find_by_number(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    kind: &str,
    number: u64,
) -> Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM notes WHERE kind=?1 AND namespace=?2 \
                  AND deleted_at IS NULL AND json_extract(properties,'$.number')=?3 LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(kind.to_string()),
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Integer(number as i64),
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

/// Advance the `(project_id, kind)` cursor. Called only after the
/// corresponding batch has been fully written — a mid-batch failure leaves
/// the cursor at its prior value so the next pass safely re-ingests the
/// batch (natural-key dedupe makes the re-ingest a no-op for anything that
/// already landed).
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
        return Err(anyhow!(
            "git log failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
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
        return Err(anyhow!(
            "git log --name-only failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
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

/// Squash-merge subject suffix `"... (#123)"` -> `123`.
fn squash_merge_pr_number(subject: &str) -> Option<u64> {
    let trimmed = subject.trim_end();
    let close = trimmed.strip_suffix(')')?;
    let open = close.rfind("(#")?;
    close[open + 2..].parse::<u64>().ok()
}

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
) -> Result<()> {
    let since = read_cursor(runtime, project_id, "commits").await?;
    let commits = walk_commits(repo, since.as_deref())?;
    if commits.is_empty() {
        return Ok(());
    }
    let files_by_sha = touched_files(repo)?;

    let mut last_sha: Option<String> = since;
    for c in &commits {
        if find_commit_by_sha(runtime, token, &c.sha).await?.is_some() {
            report.commits_skipped_existing += 1;
            last_sha = Some(c.sha.clone());
            continue;
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
                    None => find_by_number(runtime, token, "pull_request", n).await?,
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

        registry
            .dispatch(
                "create",
                json!({
                    "kind": "commit",
                    "content": content,
                    "properties": properties,
                    "annotates": annotates,
                }),
            )
            .await
            .map_err(|e| anyhow!("create commit {}: {e}", c.sha))?;

        report.commits_ingested += 1;
        last_sha = Some(c.sha.clone());
    }

    if let Some(sha) = last_sha {
        write_cursor(runtime, project_id, "commits", &sha).await?;
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
) -> Result<()> {
    let since = read_cursor(runtime, project_id, "prs").await?;
    let raw = gh_json(
        repo,
        &[
            "pr",
            "list",
            "--state",
            "all",
            "--limit",
            "1000",
            "--json",
            "number,title,author,createdAt,mergedAt,closedAt,updatedAt,baseRefName,headRefName,mergeCommit,body",
        ],
    )?;
    let prs: Vec<GhPr> = serde_json::from_str(&raw).context("parsing gh pr list --json")?;

    let mut max_updated: Option<String> = since.clone();
    for pr in prs {
        let is_new = since
            .as_deref()
            .zip(pr.updated_at.as_deref())
            .map(|(cursor, updated)| updated > cursor)
            .unwrap_or(true);

        if let Some(existing) = find_by_number(runtime, token, "pull_request", pr.number).await? {
            number_to_pr.insert(pr.number, existing);
            if let Some(oid) = pr.merge_commit.as_ref().and_then(|m| m.oid.clone()) {
                merge_sha_to_pr.insert(oid, existing);
            }
            report.prs_skipped_existing += 1;
            if let Some(u) = &pr.updated_at {
                if max_updated
                    .as_deref()
                    .map(|m| u.as_str() > m)
                    .unwrap_or(true)
                {
                    max_updated = Some(u.clone());
                }
            }
            continue;
        }
        if !is_new {
            continue;
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
            "base": pr.base_ref_name,
            "head": pr.head_ref_name,
        });

        let result = registry
            .dispatch(
                "create",
                json!({
                    "kind": "pull_request",
                    "content": content,
                    "properties": properties,
                    "annotates": [project_id.to_string()],
                }),
            )
            .await
            .map_err(|e| anyhow!("create pull_request #{}: {e}", pr.number))?;

        if let Some(id) = result
            .get("id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            number_to_pr.insert(pr.number, id);
            if let Some(oid) = pr.merge_commit.and_then(|m| m.oid) {
                merge_sha_to_pr.insert(oid, id);
            }
        }
        report.prs_ingested += 1;
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

    if let Some(cursor) = max_updated {
        write_cursor(runtime, project_id, "prs", &cursor).await?;
    }
    Ok(())
}

async fn ingest_issues(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    repo: &Path,
    project_id: Uuid,
    report: &mut IngestReport,
) -> Result<()> {
    let since = read_cursor(runtime, project_id, "issues").await?;
    let raw = gh_json(
        repo,
        &[
            "issue",
            "list",
            "--state",
            "all",
            "--limit",
            "1000",
            "--json",
            "number,title,author,createdAt,closedAt,updatedAt,labels,stateReason,body",
        ],
    )?;
    let issues: Vec<GhIssue> =
        serde_json::from_str(&raw).context("parsing gh issue list --json")?;

    let mut max_updated: Option<String> = since.clone();
    for issue in issues {
        let is_new = since
            .as_deref()
            .zip(issue.updated_at.as_deref())
            .map(|(cursor, updated)| updated > cursor)
            .unwrap_or(true);

        if find_by_number(runtime, token, "issue", issue.number)
            .await?
            .is_some()
        {
            report.issues_skipped_existing += 1;
            if let Some(u) = &issue.updated_at {
                if max_updated
                    .as_deref()
                    .map(|m| u.as_str() > m)
                    .unwrap_or(true)
                {
                    max_updated = Some(u.clone());
                }
            }
            continue;
        }
        if !is_new {
            continue;
        }

        let raw_body = issue.body.unwrap_or_default();
        let content = secret_gate::mask_secrets(&raw_body).into_owned();
        let labels: Vec<String> = issue
            .labels
            .unwrap_or_default()
            .into_iter()
            .map(|l| l.name)
            .collect();
        let properties = json!({
            "number": issue.number,
            "title": issue.title,
            "author": issue.author.and_then(|a| a.login),
            "created_at": issue.created_at,
            "closed_at": issue.closed_at,
            "labels": labels,
            "state_reason": issue.state_reason,
        });

        registry
            .dispatch(
                "create",
                json!({
                    "kind": "issue",
                    "content": content,
                    "properties": properties,
                    "annotates": [project_id.to_string()],
                }),
            )
            .await
            .map_err(|e| anyhow!("create issue #{}: {e}", issue.number))?;

        report.issues_ingested += 1;
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

    if let Some(cursor) = max_updated {
        write_cursor(runtime, project_id, "issues", &cursor).await?;
    }
    Ok(())
}
