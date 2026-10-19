//! One-shot, per-project historical commit annotation reconciliation (#3532).
//! This is an operator API, not a pack verb.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use khive_runtime::{KhiveRuntime, NamespaceToken};
mod annotation;
use annotation::{link_annotation_if_absent, AnnotationGuard, AnnotationOutcome, CursorValue};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ingest_cursor::snapshot;
use crate::source::{parse_source, repo_identity, DigestSource};

const CURSOR_MAX_BYTES: i64 = 8 * 1024;
const MAX_ACKNOWLEDGED_SHAS: usize = 250_000;
const DIAGNOSTIC_CAP: usize = 16;
const WRITE_BATCH: usize = 64;
const READ_BATCH: usize = 900;

#[derive(Clone, Debug)]
pub struct ReconcileOptions {
    pub repo: PathBuf,
    pub project_id: Uuid,
    pub frozen_tip: String,
    pub apply_preview_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReconcileCounts {
    pub acknowledged_shas_examined: u64,
    pub live_note_hits: u64,
    pub live_project_edges: u64,
    pub repairable_missing_links: u64,
    pub tombstones_skipped: u64,
    pub missing_notes: u64,
    pub deleted_notes: u64,
    pub ambiguous_notes: u64,
    pub coverage_errors: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ApplyCounts {
    pub attempted: u64,
    pub created: u64,
    pub raced_live: u64,
    pub raced_tombstone: u64,
    pub failures: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReconcileReport {
    pub project_id: Uuid,
    pub namespace: String,
    pub source: String,
    pub frozen_tip: String,
    pub preview_id: String,
    pub complete_coverage: bool,
    pub counts: ReconcileCounts,
    pub diagnostics: Vec<String>,
    pub apply: Option<ApplyCounts>,
    pub after_apply: Option<ReconcileCounts>,
    pub cursors_unchanged: bool,
    pub success: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct CursorRow {
    kind: String,
    updated_at: i64,
    value_type: String,
    value: Option<Vec<u8>>,
}

struct Preview {
    report: ReconcileReport,
    cursor: Vec<CursorRow>,
    candidates: Vec<(String, Uuid)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitCheckpoint {
    version: u8,
    namespace: String,
    base_cursor: Option<String>,
    snapshot_head: String,
    last_completed_sha: String,
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn cursor_snapshot(runtime: &KhiveRuntime, project_id: Uuid) -> Result<Vec<CursorRow>> {
    let mut reader = runtime.sql().reader().await?;
    let rows = reader
        .query_all(snapshot::statement(
            &project_id.to_string(),
            "commits",
            "commits_checkpoint",
            CURSOR_MAX_BYTES,
            "git_annotation_repair_cursor_snapshot",
        ))
        .await?;
    rows.iter().map(decode_cursor_row).collect()
}

fn decode_cursor_row(row: &SqlRow) -> Result<CursorRow> {
    let kind = row
        .text("kind")
        .map_err(|_| anyhow!("stored kind has an invalid type"))?
        .to_owned();
    let value_type = row
        .text("value_type")
        .map_err(|_| anyhow!("stored value_type has an invalid type"))?
        .to_owned();
    let value_bytes = snapshot::value_bytes(row)
        .map_err(|_| anyhow!("stored cursor length has an invalid type"))?;
    if value_bytes.is_some_and(|size| size > CURSOR_MAX_BYTES) {
        bail!("stored {kind} cursor exceeds the repair size limit");
    }
    let value = snapshot::value(row)
        .map_err(|_| anyhow!("stored cursor value has an invalid type"))?
        .map(|value| value.to_vec());
    Ok(CursorRow {
        kind,
        updated_at: row
            .i64("updated_at")
            .map_err(|_| anyhow!("stored updated_at has an invalid type"))?,
        value_type,
        value,
    })
}

fn cursor_text<'a>(rows: &'a [CursorRow], kind: &str) -> Result<Option<&'a str>> {
    let Some(row) = rows.iter().find(|row| row.kind == kind) else {
        return Ok(None);
    };
    if row.value_type != "text" {
        bail!("stored {kind} cursor is not text");
    }
    let value = row
        .value
        .as_deref()
        .ok_or_else(|| anyhow!("null {kind} cursor"))?;
    Ok(Some(
        std::str::from_utf8(value).context("cursor is not UTF-8")?,
    ))
}

fn annotation_guard(
    rows: &[CursorRow],
    expected_sha: &str,
    source_identity: &str,
) -> Result<AnnotationGuard> {
    let value = |kind: &str| -> Result<CursorValue> {
        let row = rows
            .iter()
            .find(|row| row.kind == kind)
            .ok_or_else(|| anyhow!("{kind} cursor is absent"))?;
        if row.value_type != "text" {
            bail!("{kind} cursor is not text");
        }
        Ok(CursorValue {
            value: row
                .value
                .clone()
                .ok_or_else(|| anyhow!("null {kind} cursor"))?,
            updated_at: row.updated_at,
        })
    };
    Ok(AnnotationGuard {
        expected_sha: expected_sha.into(),
        source_identity: source_identity.into(),
        commits: value("commits")?,
        checkpoint: value("commits_checkpoint")?,
    })
}

fn git_command(repo: &Path, args: &[&str]) -> Result<Command> {
    Ok(crate::local_git::git_command(
        Path::new("git"),
        repo,
        args,
        None,
    )?)
}

fn git_status(repo: &Path, args: &[&str]) -> Result<bool> {
    let status = git_command(repo, args)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("starting bounded git history check")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git history check failed"),
    }
}

fn git_commit_exists(repo: &Path, sha: &str) -> Result<bool> {
    if !oid(sha) {
        return Ok(false);
    }
    let output = git_command(repo, &["cat-file", "-t", sha])?
        .stdin(Stdio::null())
        .output()
        .context("checking frozen git object")?;
    Ok(output.status.success() && output.stdout == b"commit\n")
}

fn git_repository_is_shallow(repo: &Path) -> Result<bool> {
    let output = git_command(repo, &["rev-parse", "--is-shallow-repository"])?
        .stdin(Stdio::null())
        .output()
        .context("checking repository history depth")?;
    if !output.status.success() {
        bail!("repository history depth check failed");
    }
    match output.stdout.as_slice() {
        b"true\n" => Ok(true),
        b"false\n" => Ok(false),
        _ => bail!("repository history depth check returned an invalid result"),
    }
}

fn acknowledged_prefix(repo: &Path, checkpoint: &CommitCheckpoint) -> Result<Vec<String>> {
    use std::io::{BufRead, BufReader};
    let mut child = git_command(
        repo,
        &[
            "log",
            "--reverse",
            "--topo-order",
            "--format=%H",
            &checkpoint.snapshot_head,
            "--",
        ],
    )?
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .context("walking frozen commit history")?;
    let stdout = child.stdout.take().context("missing git log output")?;
    let mut shas = Vec::new();
    let mut position = None;
    for line in BufReader::new(stdout).lines() {
        let sha = line.context("reading frozen commit history")?;
        if !oid(&sha) || shas.len() == MAX_ACKNOWLEDGED_SHAS {
            let _ = child.kill();
            let _ = child.wait();
            bail!("frozen commit walk is malformed or exceeds repair bound");
        }
        if sha == checkpoint.last_completed_sha {
            position = Some(shas.len());
        }
        shas.push(sha);
    }
    let status = child.wait().context("waiting for frozen commit walk")?;
    if !status.success() {
        bail!("frozen commit walk failed");
    }
    let position =
        position.ok_or_else(|| anyhow!("checkpoint position is absent from frozen walk"))?;
    shas.truncate(position + 1);
    Ok(shas)
}

async fn validate_project(
    runtime: &KhiveRuntime,
    namespace: &str,
    project_id: Uuid,
    source_identity: &str,
) -> Result<()> {
    let mut reader = runtime.sql().reader().await?;
    let found = reader
        .query_scalar(SqlStatement {
            sql: crate::sql::sql!("annotation_repair_project_select").into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text(namespace.into()),
                SqlValue::Text(source_identity.into()),
            ],
            label: Some("git_annotation_repair_project".into()),
        })
        .await?;
    if !matches!(found, Some(SqlValue::Integer(1))) {
        bail!("project is not a live anchor for the selected namespace and repository source");
    }
    Ok(())
}

fn diagnostic(report: &mut ReconcileReport, message: impl Into<String>) {
    if report.diagnostics.len() < DIAGNOSTIC_CAP {
        report.diagnostics.push(message.into());
    }
}

#[derive(Clone, Copy)]
enum AcknowledgedNote {
    Missing,
    Ambiguous,
    Deleted,
    Live(Uuid),
}

// Bound returned note rows to three per SHA and retain acknowledged order when
// hashing/classifying. The reader and per-link apply guards remain caller-owned.
async fn inspect_acknowledged(
    reader: &mut dyn khive_storage::SqlReader,
    namespace: &str,
    project_id: Uuid,
    shas: &[String],
    report: &mut ReconcileReport,
    hasher: &mut blake3::Hasher,
) -> Result<Vec<(String, Uuid)>> {
    let mut candidates = Vec::new();
    for chunk in shas.chunks(READ_BATCH) {
        let rows = reader
            .query_all(SqlStatement {
                sql: crate::sql::sql!("annotation_repair_commit_notes_select").into(),
                params: vec![
                    SqlValue::Text(namespace.into()),
                    SqlValue::Text(serde_json::to_string(chunk)?),
                ],
                label: Some("git_annotation_repair_note".into()),
            })
            .await?;
        let mut by_sha = std::collections::HashMap::<String, Vec<_>>::new();
        for row in rows {
            by_sha
                .entry(row.text("sha")?.to_owned())
                .or_default()
                .push(row);
        }
        let mut notes = Vec::with_capacity(chunk.len());
        let mut ids = Vec::new();
        for sha in chunk {
            let rows = by_sha.get(sha).map(Vec::as_slice).unwrap_or(&[]);
            notes.push(if rows.is_empty() {
                AcknowledgedNote::Missing
            } else if rows.len() != 1 {
                AcknowledgedNote::Ambiguous
            } else if !matches!(rows[0].get("deleted_at"), Some(SqlValue::Null)) {
                AcknowledgedNote::Deleted
            } else {
                let id = match rows[0].get("id") {
                    Some(SqlValue::Uuid(id)) => *id,
                    Some(SqlValue::Text(id)) => {
                        Uuid::parse_str(id).context("stored commit note has an invalid id")?
                    }
                    _ => bail!("stored commit note has an invalid id"),
                };
                ids.push(id.to_string());
                AcknowledgedNote::Live(id)
            });
        }
        let mut edges = std::collections::HashMap::new();
        if !ids.is_empty() {
            for row in reader
                .query_all(SqlStatement {
                    sql: crate::sql::sql!("annotation_repair_project_edge_select").into(),
                    params: vec![
                        SqlValue::Text(namespace.into()),
                        SqlValue::Text(serde_json::to_string(&ids)?),
                        SqlValue::Text(project_id.to_string()),
                    ],
                    label: Some("git_annotation_repair_edge".into()),
                })
                .await?
            {
                // SQL compares canonical lookup IDs to stored source_id exactly.
                // Do not parse/normalize stored edge IDs and widen that match.
                edges.insert(
                    row.text("source_id")?.to_owned(),
                    matches!(row.get("deleted_at"), Some(SqlValue::Null)),
                );
            }
        }
        for (sha, note) in chunk.iter().zip(notes) {
            report.counts.acknowledged_shas_examined += 1;
            let class = match note {
                AcknowledgedNote::Missing => {
                    report.counts.missing_notes += 1;
                    "missing_note"
                }
                AcknowledgedNote::Ambiguous => {
                    report.counts.ambiguous_notes += 1;
                    "ambiguous_note"
                }
                AcknowledgedNote::Deleted => {
                    report.counts.deleted_notes += 1;
                    "deleted_note"
                }
                AcknowledgedNote::Live(note_id) => {
                    hasher.update(note_id.as_bytes());
                    report.counts.live_note_hits += 1;
                    match edges.get(&note_id.to_string()) {
                        Some(true) => {
                            report.counts.live_project_edges += 1;
                            "live_edge"
                        }
                        Some(false) => {
                            report.counts.tombstones_skipped += 1;
                            "tombstone"
                        }
                        None => {
                            report.counts.repairable_missing_links += 1;
                            candidates.push((sha.clone(), note_id));
                            "missing_link"
                        }
                    }
                }
            };
            hasher.update(&serde_json::to_vec(&(sha, class))?);
        }
    }
    Ok(candidates)
}

async fn preview(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    repo: &Path,
    source_identity: &str,
    project_id: Uuid,
    frozen_tip: &str,
) -> Result<Preview> {
    let namespace = token.namespace().as_str();
    validate_project(runtime, namespace, project_id, source_identity).await?;
    let before = cursor_snapshot(runtime, project_id).await?;
    let mut report = ReconcileReport {
        project_id,
        namespace: namespace.into(),
        source: repo.display().to_string(),
        frozen_tip: frozen_tip.into(),
        preview_id: String::new(),
        complete_coverage: false,
        counts: ReconcileCounts::default(),
        diagnostics: Vec::new(),
        apply: None,
        after_apply: None,
        cursors_unchanged: true,
        success: false,
    };
    let mut candidates = Vec::new();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&serde_json::to_vec(&(
        project_id,
        namespace,
        repo.to_string_lossy(),
        source_identity,
        frozen_tip,
        &before,
    ))?);
    let coverage = (|| -> Result<Vec<String>> {
        if git_repository_is_shallow(repo)? {
            bail!("shallow repository cannot prove complete acknowledged history");
        }
        if !git_commit_exists(repo, frozen_tip)? {
            bail!("frozen tip is unavailable as a commit");
        }
        if !git_status(repo, &["merge-base", "--is-ancestor", frozen_tip, "HEAD"])? {
            bail!("frozen tip is no longer on source HEAD history");
        }
        let cursor =
            cursor_text(&before, "commits")?.ok_or_else(|| anyhow!("commits cursor is absent"))?;
        let progress = cursor_text(&before, "commits_checkpoint")?
            .ok_or_else(|| anyhow!("commits checkpoint is absent"))?;
        let checkpoint: CommitCheckpoint =
            serde_json::from_str(progress).context("malformed commits checkpoint")?;
        if checkpoint.version != 1
            || checkpoint.namespace != namespace
            || !oid(&checkpoint.snapshot_head)
            || !oid(&checkpoint.last_completed_sha)
            || cursor != checkpoint.last_completed_sha
        {
            bail!("checkpoint cannot prove the complete acknowledged prefix");
        }
        if checkpoint.base_cursor.is_some() {
            bail!("non-null base_cursor lacks a recorded earlier walk");
        }
        if !git_commit_exists(repo, &checkpoint.snapshot_head)?
            || !git_status(
                repo,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &checkpoint.snapshot_head,
                    frozen_tip,
                ],
            )?
        {
            bail!("checkpoint snapshot is unavailable or diverged from frozen tip");
        }
        acknowledged_prefix(repo, &checkpoint)
    })();
    match coverage {
        Err(error) => {
            report.counts.coverage_errors = 1;
            diagnostic(&mut report, format!("incomplete coverage: {error}"));
        }
        Ok(shas) => {
            report.complete_coverage = true;
            let mut reader = runtime.sql().reader().await?;
            candidates = inspect_acknowledged(
                reader.as_mut(),
                namespace,
                project_id,
                &shas,
                &mut report,
                &mut hasher,
            )
            .await?;
        }
    }
    let after = cursor_snapshot(runtime, project_id).await?;
    if after != before {
        report.cursors_unchanged = false;
        report.complete_coverage = false;
        report.counts.coverage_errors += 1;
        diagnostic(&mut report, "cursor rows changed during preview");
    }
    report.preview_id = hasher.finalize().to_hex().to_string();
    Ok(Preview {
        report,
        cursor: before,
        candidates,
    })
}

