//! One-shot, per-project historical commit annotation reconciliation (#3532).
//! This is an operator API, not a pack verb.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use khive_runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::graph::{
    CommitAnnotationCursorValue, CommitAnnotationGuard, CommitAnnotationInsertOutcome,
};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::source::{parse_source, repo_identity, DigestSource};

const CURSOR_MAX_BYTES: i64 = 8 * 1024;
const MAX_ACKNOWLEDGED_SHAS: usize = 250_000;
const DIAGNOSTIC_CAP: usize = 16;
const WRITE_BATCH: usize = 64;

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

fn text<'a>(row: &'a SqlRow, key: &str) -> Result<&'a str> {
    match row.get(key) {
        Some(SqlValue::Text(value)) => Ok(value),
        _ => bail!("stored {key} has an invalid type"),
    }
}

fn integer(row: &SqlRow, key: &str) -> Result<i64> {
    match row.get(key) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        _ => bail!("stored {key} has an invalid type"),
    }
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn cursor_snapshot(runtime: &KhiveRuntime, project_id: Uuid) -> Result<Vec<CursorRow>> {
    let mut reader = runtime.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: crate::sql::sql!("annotation_repair_cursor_snapshot_select").into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Integer(CURSOR_MAX_BYTES),
            ],
            label: Some("git_annotation_repair_cursor_snapshot".into()),
        })
        .await?;
    let mut snapshot = Vec::with_capacity(rows.len());
    for row in rows {
        let kind = text(&row, "kind")?.to_owned();
        let value_type = text(&row, "value_type")?.to_owned();
        let value_bytes = match row.get("value_bytes") {
            Some(SqlValue::Integer(value)) if *value >= 0 => Some(*value),
            Some(SqlValue::Null) => None,
            _ => bail!("stored cursor length has an invalid type"),
        };
        if value_bytes.is_some_and(|size| size > CURSOR_MAX_BYTES) {
            bail!("stored {kind} cursor exceeds the repair size limit");
        }
        let value = match row.get("value") {
            Some(SqlValue::Blob(value)) => Some(value.clone()),
            Some(SqlValue::Null) => None,
            _ => bail!("stored cursor value has an invalid type"),
        };
        snapshot.push(CursorRow {
            kind,
            updated_at: integer(&row, "updated_at")?,
            value_type,
            value,
        });
    }
    Ok(snapshot)
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
) -> Result<CommitAnnotationGuard> {
    let value = |kind: &str| -> Result<CommitAnnotationCursorValue> {
        let row = rows
            .iter()
            .find(|row| row.kind == kind)
            .ok_or_else(|| anyhow!("{kind} cursor is absent"))?;
        if row.value_type != "text" {
            bail!("{kind} cursor is not text");
        }
        Ok(CommitAnnotationCursorValue {
            value: row
                .value
                .clone()
                .ok_or_else(|| anyhow!("null {kind} cursor"))?,
            updated_at: row.updated_at,
        })
    };
    Ok(CommitAnnotationGuard {
        expected_sha: expected_sha.into(),
        source_identity: source_identity.into(),
        commits: value("commits")?,
        checkpoint: value("commits_checkpoint")?,
    })
}

fn git_status(repo: &Path, args: &[&str]) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
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
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-t", sha])
        .stdin(Stdio::null())
        .output()
        .context("checking frozen git object")?;
    Ok(output.status.success() && output.stdout == b"commit\n")
}