/// Preview by default; apply requires the exact ID from a prior preview and
/// repeats all source, cursor, note, and edge checks before its first write.
pub async fn run_reconciliation(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    options: ReconcileOptions,
) -> Result<ReconcileReport> {
    if !oid(&options.frozen_tip) {
        bail!("frozen_tip must be a full commit object id");
    }
    let path = std::fs::canonicalize(&options.repo).context("canonicalizing repository source")?;
    let source = parse_source(path.to_str().context("repository path is not UTF-8")?)
        .map_err(|error| anyhow!("{error}"))?;
    let DigestSource::Local(path) = source else {
        bail!("historical annotation repair requires a local repository source");
    };
    let source_identity = repo_identity(&DigestSource::Local(path.clone())).await;
    let mut first = preview(
        runtime,
        token,
        &path,
        &source_identity,
        options.project_id,
        &options.frozen_tip,
    )
    .await?;
    let Some(expected_preview_id) = options.apply_preview_id else {
        return Ok(first.report);
    };
    let mut apply = ApplyCounts::default();
    if expected_preview_id != first.report.preview_id {
        apply.failures = 1;
        apply.error = Some("preview ID is stale or mismatched".into());
    } else if !first.report.complete_coverage || !first.report.cursors_unchanged {
        apply.failures = 1;
        apply.error = Some("acknowledged history coverage is incomplete".into());
    } else if first.report.counts.missing_notes != 0
        || first.report.counts.deleted_notes != 0
        || first.report.counts.ambiguous_notes != 0
    {
        apply.failures = 1;
        apply.error = Some("missing, deleted, or ambiguous commit notes prevent completion".into());
    }
    if apply.failures == 0 {
        for chunk in first.candidates.chunks(WRITE_BATCH) {
            for (sha, note_id) in chunk {
                apply.attempted += 1;
                #[cfg(test)]
                if apply.attempted == 1 {
                    test_hooks::before_first_link(runtime, token, *note_id, options.project_id)
                        .await?;
                }
                match link_annotation_if_absent(
                    runtime,
                    token,
                    *note_id,
                    options.project_id,
                    annotation_guard(&first.cursor, sha, &source_identity)?,
                )
                .await
                {
                    Ok(AnnotationOutcome::Created) => apply.created += 1,
                    Ok(AnnotationOutcome::ExistingLive) => apply.raced_live += 1,
                    Ok(AnnotationOutcome::Tombstoned) => apply.raced_tombstone += 1,
                    Ok(AnnotationOutcome::SourceChanged) => {
                        apply.failures += 1;
                        apply.error = Some(format!("commit note changed before link: {sha}"));
                        break;
                    }
                    Ok(AnnotationOutcome::TargetChanged) => {
                        apply.failures += 1;
                        apply.error = Some("project changed before link".into());
                        break;
                    }
                    Ok(AnnotationOutcome::CursorChanged) => {
                        apply.failures += 1;
                        apply.error = Some("commit cursor changed before link".into());
                        break;
                    }
                    Err(error) => {
                        apply.failures += 1;
                        apply.error = Some(format!("conditional link failed: {error}"));
                        break;
                    }
                }
                #[cfg(test)]
                if apply.created == 1 {
                    test_hooks::after_first_link(runtime, options.project_id).await?;
                }
            }
            if apply.failures != 0 {
                break;
            }
        }
    }
    let after_cursor = cursor_snapshot(runtime, options.project_id).await?;
    first.report.cursors_unchanged = after_cursor == first.cursor;
    if !first.report.cursors_unchanged {
        apply.failures += 1;
        apply.error = Some("cursor rows changed during apply".into());
    }
    let second = preview(
        runtime,
        token,
        &path,
        &source_identity,
        options.project_id,
        &options.frozen_tip,
    )
    .await;
    match second {
        Ok(second) => {
            first.report.after_apply = Some(second.report.counts.clone());
            first.report.success = first.report.cursors_unchanged
                && second.cursor == first.cursor
                && apply.failures == 0
                && second.report.complete_coverage
                && second.report.cursors_unchanged
                && second.report.counts.repairable_missing_links == 0
                && second.report.counts.missing_notes == 0
                && second.report.counts.deleted_notes == 0
                && second.report.counts.ambiguous_notes == 0;
            if second.cursor != first.cursor {
                first.report.cursors_unchanged = false;
                if apply.failures == 0 {
                    apply.failures = 1;
                    apply.error = Some("cursor rows changed after apply".into());
                }
            }
        }
        Err(error) => {
            apply.failures += 1;
            apply.error = Some(format!("post-apply preview failed: {error}"));
        }
    }
    first.report.apply = Some(apply);
    Ok(first.report)
}

#[cfg(test)]
mod test_hooks {
    use std::sync::Mutex;

    use khive_runtime::{KhiveRuntime, NamespaceToken};
    use khive_storage::types::{SqlStatement, SqlValue};
    use khive_types::EdgeRelation;
    use serde_json::json;
    use uuid::Uuid;

    pub(super) enum Fault {
        DeleteNote(Uuid),
        ChangeCursorTimestamp,
    }

    static FAULT: Mutex<Option<Fault>> = Mutex::new(None);
    static BEFORE_FIRST: Mutex<bool> = Mutex::new(false);
    static BEFORE_FIRST_CURSOR: Mutex<bool> = Mutex::new(false);

    pub(super) fn set(fault: Fault) {
        *FAULT.lock().expect("fault lock") = Some(fault);
    }

    pub(super) fn insert_curated_before_first() {
        *BEFORE_FIRST.lock().expect("before-first lock") = true;
    }

    pub(super) fn change_cursor_before_first() {
        *BEFORE_FIRST_CURSOR
            .lock()
            .expect("before-first cursor lock") = true;
    }

    pub(super) async fn before_first_link(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note_id: Uuid,
        project_id: Uuid,
    ) -> anyhow::Result<()> {
        let enabled = std::mem::take(&mut *BEFORE_FIRST.lock().expect("before-first lock"));
        if enabled {
            runtime
                .link(
                    token,
                    note_id,
                    project_id,
                    EdgeRelation::Annotates,
                    0.42,
                    Some(json!({"curated":true})),
                )
                .await?;
        }
        let change_cursor = std::mem::take(
            &mut *BEFORE_FIRST_CURSOR
                .lock()
                .expect("before-first cursor lock"),
        );
        if change_cursor {
            runtime
                .sql()
                .writer()
                .await?
                .execute(SqlStatement {
                    sql: "UPDATE git_mirror_cursor SET updated_at=updated_at+1 \
                          WHERE project_id=?1 AND kind='commits_checkpoint'"
                        .into(),
                    params: vec![SqlValue::Text(project_id.to_string())],
                    label: Some("git_annotation_repair_test_pre_link_cursor".into()),
                })
                .await?;
        }
        Ok(())
    }

    pub(super) async fn after_first_link(
        runtime: &KhiveRuntime,
        project_id: Uuid,
    ) -> anyhow::Result<()> {
        let fault = FAULT.lock().expect("fault lock").take();
        let Some(fault) = fault else {
            return Ok(());
        };
        let mut writer = runtime.sql().writer().await?;
        match fault {
            Fault::DeleteNote(note_id) => {
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE notes SET deleted_at=42 WHERE id=?1".into(),
                        params: vec![SqlValue::Text(note_id.to_string())],
                        label: Some("git_annotation_repair_test_delete_note".into()),
                    })
                    .await?;
            }
            Fault::ChangeCursorTimestamp => {
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE git_mirror_cursor SET updated_at=updated_at+1 \
                              WHERE project_id=?1 AND kind='commits_checkpoint'"
                            .into(),
                        params: vec![SqlValue::Text(project_id.to_string())],
                        label: Some("git_annotation_repair_test_change_cursor".into()),
                    })
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "reconcile/batch_tests.rs"]
mod batch_tests;

#[cfg(test)]
#[path = "reconcile/cursor_snapshot_tests.rs"]
mod cursor_snapshot_tests;

#[cfg(test)]
#[path = "reconcile_tests.rs"]
mod tests;