fn acknowledged_prefix(repo: &Path, checkpoint: &CommitCheckpoint) -> Result<Vec<String>> {
    use std::io::{BufRead, BufReader};
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "log",
            "--reverse",
            "--topo-order",
            "--format=%H",
            &checkpoint.snapshot_head,
            "--",
        ])
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
            for sha in shas {
                report.counts.acknowledged_shas_examined += 1;
                let notes = reader
                    .query_all(SqlStatement {
                        sql: crate::sql::sql!("annotation_repair_commit_notes_select").into(),
                        params: vec![
                            SqlValue::Text(namespace.into()),
                            SqlValue::Text(sha.clone()),
                        ],
                        label: Some("git_annotation_repair_note".into()),
                    })
                    .await?;
                let class = if notes.is_empty() {
                    report.counts.missing_notes += 1;
                    "missing_note"
                } else if notes.len() != 1 {
                    report.counts.ambiguous_notes += 1;
                    "ambiguous_note"
                } else if !matches!(notes[0].get("deleted_at"), Some(SqlValue::Null)) {
                    report.counts.deleted_notes += 1;
                    "deleted_note"
                } else {
                    let note_id = match notes[0].get("id") {
                        Some(SqlValue::Uuid(id)) => *id,
                        Some(SqlValue::Text(id)) => {
                            Uuid::parse_str(id).context("stored commit note has an invalid id")?
                        }
                        _ => bail!("stored commit note has an invalid id"),
                    };
                    hasher.update(note_id.as_bytes());
                    report.counts.live_note_hits += 1;
                    let edge = reader
                        .query_row(SqlStatement {
                            sql: crate::sql::sql!("annotation_repair_project_edge_select").into(),
                            params: vec![
                                SqlValue::Text(namespace.into()),
                                SqlValue::Text(note_id.to_string()),
                                SqlValue::Text(project_id.to_string()),
                            ],
                            label: Some("git_annotation_repair_edge".into()),
                        })
                        .await?;
                    match edge {
                        Some(edge) if matches!(edge.get("deleted_at"), Some(SqlValue::Null)) => {
                            report.counts.live_project_edges += 1;
                            "live_edge"
                        }
                        Some(_) => {
                            report.counts.tombstones_skipped += 1;
                            "tombstone"
                        }
                        None => {
                            report.counts.repairable_missing_links += 1;
                            candidates.push((sha.clone(), note_id));
                            "missing_link"
                        }
                    }
                };
                hasher.update(&serde_json::to_vec(&(&sha, class))?);
            }
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
                match runtime
                    .link_commit_annotation_if_absent(
                        token,
                        *note_id,
                        options.project_id,
                        annotation_guard(&first.cursor, sha, &source_identity)?,
                    )
                    .await
                {
                    Ok(CommitAnnotationInsertOutcome::Created(_)) => apply.created += 1,
                    Ok(CommitAnnotationInsertOutcome::ExistingLive) => apply.raced_live += 1,
                    Ok(CommitAnnotationInsertOutcome::Tombstoned) => apply.raced_tombstone += 1,
                    Ok(CommitAnnotationInsertOutcome::SourceChanged) => {
                        apply.failures += 1;
                        apply.error = Some(format!("commit note changed before link: {sha}"));
                        break;
                    }
                    Ok(CommitAnnotationInsertOutcome::TargetChanged) => {
                        apply.failures += 1;
                        apply.error = Some("project changed before link".into());
                        break;
                    }
                    Ok(CommitAnnotationInsertOutcome::CursorChanged) => {
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
mod tests {
    use super::*;

    use khive_runtime::{Namespace, VerbRegistry, VerbRegistryBuilder};
    use serde_json::{json, Value};

    use crate::GitPack;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("start git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output is UTF-8")
            .trim()
            .into()
    }

    fn commit(repo: &Path, name: &str) -> String {
        std::fs::write(repo.join(name), name).expect("write fixture file");
        git(repo, &["add", name]);
        git(repo, &["commit", "-q", "-m", name]);
        git(repo, &["rev-parse", "HEAD"])
    }

    struct Fixture {
        runtime: KhiveRuntime,
        token: NamespaceToken,
        registry: VerbRegistry,
        repo: tempfile::TempDir,
        project_a: Uuid,
        project_b: Uuid,
        source_identity: String,
        first_sha: String,
        second_sha: String,
        first_note: Uuid,
        second_note: Uuid,
    }

    impl Fixture {
        async fn new() -> Self {
            let runtime = KhiveRuntime::memory().expect("memory runtime");
            let token = runtime.authorize(Namespace::local()).expect("local token");
            let mut builder = VerbRegistryBuilder::new();
            builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
            builder.register(GitPack::new(runtime.clone()));
            builder
                .with_runtime_event_store(&runtime)
                .expect("event store");
            let registry = builder.build().expect("registry");
            runtime.install_edge_rules(registry.all_edge_rules());
            registry.apply_schema_plans(runtime.backend());

            let repo = tempfile::tempdir().expect("repository");
            git(repo.path(), &["init", "-q", "-b", "main"]);
            git(repo.path(), &["config", "user.email", "test@example.com"]);
            git(repo.path(), &["config", "user.name", "Test User"]);
            let first_sha = commit(repo.path(), "first.txt");
            let second_sha = commit(repo.path(), "second.txt");
            let identity = repo_identity(&DigestSource::Local(repo.path().to_path_buf())).await;
            let project_a = Self::create(
                &registry,
                json!({"kind":"project","name":"anchor A","properties":{"repo_slug":identity}}),
            )
            .await;
            let project_b = Self::create(
                &registry,
                json!({"kind":"project","name":"anchor B","properties":{"repo_slug":identity}}),
            )
            .await;
            let first_note = Self::create(
                &registry,
                json!({"kind":"commit","name":"first","content":"first",
                       "properties":{"sha":first_sha},"annotates":[project_a.to_string()]}),
            )
            .await;
            let second_note = Self::create(
                &registry,
                json!({"kind":"commit","name":"second","content":"second",
                       "properties":{"sha":second_sha},"annotates":[project_b.to_string()]}),
            )
            .await;
            let fixture = Self {
                runtime,
                token,
                registry,
                repo,
                project_a,
                project_b,
                source_identity: identity,
                first_sha,
                second_sha,
                first_note,
                second_note,
            };
            fixture
                .set_checkpoint(
                    fixture.project_b,
                    None,
                    &fixture.second_sha,
                    &fixture.second_sha,
                )
                .await;
            fixture
        }

        async fn create(registry: &VerbRegistry, request: Value) -> Uuid {
            let response = registry.dispatch("create", request).await.expect("create");
            Uuid::parse_str(response["id"].as_str().expect("created id")).expect("uuid")
        }

        async fn set_checkpoint(&self, project: Uuid, base: Option<&str>, head: &str, last: &str) {
            let checkpoint = json!({
                "version": 1,
                "namespace": "local",
                "base_cursor": base,
                "snapshot_head": head,
                "last_completed_sha": last,
            });
            self.runtime
                .sql()
                .writer()
                .await
                .expect("writer")
                .execute(SqlStatement {
                    sql: crate::sql::sql!("commit_checkpoint_upsert").into(),
                    params: vec![
                        SqlValue::Text(project.to_string()),
                        SqlValue::Text(checkpoint.to_string()),
                        SqlValue::Text(last.into()),
                        SqlValue::Integer(123),
                    ],
                    label: Some("test_commit_checkpoint".into()),
                })
                .await
                .expect("checkpoint write");
        }

        fn options(&self, apply_preview_id: Option<String>) -> ReconcileOptions {
            ReconcileOptions {
                repo: self.repo.path().to_path_buf(),
                project_id: self.project_b,
                frozen_tip: self.second_sha.clone(),
                apply_preview_id,
            }
        }

        async fn run(&self, apply_preview_id: Option<String>) -> ReconcileReport {
            run_reconciliation(&self.runtime, &self.token, self.options(apply_preview_id))
                .await
                .expect("repair report")
        }

        async fn edge_bytes(&self, note: Uuid) -> Option<Vec<u8>> {
            let row = self
                .runtime
                .sql()
                .reader()
                .await
                .expect("reader")
                .query_row(SqlStatement {
                    sql: "SELECT namespace,id,source_id,target_id,relation,weight, \
                          created_at,updated_at,deleted_at,metadata,target_backend \
                          FROM graph_edges WHERE namespace='local' AND source_id=?1 \
                          AND target_id=?2 AND relation='annotates'"
                        .into(),
                    params: vec![
                        SqlValue::Text(note.to_string()),
                        SqlValue::Text(self.project_b.to_string()),
                    ],
                    label: Some("test_annotation_edge_bytes".into()),
                })
                .await
                .expect("edge read");
            row.map(|row| serde_json::to_vec(&row).expect("serialize row"))
        }

        async fn link_created_events(&self, note: Uuid) -> i64 {
            let count = self
                .runtime
                .sql()
                .reader()
                .await
                .expect("reader")
                .query_scalar(SqlStatement {
                    sql: "SELECT COUNT(*) FROM events WHERE verb='link' \
                          AND kind='link_created' AND target_id IN \
                          (SELECT id FROM graph_edges WHERE namespace='local' \
                           AND source_id=?1 AND target_id=?2 AND relation='annotates')"
                        .into(),
                    params: vec![
                        SqlValue::Text(note.to_string()),
                        SqlValue::Text(self.project_b.to_string()),
                    ],
                    label: Some("test_repair_link_created_events".into()),
                })
                .await
                .expect("event count")
                .expect("COUNT returns a row");
            match count {
                SqlValue::Integer(count) => count,
                other => panic!("unexpected event count: {other:?}"),
            }
        }

        async fn execute(&self, sql: &str, params: Vec<SqlValue>) {
            self.runtime
                .sql()
                .writer()
                .await
                .expect("writer")
                .execute(SqlStatement {
                    sql: sql.into(),
                    params,
                    label: Some("test_annotation_mutation".into()),
                })
                .await
                .expect("fixture mutation");
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn two_anchors_repair_one_old_shared_sha_and_preserve_cursor_and_curated_edge() {
        let fixture = Fixture::new().await;
        assert_ne!(fixture.project_a, fixture.project_b);
        fixture
            .execute(
                "UPDATE graph_edges SET weight=0.42, metadata='{\"curated\":true}' \
                 WHERE source_id=?1 AND target_id=?2 AND relation='annotates'",
                vec![
                    SqlValue::Text(fixture.second_note.to_string()),
                    SqlValue::Text(fixture.project_b.to_string()),
                ],
            )
            .await;
        let curated_before = fixture.edge_bytes(fixture.second_note).await;
        let cursor_before = cursor_snapshot(&fixture.runtime, fixture.project_b)
            .await
            .unwrap();
        let preview = fixture.run(None).await;
        assert!(preview.complete_coverage, "{:?}", preview.diagnostics);
        assert_eq!(preview.counts.acknowledged_shas_examined, 2);
        assert_eq!(preview.counts.repairable_missing_links, 1);
        assert_eq!(preview.counts.live_project_edges, 1);
        assert_eq!(fixture.edge_bytes(fixture.first_note).await, None);
        assert_eq!(fixture.link_created_events(fixture.first_note).await, 0);
        assert_eq!(
            fixture.edge_bytes(fixture.second_note).await,
            curated_before
        );
        let applied = fixture.run(Some(preview.preview_id)).await;
        assert!(applied.success, "{applied:?}");
        assert_eq!(applied.apply.as_ref().unwrap().created, 1);
        assert!(fixture.edge_bytes(fixture.first_note).await.is_some());
        assert_eq!(fixture.link_created_events(fixture.first_note).await, 1);
        assert_eq!(
            fixture.edge_bytes(fixture.second_note).await,
            curated_before
        );
        let repaired_edge = fixture.edge_bytes(fixture.first_note).await;
        let repeat_preview = fixture.run(None).await;
        assert_eq!(repeat_preview.counts.repairable_missing_links, 0);
        let repeat = fixture.run(Some(repeat_preview.preview_id)).await;
        assert!(repeat.success);
        assert_eq!(repeat.apply.as_ref().unwrap().created, 0);
        assert_eq!(fixture.edge_bytes(fixture.first_note).await, repaired_edge);
        assert_eq!(fixture.link_created_events(fixture.first_note).await, 1);
        assert_eq!(
            cursor_snapshot(&fixture.runtime, fixture.project_b)
                .await
                .unwrap(),
            cursor_before
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn conditional_annotation_link_uses_normal_endpoint_rules() {
        let fixture = Fixture::new().await;
        let guard = annotation_guard(
            &cursor_snapshot(&fixture.runtime, fixture.project_b)
                .await
                .unwrap(),
            &fixture.first_sha,
            &fixture.source_identity,
        )
        .unwrap();
        let error = fixture
            .runtime
            .link_commit_annotation_if_absent(
                &fixture.token,
                fixture.project_a,
                fixture.project_b,
                guard,
            )
            .await
            .expect_err("an entity cannot be an annotates source");
        assert!(error.to_string().contains("annotates source"), "{error}");
        let written = fixture
            .runtime
            .sql()
            .reader()
            .await
            .expect("reader")
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM graph_edges WHERE namespace='local' \
                      AND source_id=?1 AND target_id=?2 AND relation='annotates'"
                    .into(),
                params: vec![
                    SqlValue::Text(fixture.project_a.to_string()),
                    SqlValue::Text(fixture.project_b.to_string()),
                ],
                label: Some("test_rejected_annotation_edge_count".into()),
            })
            .await
            .expect("edge count");
        assert!(matches!(written, Some(SqlValue::Integer(0))));
        assert_eq!(fixture.link_created_events(fixture.project_a).await, 0);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn conditional_annotation_link_emits_only_for_created_outcome() {
        let fixture = Fixture::new().await;
        let guard = annotation_guard(
            &cursor_snapshot(&fixture.runtime, fixture.project_b)
                .await
                .unwrap(),
            &fixture.first_sha,
            &fixture.source_identity,
        )
        .unwrap();
        assert_eq!(fixture.link_created_events(fixture.first_note).await, 0);

        let created = fixture
            .runtime
            .link_commit_annotation_if_absent(
                &fixture.token,
                fixture.first_note,
                fixture.project_b,
                guard.clone(),
            )
            .await
            .expect("new annotation");
        assert!(matches!(created, CommitAnnotationInsertOutcome::Created(_)));
        assert_eq!(fixture.link_created_events(fixture.first_note).await, 1);
        let live_bytes = fixture.edge_bytes(fixture.first_note).await;
        let before_existing_events = fixture.link_created_events(fixture.first_note).await;

        let existing = fixture
            .runtime
            .link_commit_annotation_if_absent(
                &fixture.token,
                fixture.first_note,
                fixture.project_b,
                guard.clone(),
            )
            .await
            .expect("existing live annotation");
        assert!(matches!(
            existing,
            CommitAnnotationInsertOutcome::ExistingLive
        ));
        assert_eq!(
            fixture.link_created_events(fixture.first_note).await - before_existing_events,
            0,
            "an existing-live outcome emits zero new LinkCreated events"
        );
        assert_eq!(fixture.edge_bytes(fixture.first_note).await, live_bytes);

        fixture
            .execute(
                "UPDATE graph_edges SET deleted_at=77 WHERE source_id=?1 AND target_id=?2 \
                 AND relation='annotates'",
                vec![
                    SqlValue::Text(fixture.first_note.to_string()),
                    SqlValue::Text(fixture.project_b.to_string()),
                ],
            )
            .await;
        let tombstone_bytes = fixture.edge_bytes(fixture.first_note).await;
        let before_tombstone_events = fixture.link_created_events(fixture.first_note).await;
        let tombstoned = fixture
            .runtime
            .link_commit_annotation_if_absent(
                &fixture.token,
                fixture.first_note,
                fixture.project_b,
                guard,
            )
            .await
            .expect("tombstoned annotation");
        assert!(matches!(
            tombstoned,
            CommitAnnotationInsertOutcome::Tombstoned
        ));
        assert_eq!(
            fixture.link_created_events(fixture.first_note).await - before_tombstone_events,
            0,
            "a tombstoned outcome emits zero new LinkCreated events"
        );
        assert_eq!(
            fixture.edge_bytes(fixture.first_note).await,
            tombstone_bytes
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn tombstone_is_an_exclusion_and_remains_byte_exact() {
        let fixture = Fixture::new().await;
        fixture
            .runtime
            .link_commit_annotation_if_absent(
                &fixture.token,
                fixture.first_note,
                fixture.project_b,
                annotation_guard(
                    &cursor_snapshot(&fixture.runtime, fixture.project_b)
                        .await
                        .unwrap(),
                    &fixture.first_sha,
                    &fixture.source_identity,
                )
                .unwrap(),
            )
            .await
            .expect("fixture link");
        fixture
            .execute(
                "UPDATE graph_edges SET deleted_at=77 WHERE source_id=?1 AND target_id=?2 \
                 AND relation='annotates'",
                vec![
                    SqlValue::Text(fixture.first_note.to_string()),
                    SqlValue::Text(fixture.project_b.to_string()),
                ],
            )
            .await;
        let before = fixture.edge_bytes(fixture.first_note).await;
        let preview = fixture.run(None).await;
        assert_eq!(preview.counts.tombstones_skipped, 1);
        assert_eq!(preview.counts.repairable_missing_links, 0);
        let apply = fixture.run(Some(preview.preview_id)).await;
        assert!(apply.success);
        assert_eq!(apply.apply.as_ref().unwrap().created, 0);
        assert_eq!(fixture.edge_bytes(fixture.first_note).await, before);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn curated_link_racing_preview_is_preserved_byte_exact() {
        let fixture = Fixture::new().await;
        let preview = fixture.run(None).await;
        assert_eq!(preview.counts.repairable_missing_links, 1);
        test_hooks::insert_curated_before_first();
        let applied = fixture.run(Some(preview.preview_id)).await;
        assert!(applied.success, "{applied:?}");
        assert_eq!(applied.apply.as_ref().unwrap().created, 0);
        assert_eq!(applied.apply.as_ref().unwrap().raced_live, 1);
        let edge = fixture.edge_bytes(fixture.first_note).await.unwrap();
        assert!(String::from_utf8_lossy(&edge).contains("curated"));
        let again = fixture.run(None).await;
        assert_eq!(again.counts.repairable_missing_links, 0);
        assert_eq!(fixture.edge_bytes(fixture.first_note).await.unwrap(), edge);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn cursor_change_before_first_link_blocks_every_write() {
        let fixture = Fixture::new().await;
        let preview = fixture.run(None).await;
        test_hooks::change_cursor_before_first();
        let applied = fixture.run(Some(preview.preview_id)).await;
        assert!(!applied.success);
        assert_eq!(applied.apply.as_ref().unwrap().created, 0);
        assert_eq!(applied.apply.as_ref().unwrap().failures, 2);
        assert_eq!(fixture.edge_bytes(fixture.first_note).await, None);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn missing_deleted_and_ambiguous_notes_refuse_apply_without_fabrication() {
        let deleted = Fixture::new().await;
        deleted
            .execute(
                "UPDATE notes SET deleted_at=42 WHERE id=?1",
                vec![SqlValue::Text(deleted.first_note.to_string())],
            )
            .await;
        let preview = deleted.run(None).await;
        assert_eq!(preview.counts.deleted_notes, 1);
        let apply = deleted.run(Some(preview.preview_id)).await;
        assert!(!apply.success);
        assert_eq!(apply.apply.as_ref().unwrap().created, 0);

        let missing = Fixture::new().await;
        missing
            .execute(
                "DELETE FROM notes WHERE id=?1",
                vec![SqlValue::Text(missing.first_note.to_string())],
            )
            .await;
        let preview = missing.run(None).await;
        assert_eq!(preview.counts.missing_notes, 1);
        assert!(!missing.run(Some(preview.preview_id)).await.success);

        let ambiguous = Fixture::new().await;
        let _duplicate = Fixture::create(
            &ambiguous.registry,
            json!({"kind":"commit","name":"duplicate","content":"duplicate",
                   "properties":{"sha":ambiguous.first_sha}}),
        )
        .await;
        let preview = ambiguous.run(None).await;
        assert_eq!(preview.counts.ambiguous_notes, 1);
        assert!(!ambiguous.run(Some(preview.preview_id)).await.success);
        assert_eq!(ambiguous.edge_bytes(ambiguous.first_note).await, None);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn link_failure_after_one_write_reports_partial_and_leaves_retryable_candidate() {
        let fixture = Fixture::new().await;
        fixture
            .execute(
                "DELETE FROM graph_edges WHERE source_id=?1 AND target_id=?2 \
                 AND relation='annotates'",
                vec![
                    SqlValue::Text(fixture.second_note.to_string()),
                    SqlValue::Text(fixture.project_b.to_string()),
                ],
            )
            .await;
        let preview = fixture.run(None).await;
        assert_eq!(preview.counts.repairable_missing_links, 2);
        test_hooks::set(test_hooks::Fault::DeleteNote(fixture.second_note));
        let applied = fixture.run(Some(preview.preview_id)).await;
        assert!(!applied.success);
        assert_eq!(applied.apply.as_ref().unwrap().created, 1);
        assert_eq!(applied.apply.as_ref().unwrap().failures, 1);
        assert!(fixture.edge_bytes(fixture.first_note).await.is_some());
        assert_eq!(fixture.edge_bytes(fixture.second_note).await, None);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn cursor_byte_or_timestamp_change_refuses_stale_or_inflight_apply() {
        let fixture = Fixture::new().await;
        let preview = fixture.run(None).await;
        fixture
            .execute(
                "UPDATE git_mirror_cursor SET cursor_value=cursor_value || 'x' \
                 WHERE project_id=?1 AND kind='commits'",
                vec![SqlValue::Text(fixture.project_b.to_string())],
            )
            .await;
        let refused = fixture.run(Some(preview.preview_id)).await;
        assert!(!refused.success);
        assert_eq!(refused.apply.as_ref().unwrap().created, 0);
        assert_eq!(fixture.edge_bytes(fixture.first_note).await, None);

        let checkpoint = Fixture::new().await;
        let preview = checkpoint.run(None).await;
        checkpoint
            .execute(
                "UPDATE git_mirror_cursor SET cursor_value=cursor_value || 'x' \
                 WHERE project_id=?1 AND kind='commits_checkpoint'",
                vec![SqlValue::Text(checkpoint.project_b.to_string())],
            )
            .await;
        let refused = checkpoint.run(Some(preview.preview_id)).await;
        assert!(!refused.success);
        assert_eq!(refused.apply.as_ref().unwrap().created, 0);
        assert_eq!(checkpoint.edge_bytes(checkpoint.first_note).await, None);

        let inflight = Fixture::new().await;
        let preview = inflight.run(None).await;
        test_hooks::set(test_hooks::Fault::ChangeCursorTimestamp);
        let applied = inflight.run(Some(preview.preview_id)).await;
        assert!(!applied.success);
        assert!(!applied.cursors_unchanged);
        assert_eq!(applied.apply.as_ref().unwrap().created, 1);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn prior_walk_and_unavailable_or_diverged_tip_are_incomplete() {
        let fixture = Fixture::new().await;
        fixture
            .set_checkpoint(
                fixture.project_b,
                Some(&fixture.first_sha),
                &fixture.second_sha,
                &fixture.second_sha,
            )
            .await;
        let incomplete = fixture.run(None).await;
        assert!(!incomplete.complete_coverage);
        assert_eq!(incomplete.counts.coverage_errors, 1);
        assert!(incomplete.diagnostics[0].contains("non-null base_cursor"));
        assert!(!fixture.run(Some(incomplete.preview_id)).await.success);

        let unavailable = Fixture::new().await;
        let mut options = unavailable.options(None);
        options.frozen_tip = "0".repeat(40);
        let preview = run_reconciliation(&unavailable.runtime, &unavailable.token, options)
            .await
            .expect("incomplete preview");
        assert!(!preview.complete_coverage);
        assert_eq!(preview.counts.coverage_errors, 1);

        let diverged = Fixture::new().await;
        git(diverged.repo.path(), &["checkout", "--orphan", "unrelated"]);
        let orphan_sha = commit(diverged.repo.path(), "orphan.txt");
        git(diverged.repo.path(), &["checkout", "main"]);
        let mut options = diverged.options(None);
        options.frozen_tip = orphan_sha;
        let preview = run_reconciliation(&diverged.runtime, &diverged.token, options)
            .await
            .expect("diverged preview");
        assert!(!preview.complete_coverage);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn merge_dag_prefix_includes_a_nonancestor_sibling() {
        let fixture = Fixture::new().await;
        git(
            fixture.repo.path(),
            &["checkout", "-q", "-b", "side", &fixture.first_sha],
        );
        commit(fixture.repo.path(), "side.txt");
        git(fixture.repo.path(), &["checkout", "-q", "main"]);
        commit(fixture.repo.path(), "main.txt");
        git(
            fixture.repo.path(),
            &["merge", "-q", "--no-ff", "-m", "merge branches", "side"],
        );
        let merge = git(fixture.repo.path(), &["rev-parse", "HEAD"]);
        let walk = git(
            fixture.repo.path(),
            &[
                "log",
                "--reverse",
                "--topo-order",
                "--format=%H",
                &merge,
                "--",
            ],
        );
        let shas: Vec<_> = walk.lines().collect();
        let (index, last) = shas
            .iter()
            .enumerate()
            .find(|(index, last)| {
                shas[..*index].iter().any(|older| {
                    !git_status(
                        fixture.repo.path(),
                        &["merge-base", "--is-ancestor", older, last],
                    )
                    .unwrap()
                })
            })
            .expect("merge walk has a sibling before a branch tip");
        let checkpoint = CommitCheckpoint {
            version: 1,
            namespace: "local".into(),
            base_cursor: None,
            snapshot_head: merge,
            last_completed_sha: (*last).into(),
        };
        let prefix = acknowledged_prefix(fixture.repo.path(), &checkpoint).unwrap();
        let expected: Vec<String> = shas[..=index].iter().map(|sha| (*sha).to_owned()).collect();
        assert_eq!(prefix, expected);
        assert!(prefix[..index].iter().any(|older| {
            !git_status(
                fixture.repo.path(),
                &["merge-base", "--is-ancestor", older, last],
            )
            .unwrap()
        }));

        // A non-null base SHA cannot prove that a previous walk visited the
        // non-ancestor sibling above, even though this latest span is valid.
        fixture
            .set_checkpoint(
                fixture.project_b,
                Some(&fixture.first_sha),
                &checkpoint.snapshot_head,
                &checkpoint.last_completed_sha,
            )
            .await;
        let mut options = fixture.options(None);
        options.frozen_tip = checkpoint.snapshot_head.clone();
        let incomplete = run_reconciliation(&fixture.runtime, &fixture.token, options.clone())
            .await
            .expect("incomplete preview");
        assert!(!incomplete.complete_coverage);
        assert_eq!(incomplete.counts.coverage_errors, 1);
        assert!(incomplete.diagnostics[0].contains("non-null base_cursor"));
        options.apply_preview_id = Some(incomplete.preview_id);
        let refused = run_reconciliation(&fixture.runtime, &fixture.token, options)
            .await
            .expect("refused apply");
        assert!(!refused.success);
        assert_eq!(refused.apply.as_ref().unwrap().created, 0);
    }
}
