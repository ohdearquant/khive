//! `kkernel code-ingest`: admin path that ingests a validated
//! `findings.json` sweep into the graph via
//! `khive_pack_code::ingest::ingest_findings_json` (ADR-085 Amendment 3).
//!
//! Findings ingestion is deliberately not a verb (ADR-085 D1, Amendment 3
//! C2): this CLI is the only writer of `finding` notes, and agents never
//! hold a bulk-ingest verb (the runner-writes rule). Validation is whole-document and fail-closed: a
//! malformed `findings.json` is rejected before any record is written.
//! `--dry-run` runs the same validation and existence checks but performs
//! no writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use serde::Serialize;

use khive_db::StorageBackend;
use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_pack_code::{ingest_findings_json, CodeIngestBatch, CodeIngestOptions};
use khive_runtime::{entity_fts_document, note_fts_document, secret_gate, KhiveRuntime, Namespace};
use khive_storage::{SqlStatement, SqlValue, SubstrateKind};

/// Upper bound on how long the real ingest path waits for the pool's writer
/// task to exit after the last write returned. Generous relative to any
/// realistic queue depth for a findings batch; hitting it means something is
/// holding a `WriterTaskHandle` clone alive (the queue never closed) or the
/// writer is wedged, and the caller is told loudly rather than returning
/// with the database file still in motion.
const WRITER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Arguments for `kkernel code-ingest`.
#[derive(Parser, Debug)]
pub struct CodeIngestArgs {
    /// Path to a validated `findings.json` sweep.
    pub findings: PathBuf,

    /// Stable sweep identity. Falls back to `audit.date:audit.commit` from
    /// the findings document when absent.
    #[arg(long = "source-run")]
    pub source_run: Option<String>,

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,

    /// Namespace to write into.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Validate the document and report what would happen without writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,
}

/// Outcome of one `code-ingest` pass.
#[derive(Debug, Default, Serialize)]
pub struct CodeIngestReport {
    pub dry_run: bool,
    pub entities_created: u64,
    pub entities_skipped_existing: u64,
    pub notes_created: u64,
    pub notes_skipped_existing: u64,
    pub edges_created: u64,
    pub edges_skipped_existing: u64,
    /// Actual embedding-input truncation grouped by the model that received
    /// each bounded document. Empty for dry runs and no-embedder runs.
    pub truncation_by_model: BTreeMap<String, khive_runtime::retrieval::EmbeddingTruncationReport>,
}

/// Run one `kkernel code-ingest` pass: resolve config, validate the
/// `findings.json` document as a whole (fail-closed, before any write), then
/// persist the deterministic entity/note/edge batch record-by-record.
/// Records whose content-derived ID has ever existed, including soft-deleted
/// tombstones, are reported as skipped and never overwritten or reactivated:
/// a `finding` note's lifecycle state (`kind_status`) and deletion state are
/// curated data, not something re-ingesting the same sweep should reset.
pub async fn run_code_ingest(args: CodeIngestArgs) -> Result<()> {
    let human = args.human;
    let report = code_ingest_batch(args).await?;

    if human {
        println!(
            "entities: {} created, {} skipped\nnotes: {} created, {} skipped\nedges: {} created, {} skipped{}",
            report.entities_created,
            report.entities_skipped_existing,
            report.notes_created,
            report.notes_skipped_existing,
            report.edges_created,
            report.edges_skipped_existing,
            if report.dry_run {
                "\n(dry run: nothing written)"
            } else {
                ""
            },
        );
        if report
            .truncation_by_model
            .values()
            .any(khive_runtime::retrieval::EmbeddingTruncationReport::any_truncated)
        {
            println!(
                "embedding truncation: {}",
                serde_json::to_string(&report.truncation_by_model)?
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

/// Core of `run_code_ingest`, split out so tests can assert on the returned
/// [`CodeIngestReport`] directly instead of parsing stdout.
///
/// Order matters here: the document is read, parsed, and fully validated
/// (`ingest_findings_json`), then secret-gate-preflighted, entirely BEFORE
/// any `KhiveRuntime`/database construction. This is what makes `--dry-run`
/// (and a rejected invalid document) leave the filesystem untouched: no
/// runtime, no migrations, no embedding-model registration happen until
/// after validation has already succeeded and a real (non-dry-run) write is
/// about to occur.
async fn code_ingest_batch(args: CodeIngestArgs) -> Result<CodeIngestReport> {
    code_ingest_batch_with_runtime_setup(args, |_| Ok(())).await
}

async fn code_ingest_batch_with_runtime_setup<F>(
    args: CodeIngestArgs,
    runtime_setup: F,
) -> Result<CodeIngestReport>
where
    F: FnOnce(&KhiveRuntime) -> Result<()>,
{
    let bytes = std::fs::read(&args.findings)
        .with_context(|| format!("failed to read {}", args.findings.display()))?;

    let ns = Namespace::parse(&args.namespace).map_err(|e| anyhow::anyhow!("{e}"))?;
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: None,
        namespace: ns,
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })?;

    // The write path below persists `finding` notes directly through
    // EntityStore/NoteStore/GraphStore rather than through pack dispatch (see
    // `preflight_secret_gate` below), so it must independently confirm the
    // `code` pack is actually part of this run's configured pack set —
    // otherwise a misconfigured `KHIVE_PACKS`/`--pack` could accept
    // `finding` records into a graph that never declared the kind.
    if !cfg.packs.iter().any(|p| p == "code") {
        anyhow::bail!(
            "the `code` pack is not in the configured pack set {:?}; `finding` notes require it \
             to be loaded (set KHIVE_PACKS to include `code`, or drop --pack overrides)",
            cfg.packs
        );
    }

    // Whole-document validation before any runtime/database construction
    // (fail-closed): a malformed findings.json returns Err here and the
    // process exits nonzero with zero filesystem effect.
    let batch = ingest_findings_json(
        &bytes,
        CodeIngestOptions {
            namespace: cfg.default_namespace.as_str(),
            observed_at: Utc::now(),
            source_run: args.source_run.as_deref(),
        },
    )
    .with_context(|| format!("{} failed validation", args.findings.display()))?;

    // Preflight every entity/note content and nested property value through
    // the same secret gate the shared `create` verb path applies
    // (`crate::secret_gate::check`/`check_json`). This path writes directly
    // through the storage traits rather than `registry.dispatch("create",
    // ...)` — explicit-id creation (required for the content-derived UUIDv5
    // identity that makes re-ingest idempotent) has no dispatch-level
    // equivalent today — so the gate has to run here instead of being
    // inherited for free from the shared create handler.
    preflight_secret_gate(&batch)?;

    if args.dry_run {
        return dry_run_report(cfg.db_path.as_deref(), &batch).await;
    }

    let runtime = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Every path out of the write section below — success or error — must
    // still drain the pool's writer task before this function returns, so
    // the section runs as an inner block and the drain happens once, after
    // it, on the captured result.
    let ingest_result: Result<CodeIngestReport> = async {
        runtime_setup(&runtime)?;
        let resolved_ns = runtime.config().default_namespace.clone();
        let token = runtime
            .authorize(resolved_ns)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("failed to authorize namespace")?;

        // One immutable model-name snapshot governs this ingest pass. Each report
        // below is still derived from the corresponding completed embed call, so a
        // provider cannot appear in execution without also appearing in reporting.
        let embedding_model_names = runtime.registered_embedding_model_names();

        let mut report = CodeIngestReport {
            dry_run: false,
            ..CodeIngestReport::default()
        };

        let entities = runtime
            .entities(&token)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for entity in &batch.entities {
            let existing = entities
                .get_entity_including_deleted(entity.id)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if existing.is_some() {
                report.entities_skipped_existing += 1;
                continue;
            }
            report.entities_created += 1;
            entities
                .upsert_entity(entity.clone())
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            let doc = entity_fts_document(entity);
            let embed_body = doc.body.clone();
            if let Ok(fts) = runtime.text(&token) {
                if let Err(e) = fts.upsert_document(doc).await {
                    tracing::warn!(
                        entity_id = %entity.id,
                        error = %e,
                        "code-ingest: entity FTS indexing failed (non-fatal)"
                    );
                }
            }
            for model_name in &embedding_model_names {
                match runtime
                    .embed_document_with_model_outcome(model_name, &embed_body)
                    .await
                {
                    Ok(outcome) => {
                        report
                            .truncation_by_model
                            .entry(model_name.clone())
                            .or_default()
                            .observe(&outcome);
                        if let Ok(vs) = runtime.vectors_for_model(&token, model_name) {
                            if let Err(e) = vs
                                .insert(
                                    entity.id,
                                    SubstrateKind::Entity,
                                    token.namespace().as_str(),
                                    // Canonical field label for the entity body
                                    // vector (khive-runtime/src/operations.rs,
                                    // curation.rs) — must match so vector
                                    // provenance metadata agrees with every
                                    // other write path.
                                    "entity.body",
                                    vec![outcome.vector],
                                )
                                .await
                            {
                                tracing::warn!(
                                    entity_id = %entity.id,
                                    model = %model_name,
                                    error = %e,
                                    "code-ingest: entity vector insert failed (non-fatal)"
                                );
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        entity_id = %entity.id,
                        model = %model_name,
                        error = %e,
                        "code-ingest: entity embedding failed (non-fatal)"
                    ),
                }
            }
        }

        let notes = runtime.notes(&token).map_err(|e| anyhow::anyhow!("{e}"))?;
        for note in &batch.notes {
            let existing = notes
                .get_note_including_deleted(note.id)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if existing.is_some() {
                report.notes_skipped_existing += 1;
                continue;
            }
            report.notes_created += 1;
            notes
                .upsert_note(note.clone())
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            if let Ok(fts) = runtime.text_for_notes(&token) {
                if let Err(e) = fts.upsert_document(note_fts_document(note)).await {
                    tracing::warn!(
                        note_id = %note.id,
                        error = %e,
                        "code-ingest: note FTS indexing failed (non-fatal)"
                    );
                }
            }
            for model_name in &embedding_model_names {
                match runtime
                    .embed_document_with_model_outcome(model_name, &note.content)
                    .await
                {
                    Ok(outcome) => {
                        report
                            .truncation_by_model
                            .entry(model_name.clone())
                            .or_default()
                            .observe(&outcome);
                        if let Ok(vs) = runtime.vectors_for_model(&token, model_name) {
                            if let Err(e) = vs
                                .insert(
                                    note.id,
                                    SubstrateKind::Note,
                                    token.namespace().as_str(),
                                    "note.content",
                                    vec![outcome.vector],
                                )
                                .await
                            {
                                tracing::warn!(
                                    note_id = %note.id,
                                    model = %model_name,
                                    error = %e,
                                    "code-ingest: note vector insert failed (non-fatal)"
                                );
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        note_id = %note.id,
                        model = %model_name,
                        error = %e,
                        "code-ingest: note embedding failed (non-fatal)"
                    ),
                }
            }
        }

        let graph = runtime.graph(&token).map_err(|e| anyhow::anyhow!("{e}"))?;
        for edge in &batch.edges {
            let existing = graph
                .get_edge_including_deleted(edge.id)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if existing.is_some() {
                report.edges_skipped_existing += 1;
                continue;
            }
            report.edges_created += 1;
            graph
                .upsert_edge(edge.clone())
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        Ok(report)
    }
    .await;

    // The migrated stores above route writes through the pool's shared
    // writer task (ADR-067 Component A), which owns its own SQLite
    // connection and exits only after every WriterTaskHandle clone has
    // dropped and the queue has drained. That connection's close fires
    // SQLite's close-time WAL checkpoint, so until the task exits the
    // database file bytes can still move after this function returns. The
    // inner block above dropped every store handle and the token on its way
    // out (success or error); dropping the runtime here closes the queue.
    // Then await the task's exit with a bounded, loud timeout — on BOTH
    // paths, so an early error return cannot leave the file still moving.
    let writer_join = take_writer_task_join_or_warn(runtime.backend().pool());
    drop(runtime);

    settle_writer_drain(ingest_result, writer_join, WRITER_DRAIN_TIMEOUT).await
}

/// Take the pool's writer-task JoinHandle for the drain, warning loudly when
/// a previously stored handle is unavailable at take time.
///
/// A missing handle is benign when no writer-task handle was ever stored: no
/// queue-routed write occurred, or writer-task setup degraded before a handle
/// could be stored. It is a drain-ownership problem only when a handle was
/// stored and another caller already consumed the one-shot slot.
fn take_writer_task_join_or_warn(
    pool: &khive_db::ConnectionPool,
) -> Option<tokio::task::JoinHandle<()>> {
    let writer_join = pool.take_writer_task_join();
    if writer_join.is_none() && pool.write_queue_active() && pool.writer_task_join_was_stored() {
        tracing::warn!(
            "writer-task JoinHandle was stored but is absent at drain time; \
             another caller may have taken the one-shot drain handle, so \
             'return implies settled' is not enforced by this call"
        );
    }
    writer_join
}

/// Await the taken writer-task JoinHandle and reconcile the drain outcome
/// with the ingest outcome — the "return implies settled" contract's
/// enforcement point (extracted from `code_ingest_batch_with_runtime_setup`
/// so each outcome arm is unit-testable with a synthetic handle).
///
/// `timeout` bounds the drain wait (production passes [`WRITER_DRAIN_TIMEOUT`]).
/// Returns the ingest result unchanged on a clean drain, and — when the
/// ingest itself failed — also unchanged on a drain problem (the ingest error
/// is primary; a drain problem is logged, never masks it). Bails when the
/// ingest succeeded but the drain did not settle.
async fn settle_writer_drain(
    ingest_result: Result<CodeIngestReport>,
    writer_join: Option<tokio::task::JoinHandle<()>>,
    timeout: std::time::Duration,
) -> Result<CodeIngestReport> {
    if let Some(join) = writer_join {
        let drained = tokio::time::timeout(timeout, join).await;
        match (&ingest_result, drained) {
            (_, Ok(Ok(()))) => {}
            // The ingest itself failed: that error is the primary one. A
            // drain problem on this path is logged, not returned, so it
            // cannot mask the actual failure.
            (Err(_), Ok(Err(join_err))) => {
                tracing::warn!(error = %join_err, "writer task terminated abnormally after failed ingest");
            }
            (Err(_), Err(_elapsed)) => {
                tracing::warn!(
                    timeout = ?timeout,
                    "writer task did not drain after failed ingest; database file state may still be unsettled"
                );
            }
            (Ok(_), Ok(Err(join_err))) => anyhow::bail!(
                "writer task terminated abnormally after ingest completed: {join_err}"
            ),
            // Pinned behavior (fail-loud is the deliberate choice): the ingest
            // reported success, so some or all of its writes may already be in
            // the file; we bail anyway because "return implies settled" cannot
            // be proven until the writer task has exited. The JoinHandle is
            // consumed by `timeout()` above, so the task DETACHES and keeps
            // running after this bail — its close-time WAL checkpoint may
            // still move the database bytes after this function returns.
            (Ok(_), Err(_elapsed)) => anyhow::bail!(
                "writer task did not drain within {timeout:?} after ingest; \
                 database file state may still be unsettled"
            ),
        }
    }
    ingest_result
}

/// Scan every entity/note content field and nested property value in `batch`
/// through the runtime secret gate, before any storage write is attempted.
/// Mirrors the fields `khive-runtime/src/operations.rs`'s `create_entity`/
/// `create_note_inner` scan (name/description/properties for entities,
/// content/name/properties for notes) so a credential embedded in finding
/// evidence is rejected here exactly as it would be on the shared `create`
/// verb path, rather than persisting verbatim.
fn preflight_secret_gate(batch: &CodeIngestBatch) -> Result<()> {
    for entity in &batch.entities {
        secret_gate::check(&entity.name).map_err(|e| anyhow::anyhow!("{e}"))?;
        if let Some(description) = &entity.description {
            secret_gate::check(description).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        if let Some(properties) = &entity.properties {
            secret_gate::check_json(properties).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        secret_gate::check_tags(&entity.tags).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    for note in &batch.notes {
        secret_gate::check(&note.content).map_err(|e| anyhow::anyhow!("{e}"))?;
        if let Some(name) = &note.name {
            secret_gate::check(name).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        if let Some(properties) = &note.properties {
            secret_gate::check_json(properties).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    Ok(())
}

/// Report what `code_ingest_batch` would create/skip without writing
/// anything.
///
/// When `db_path` is absent, or points at a path that does not yet exist on
/// disk, every record is reported as would-create and nothing is touched —
/// there is no existing state to check identity against, so opening (and
/// thereby creating) a database purely to answer "does this id exist" would
/// itself be the mutation the dry-run contract forbids.
///
/// When the path exists, existence (including soft-deleted rows, because a
/// deterministic ID is never reusable) is checked against a snapshot copy
/// of it: `StorageBackend::sqlite_read_only`'s `SQLITE_OPEN_READ_ONLY` plus
/// `PRAGMA query_only = ON` blocks logical writes, but SQLite still performs
/// ordinary WAL shared-memory maintenance on open, which creates or updates
/// the `-shm` sidecar next to whatever path it is pointed at. Opening the
/// target path directly would therefore still touch it. Instead, the
/// database file (and its `-wal`/`-shm` sidecars, if present — an existing
/// WAL file holds uncheckpointed rows that a plain copy of the main db file
/// alone would miss, and the read-only open requires the shared-memory
/// index beside a non-empty WAL) are copied into a scratch temp directory
/// and marked read-only, and the checks run against that frozen copy. No
/// migrations run and no embedding models are registered, unlike
/// `KhiveRuntime::new`.
async fn dry_run_report(
    db_path: Option<&Path>,
    batch: &CodeIngestBatch,
) -> Result<CodeIngestReport> {
    let mut report = CodeIngestReport {
        dry_run: true,
        ..CodeIngestReport::default()
    };

    let existing_path = db_path.filter(|p| p.exists());
    let Some(db_path) = existing_path else {
        report.entities_created = batch.entities.len() as u64;
        report.notes_created = batch.notes.len() as u64;
        report.edges_created = batch.edges.len() as u64;
        return Ok(report);
    };

    let (backend, _snapshot_dir) = open_read_only_snapshot(db_path)?;
    let sql = backend.sql();
    let mut reader = sql.reader().await.map_err(|e| anyhow::anyhow!("{e}"))?;

    for entity in &batch.entities {
        let row = reader
            .query_scalar(SqlStatement {
                sql: "SELECT 1 FROM entities WHERE id = ?1".to_string(),
                params: vec![SqlValue::Uuid(entity.id)],
                label: Some("code-ingest dry-run entity existence".to_string()),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if row.is_some() {
            report.entities_skipped_existing += 1;
        } else {
            report.entities_created += 1;
        }
    }
    for note in &batch.notes {
        let row = reader
            .query_scalar(SqlStatement {
                sql: "SELECT 1 FROM notes WHERE id = ?1".to_string(),
                params: vec![SqlValue::Uuid(note.id)],
                label: Some("code-ingest dry-run note existence".to_string()),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if row.is_some() {
            report.notes_skipped_existing += 1;
        } else {
            report.notes_created += 1;
        }
    }
    for edge in &batch.edges {
        let row = reader
            .query_scalar(SqlStatement {
                sql: "SELECT 1 FROM graph_edges WHERE id = ?1".to_string(),
                params: vec![SqlValue::Uuid(uuid::Uuid::from(edge.id))],
                label: Some("code-ingest dry-run edge existence".to_string()),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if row.is_some() {
            report.edges_skipped_existing += 1;
        } else {
            report.edges_created += 1;
        }
    }

    Ok(report)
}

/// Copy `db_path` (and its `-wal` sidecar, if present) into a fresh scratch
/// temp directory and open the copy read-only. The caller must keep the
/// returned `TempDir` alive for as long as the backend is used; dropping it
/// deletes the snapshot. Any `-shm` maintenance the read-only open performs
/// lands on this disposable copy, never on `db_path`'s own sidecar.
fn open_read_only_snapshot(db_path: &Path) -> Result<(StorageBackend, tempfile::TempDir)> {
    let snapshot_dir = tempfile::TempDir::new()
        .context("failed to create a scratch directory for the dry-run db snapshot")?;
    let file_name = db_path
        .file_name()
        .with_context(|| format!("{} has no file name component", db_path.display()))?;
    let snapshot_db = snapshot_dir.path().join(file_name);
    std::fs::copy(db_path, &snapshot_db)
        .with_context(|| format!("failed to snapshot {} for dry-run", db_path.display()))?;

    let wal_path = wal_sidecar_path(db_path);
    if wal_path.exists() {
        let snapshot_wal = wal_sidecar_path(&snapshot_db);
        std::fs::copy(&wal_path, &snapshot_wal)
            .with_context(|| format!("failed to snapshot {} for dry-run", wal_path.display()))?;
    }
    // The read-only open below refuses a non-empty WAL sidecar with no
    // shared-memory index beside it (immutable mode would drop committed
    // frames) and refuses a writable one (a live index is not a snapshot).
    // Carry the `-shm` beside the WAL copy and mark both copies read-only:
    // this private point-in-time copy is exactly the frozen snapshot form
    // that open accepts. `fs::copy` preserves the source's (writable)
    // permission bits, so the freeze is required, not decorative.
    let shm_path = shm_sidecar_path(db_path);
    if shm_path.exists() {
        let snapshot_shm = shm_sidecar_path(&snapshot_db);
        std::fs::copy(&shm_path, &snapshot_shm)
            .with_context(|| format!("failed to snapshot {} for dry-run", shm_path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for sidecar in [
            wal_sidecar_path(&snapshot_db),
            shm_sidecar_path(&snapshot_db),
        ] {
            if sidecar.exists() {
                let mut permissions = std::fs::metadata(&sidecar)
                    .with_context(|| format!("stat snapshot sidecar {}", sidecar.display()))?
                    .permissions();
                permissions.set_mode(0o444);
                std::fs::set_permissions(&sidecar, permissions)
                    .with_context(|| format!("freeze snapshot sidecar {}", sidecar.display()))?;
            }
        }
    }

    let backend =
        StorageBackend::sqlite_read_only(&snapshot_db).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((backend, snapshot_dir))
}

/// The `-wal` sidecar path SQLite uses alongside a WAL-mode database file.
fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// The `-shm` shared-memory index path SQLite uses alongside a WAL-mode
/// database file.
fn shm_sidecar_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push("-shm");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use khive_runtime::{EmbedderProvider, RuntimeError};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};
    use serial_test::serial;

    use super::*;

    struct WriteQueueEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl WriteQueueEnvGuard {
        fn unset() -> Self {
            let previous = std::env::var_os("KHIVE_WRITE_QUEUE");
            std::env::remove_var("KHIVE_WRITE_QUEUE");
            Self { previous }
        }
    }

    impl Drop for WriteQueueEnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("KHIVE_WRITE_QUEUE", value),
                None => std::env::remove_var("KHIVE_WRITE_QUEUE"),
            }
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct MakeCapture(Capture);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeCapture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.0.clone()
        }
    }

    struct FixedEmbeddingService {
        dimensions: usize,
    }

    #[async_trait]
    impl EmbeddingService for FixedEmbeddingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|_| vec![1.0_f32; self.dimensions])
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "code-ingest-test"
        }
    }

    struct FixedEmbeddingProvider {
        name: String,
        dimensions: usize,
    }

    #[async_trait]
    impl EmbedderProvider for FixedEmbeddingProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn dimensions(&self) -> usize {
            self.dimensions
        }

        async fn build(&self) -> std::result::Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(FixedEmbeddingService {
                dimensions: self.dimensions,
            }))
        }
    }

    fn base_args(findings: PathBuf, db: PathBuf) -> CodeIngestArgs {
        CodeIngestArgs {
            findings,
            source_run: Some("test-run".to_string()),
            db: Some(db.display().to_string()),
            namespace: "local".to_string(),
            dry_run: false,
            human: false,
        }
    }

    fn write_valid_findings(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("findings.json");
        std::fs::write(
            &path,
            r#"{
                "audit": {
                    "date": "2026-07-11",
                    "scope": "khive-pack-code",
                    "repo": "ohdearquant/khive",
                    "branch": "feat/adr085-code-ingest-admin",
                    "commit": "abc1234",
                    "standards_file": "docs/standards.md"
                },
                "findings": [
                    {
                        "id": "F-001",
                        "title": "Example finding for a CLI integration test",
                        "severity": "medium",
                        "confidence": "high",
                        "failure_scenario": "Reproduced by running kkernel code-ingest twice.",
                        "evidence": "code_ingest.rs test",
                        "impact": "none, this is a test fixture"
                    }
                ]
            }"#,
        )
        .expect("write findings.json fixture");
        path
    }

    const TOMBSTONE_WITNESS: i64 = 1_772_812_800_000_000;

    fn mapped_batch(findings: &Path) -> CodeIngestBatch {
        let bytes = std::fs::read(findings).expect("read findings fixture");
        ingest_findings_json(
            &bytes,
            CodeIngestOptions {
                namespace: "local",
                observed_at: Utc::now(),
                source_run: Some("test-run"),
            },
        )
        .expect("map valid findings fixture")
    }

    async fn soft_delete_mapped_batch(db: &Path, batch: &CodeIngestBatch) {
        let backend = StorageBackend::sqlite(db).expect("open tombstone writer");
        let sql = backend.sql();
        let mut writer = sql.writer().await.expect("acquire tombstone writer");

        let rows = [
            (
                "entities",
                SqlValue::Uuid(batch.entities[0].id),
                "soft-delete mapped entity",
            ),
            (
                "notes",
                SqlValue::Uuid(batch.notes[0].id),
                "soft-delete mapped note",
            ),
            (
                "graph_edges",
                SqlValue::Uuid(uuid::Uuid::from(batch.edges[0].id)),
                "soft-delete mapped edge",
            ),
        ];

        for (table, id, label) in rows {
            let changed = writer
                .execute(SqlStatement {
                    sql: format!("UPDATE {table} SET deleted_at = ?1 WHERE id = ?2"),
                    params: vec![SqlValue::Integer(TOMBSTONE_WITNESS), id],
                    label: Some(label.to_string()),
                })
                .await
                .expect("soft-delete mapped row");
            assert_eq!(changed, 1, "fixture must tombstone exactly one {table} row");
        }
    }

    async fn assert_mapped_batch_remains_tombstoned(db: &Path, batch: &CodeIngestBatch) {
        let backend = StorageBackend::sqlite_read_only(db).expect("open tombstone reader");
        let sql = backend.sql();
        let mut reader = sql.reader().await.expect("acquire tombstone reader");

        let rows = [
            ("entities", SqlValue::Uuid(batch.entities[0].id)),
            ("notes", SqlValue::Uuid(batch.notes[0].id)),
            (
                "graph_edges",
                SqlValue::Uuid(uuid::Uuid::from(batch.edges[0].id)),
            ),
        ];

        for (table, id) in rows {
            let marker = reader
                .query_scalar(SqlStatement {
                    sql: format!("SELECT deleted_at FROM {table} WHERE id = ?1"),
                    params: vec![id],
                    label: Some(format!("read mapped {table} tombstone")),
                })
                .await
                .expect("read mapped tombstone");
            assert!(
                matches!(&marker, Some(SqlValue::Integer(value)) if *value == TOMBSTONE_WITNESS),
                "re-ingest must preserve the exact {table} tombstone marker, got {marker:?}"
            );
        }
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_creates_once_then_skips_on_rerun() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("scratch.db");

        let first = code_ingest_batch(base_args(findings.clone(), db.clone()))
            .await
            .expect("first ingest must succeed");
        assert_eq!(first.entities_created, 1);
        assert_eq!(first.notes_created, 1);
        assert_eq!(first.edges_created, 1);
        assert_eq!(first.entities_skipped_existing, 0);
        assert_eq!(first.notes_skipped_existing, 0);
        assert_eq!(first.edges_skipped_existing, 0);

        let second = code_ingest_batch(base_args(findings, db))
            .await
            .expect("re-ingesting the same sweep must succeed");
        assert_eq!(
            second.notes_created, 0,
            "content-derived ids must make a re-ingest a no-op, not a duplicate write"
        );
        assert_eq!(second.notes_skipped_existing, 1);
        assert_eq!(second.entities_skipped_existing, 1);
        assert_eq!(second.edges_skipped_existing, 1);
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_never_reactivates_consumed_tombstone_ids() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("tombstones.db");
        let batch = mapped_batch(&findings);

        code_ingest_batch(base_args(findings.clone(), db.clone()))
            .await
            .expect("initial ingest must succeed");
        soft_delete_mapped_batch(&db, &batch).await;
        assert_mapped_batch_remains_tombstoned(&db, &batch).await;

        let mut dry_args = base_args(findings.clone(), db.clone());
        dry_args.dry_run = true;
        let dry = code_ingest_batch(dry_args)
            .await
            .expect("dry-run over tombstones must succeed");
        assert!(dry.dry_run);
        assert_eq!(dry.entities_created, 0);
        assert_eq!(dry.entities_skipped_existing, 1);
        assert_eq!(dry.notes_created, 0);
        assert_eq!(dry.notes_skipped_existing, 1);
        assert_eq!(dry.edges_created, 0);
        assert_eq!(dry.edges_skipped_existing, 1);
        assert_mapped_batch_remains_tombstoned(&db, &batch).await;

        let real = code_ingest_batch(base_args(findings, db.clone()))
            .await
            .expect("real re-ingest over tombstones must succeed");
        assert!(!real.dry_run);
        assert_eq!(real.entities_created, 0);
        assert_eq!(real.entities_skipped_existing, 1);
        assert_eq!(real.notes_created, 0);
        assert_eq!(real.notes_skipped_existing, 1);
        assert_eq!(real.edges_created, 0);
        assert_eq!(real.edges_skipped_existing, 1);
        assert_mapped_batch_remains_tombstoned(&db, &batch).await;
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_with_no_queue_writes_does_not_warn_on_drain() {
        let _write_queue_env = WriteQueueEnvGuard::unset();
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("no_queue_writes.db");

        code_ingest_batch(base_args(findings.clone(), db.clone()))
            .await
            .expect("initial ingest must succeed");

        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(MakeCapture(capture.clone()))
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let report = code_ingest_batch(base_args(findings, db))
            .await
            .expect("all-skipped ingest must succeed");
        drop(guard);

        assert_eq!(report.entities_created, 0);
        assert_eq!(report.notes_created, 0);
        assert_eq!(report.edges_created, 0);
        let log = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
        assert!(
            !log.contains("writer-task JoinHandle"),
            "an ingest with no queue-routed writes must not warn at drain time: {log}"
        );
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_dry_run_writes_nothing() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("scratch.db");

        let mut args = base_args(findings, db.clone());
        args.dry_run = true;
        let report = code_ingest_batch(args)
            .await
            .expect("dry-run must validate successfully");
        assert!(report.dry_run);
        assert_eq!(
            report.notes_created, 1,
            "dry-run still reports what would be created"
        );

        assert_eq!(
            finding_note_count(&db).await,
            0,
            "a dry run must never persist the finding note"
        );
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_rejects_invalid_document_before_any_write() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("bad.json");
        std::fs::write(
            &path,
            r#"{
                "audit": {
                    "date": "2026-07-11",
                    "scope": "x",
                    "repo": "r",
                    "branch": "b",
                    "commit": "c",
                    "standards_file": "s"
                },
                "findings": [
                    {"id": "F-002", "title": "bad", "severity": "high", "confidence": "low"}
                ]
            }"#,
        )
        .expect("write invalid fixture");
        let db = tmp.path().join("scratch.db");

        let err = code_ingest_batch(base_args(path, db.clone()))
            .await
            .expect_err("missing failure_scenario for a high-severity finding must be rejected");
        assert!(
            err.to_string().contains("failed validation"),
            "error must name the failing document: {err}"
        );
        assert_eq!(
            finding_note_count(&db).await,
            0,
            "whole-document validation must reject the sweep before any finding note is written"
        );
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_dry_run_against_nonexistent_db_creates_no_file() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("does-not-exist.db");
        assert!(!db.exists());

        let mut args = base_args(findings, db.clone());
        args.dry_run = true;
        let report = code_ingest_batch(args).await.expect("dry-run must succeed");
        assert!(report.dry_run);
        assert_eq!(report.entities_created, 1);
        assert_eq!(report.notes_created, 1);
        assert_eq!(report.edges_created, 1);
        assert!(
            !db.exists(),
            "a dry run against a nonexistent db path must not create it"
        );
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_dry_run_against_existing_db_does_not_mutate_it() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("scratch.db");

        // Populate the db for real first so it exists on disk.
        code_ingest_batch(base_args(findings.clone(), db.clone()))
            .await
            .expect("initial ingest must succeed");
        let bytes_before = std::fs::read(&db).expect("read db bytes before dry run");

        let mut args = base_args(findings, db.clone());
        args.dry_run = true;
        let report = code_ingest_batch(args)
            .await
            .expect("dry-run against an existing db must succeed");
        assert!(report.dry_run);
        assert_eq!(
            report.entities_skipped_existing, 1,
            "the record from the prior real ingest must be reported as already existing"
        );
        assert_eq!(report.notes_skipped_existing, 1);
        assert_eq!(report.edges_skipped_existing, 1);

        let bytes_after = std::fs::read(&db).expect("read db bytes after dry run");
        assert_eq!(
            bytes_before, bytes_after,
            "a dry run against an existing db must not change a single byte of it"
        );
    }

    /// The contract the writer-task drain in `code_ingest_batch` exists to
    /// provide, pinned as its own test: a real ingest's RETURN implies the
    /// database file state is settled. Without the drain, the pool's writer
    /// task exits asynchronously after the last `WriterTaskHandle` clone
    /// drops, and its connection's close-time WAL checkpoint moves the file
    /// bytes after `code_ingest_batch` has already returned — which is the
    /// race the sibling dry-run byte test used to lose. That test passing
    /// is a consequence of this contract, not a substitute for it.
    #[serial]
    #[tokio::test]
    async fn code_ingest_return_implies_settled_file_state() {
        // Pin the queue default: with the variable unset, a file-backed pool
        // resolves the write queue ON, so this test exercises the writer-task
        // drain rather than silently passing through the queue-off path an
        // ambient KHIVE_WRITE_QUEUE=0 would select. (#[serial] guards the
        // env mutation.)
        let _write_queue_env = WriteQueueEnvGuard::unset();
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("settled.db");

        code_ingest_batch(base_args(findings, db.clone()))
            .await
            .expect("real ingest must succeed");

        // Every connection — the pool's synchronous ones and the writer
        // task's own — must be closed by the time the call returns, so
        // SQLite's last-close checkpoint has already run and removed the
        // WAL sidecars.
        let wal_path = wal_sidecar_path(&db);
        let shm_path = shm_sidecar_path(&db);
        assert!(
            !wal_path.exists(),
            "ingest return must imply the -wal sidecar was checkpointed and removed"
        );
        assert!(
            !shm_path.exists(),
            "ingest return must imply the -shm sidecar was removed"
        );

        let bytes_at_return = std::fs::read(&db).expect("read db at the return boundary");

        // Give any (incorrectly) still-pending async writer a generous
        // window: if the writer task were still alive past the return
        // boundary, its close-time checkpoint would move these bytes.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let bytes_later = std::fs::read(&db).expect("re-read db after the settle window");
        assert_eq!(
            bytes_at_return, bytes_later,
            "no byte of the database may move after code_ingest_batch has returned"
        );
    }

    /// The `-shm` sidecar path SQLite uses alongside a WAL-mode database file.
    fn shm_sidecar_path(db_path: &std::path::Path) -> PathBuf {
        let mut name = db_path.as_os_str().to_owned();
        name.push("-shm");
        PathBuf::from(name)
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_dry_run_against_existing_wal_db_leaves_sidecars_untouched() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("wal_scratch.db");

        // Populate the db for real first so it exists on disk in WAL mode.
        code_ingest_batch(base_args(findings.clone(), db.clone()))
            .await
            .expect("initial ingest must succeed");

        // Hold a live writer connection open across the dry run below, with
        // one uncheckpointed write on it, so the target's `-wal`/`-shm`
        // sidecars are guaranteed present with real content going into the
        // Dry run of the "existing WAL database" scenario
        // reproduced against (e.g. a live daemon holding the db open while
        // an admin separately runs `code-ingest --dry-run`).
        let pin = StorageBackend::sqlite(&db).expect("open pin backend");
        {
            let sql = pin.sql();
            let mut writer = sql.writer().await.expect("pin writer");
            writer
                .execute_script(
                    "CREATE TABLE IF NOT EXISTS wal_pin_probe(x INTEGER); \
                     INSERT INTO wal_pin_probe VALUES (1);"
                        .to_string(),
                )
                .await
                .expect("pin write to keep the wal open");
        }

        let wal_path = wal_sidecar_path(&db);
        let shm_path = shm_sidecar_path(&db);
        assert!(
            wal_path.exists(),
            "expected a live -wal sidecar before dry-run"
        );
        assert!(
            shm_path.exists(),
            "expected a live -shm sidecar before dry-run"
        );

        let db_before = std::fs::read(&db).expect("read db before dry run");
        let wal_before = std::fs::read(&wal_path).expect("read -wal before dry run");
        let shm_before = std::fs::read(&shm_path).expect("read -shm before dry run");

        let mut args = base_args(findings, db.clone());
        args.dry_run = true;
        let report = code_ingest_batch(args)
            .await
            .expect("dry-run against an existing WAL db must succeed");
        assert!(report.dry_run);

        assert!(
            wal_path.exists(),
            "the existing -wal sidecar must not disappear"
        );
        assert!(
            shm_path.exists(),
            "the existing -shm sidecar must not disappear"
        );

        let db_after = std::fs::read(&db).expect("read db after dry run");
        let wal_after = std::fs::read(&wal_path).expect("read -wal after dry run");
        let shm_after = std::fs::read(&shm_path).expect("read -shm after dry run");

        assert_eq!(
            db_before, db_after,
            "dry-run must not touch the main db file"
        );
        assert_eq!(
            wal_before, wal_after,
            "dry-run must not touch the existing -wal sidecar"
        );
        assert_eq!(
            shm_before, shm_after,
            "dry-run must not touch the existing -shm sidecar"
        );

        drop(pin);
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_rejects_secret_bearing_evidence_before_any_write() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("secret.json");
        std::fs::write(
            &path,
            r#"{
                "audit": {
                    "date": "2026-07-11",
                    "scope": "khive-pack-code",
                    "repo": "ohdearquant/khive",
                    "branch": "feat/adr085-code-ingest-admin",
                    "commit": "abc1234",
                    "standards_file": "docs/standards.md"
                },
                "findings": [
                    {
                        "id": "F-003",
                        "title": "Example finding carrying a leaked credential",
                        "severity": "high",
                        "confidence": "high",
                        "failure_scenario": "A scanner captured a live AWS key in evidence.",
                        "evidence": "AKIAFAKEKEY1234567890",
                        "impact": "credential AKIAFAKEKEY1234567890 must never persist verbatim"
                    }
                ]
            }"#,
        )
        .expect("write secret-bearing fixture");
        let db = tmp.path().join("scratch.db");

        let err = code_ingest_batch(base_args(path, db.clone()))
            .await
            .expect_err("a secret-shaped evidence value must be rejected before any write");
        assert!(
            err.to_string().to_lowercase().contains("secret"),
            "error must name the secret-gate rejection: {err}"
        );
        assert!(
            !db.exists(),
            "rejecting a secret-bearing document must leave the db path untouched"
        );
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_fails_loud_when_code_pack_not_configured() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("scratch.db");

        let prior = std::env::var("KHIVE_PACKS").ok();
        // SAFETY: `#[tokio::test]` gives each test its own single-threaded
        // runtime, but process env is still global across the test binary;
        // this mirrors the same accepted pattern (and its safety rationale)
        // used by `default_config_packs_loads_all_production_packs` in
        // `khive-runtime/src/runtime.rs`, restored in a `finally`-style tail.
        unsafe {
            std::env::set_var("KHIVE_PACKS", "kg");
        }
        let result = code_ingest_batch(base_args(findings, db.clone())).await;
        unsafe {
            match &prior {
                Some(v) => std::env::set_var("KHIVE_PACKS", v),
                None => std::env::remove_var("KHIVE_PACKS"),
            }
        }

        let err = result.expect_err("a pack set without `code` must be rejected");
        assert!(
            err.to_string().contains("code"),
            "error must name the missing `code` pack: {err}"
        );
        assert!(
            !db.exists(),
            "rejecting a misconfigured pack set must leave the db path untouched"
        );
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_entity_vector_uses_canonical_body_field_label() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("scratch.db");

        code_ingest_batch_with_runtime_setup(base_args(findings, db.clone()), |runtime| {
            let model_names = runtime.registered_embedding_model_names();
            assert!(
                !model_names.is_empty(),
                "test requires at least one configured embedding model"
            );
            for name in model_names {
                let dimensions = runtime.resolve_embedding_model(Some(&name))?.dimensions();
                runtime.register_embedder(FixedEmbeddingProvider { name, dimensions });
            }
            Ok(())
        })
        .await
        .expect("ingest must succeed");

        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(db.to_str().expect("utf8 path")),
            config: None,
            namespace: Namespace::parse("local").expect("valid namespace"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve runtime config");
        let runtime = KhiveRuntime::new(cfg).expect("runtime");
        let sql = runtime.sql();
        let mut reader = sql.reader().await.expect("reader");
        let tables = reader
            .query_all(SqlStatement {
                sql: "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'vec_%'"
                    .to_string(),
                params: vec![],
                label: None,
            })
            .await
            .expect("list vec tables");
        assert!(
            !tables.is_empty(),
            "expected at least one vector table after ingest"
        );

        // sqlite-vec creates companion/shadow tables (e.g. `_info`, vec0
        // virtual-table internals) alongside the real vector row table, so a
        // bare `LIKE 'vec_%'` sweep must tolerate tables that don't carry a
        // `field` column rather than assuming every match is a row table.
        let mut saw_entity_row = false;
        for table in &tables {
            let table_name = match table.get("name") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected table name column: {other:?}"),
            };
            let Ok(rows) = reader
                .query_all(SqlStatement {
                    sql: format!("SELECT field FROM {table_name} WHERE kind = 'entity'"),
                    params: vec![],
                    label: None,
                })
                .await
            else {
                continue;
            };
            for row in rows {
                if let Some(SqlValue::Text(field)) = row.get("field") {
                    assert_eq!(
                        field, "entity.body",
                        "entity vector metadata must use the canonical 'entity.body' field \
                         label to match khive-runtime/src/operations.rs, got {field:?}"
                    );
                    saw_entity_row = true;
                }
            }
        }
        assert!(saw_entity_row, "expected at least one entity vector row");
    }

    #[serial]
    #[tokio::test]
    async fn code_ingest_reports_actual_embedding_truncation_by_model() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("scratch.db");

        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&findings).expect("read findings fixture"))
                .expect("parse findings fixture");
        // Finding-note embeddings use the canonical `title: impact` content,
        // while evidence remains structured properties only.
        document["findings"][0]["impact"] =
            serde_json::Value::String("x".repeat(MAX_TEXT_BYTES + 1));
        std::fs::write(
            &findings,
            serde_json::to_vec(&document).expect("serialize long findings fixture"),
        )
        .expect("write long findings fixture");

        let report = code_ingest_batch_with_runtime_setup(base_args(findings, db), |runtime| {
            for name in runtime.registered_embedding_model_names() {
                let dimensions = runtime.resolve_embedding_model(Some(&name))?.dimensions();
                runtime.register_embedder(FixedEmbeddingProvider { name, dimensions });
            }
            Ok(())
        })
        .await
        .expect("ingest with bounded embedding input must succeed");

        assert!(
            !report.truncation_by_model.is_empty(),
            "configured models that received embeddings must appear in the report"
        );
        assert!(
            report
                .truncation_by_model
                .values()
                .any(|truncation| { truncation.truncated > 0 && truncation.discarded_bytes > 0 }),
            "the report must reflect the long finding content actually bounded by an embedder: \
             {:?}",
            report.truncation_by_model
        );
    }

    /// The drain-finalization matrix, tested through the extracted
    /// `settle_writer_drain` with synthetic JoinHandles: the real writer
    /// task never produces a join ERROR (every documented failure mode —
    /// op panic, commit failure, poisoned connection — is caught inside the
    /// request wrapper or exits the task normally; see writer_task.rs's
    /// `run_writer_task`), and a real drain TIMEOUT would need to exceed the
    /// 30s production bound, so those two success-path arms are not
    /// constructible end-to-end with standing infra. The arms below pin the
    /// contract the production drain relies on.
    fn ok_report() -> CodeIngestReport {
        CodeIngestReport {
            dry_run: false,
            ..CodeIngestReport::default()
        }
    }

    /// 3(b): a join ERROR after a successful ingest surfaces as a bail whose
    /// message names the writer task.
    #[tokio::test]
    async fn settle_writer_drain_join_error_after_success_bails_naming_writer_task() {
        let join = tokio::spawn(async {
            panic!("synthetic writer task explosion");
        });
        let err = settle_writer_drain(
            Ok(ok_report()),
            Some(join),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect_err("a panicked writer task must fail the settled-file contract");
        let msg = err.to_string();
        assert!(
            msg.contains("writer task terminated abnormally after ingest completed"),
            "the error must name the writer task: {msg}"
        );
    }

    /// 3(c): a drain TIMEOUT after a successful ingest bails — pinning the
    /// current fail-loud behavior (report discarded, JoinHandle consumed by
    /// `timeout()` so the task detaches; see the bail-site doc comment).
    #[tokio::test]
    async fn settle_writer_drain_timeout_after_success_bails() {
        let join = tokio::spawn(std::future::pending::<()>());
        let err = settle_writer_drain(
            Ok(ok_report()),
            Some(join),
            std::time::Duration::from_millis(50),
        )
        .await
        .expect_err("an undrained writer task must fail the settled-file contract");
        let msg = err.to_string();
        assert!(
            msg.contains("did not drain within"),
            "the error must name the drain timeout: {msg}"
        );
    }

    /// 3(a) unit half: when the ingest itself failed, the PRIMARY ingest
    /// error is what surfaces — a join error on the drain is logged, not
    /// substituted.
    #[tokio::test]
    async fn settle_writer_drain_failed_ingest_error_survives_join_error() {
        let join = tokio::spawn(async {
            panic!("synthetic writer task explosion");
        });
        let primary: Result<CodeIngestReport> = Err(anyhow::anyhow!("primary ingest failure"));
        let err = settle_writer_drain(primary, Some(join), std::time::Duration::from_secs(5))
            .await
            .expect_err("the primary ingest error must surface");
        assert_eq!(err.to_string(), "primary ingest failure");
    }

    /// 3(a) unit half: same for a drain timeout on the failure path — the
    /// ingest error is primary, the timeout is logged only.
    #[tokio::test]
    async fn settle_writer_drain_failed_ingest_error_survives_timeout() {
        let join = tokio::spawn(std::future::pending::<()>());
        let primary: Result<CodeIngestReport> = Err(anyhow::anyhow!("primary ingest failure"));
        let err = settle_writer_drain(primary, Some(join), std::time::Duration::from_millis(50))
            .await
            .expect_err("the primary ingest error must surface");
        assert_eq!(err.to_string(), "primary ingest failure");
    }

    /// Clean-drain passthrough: a settled task returns the report unchanged,
    /// and a missing handle (queue off / never spawned) is a no-op.
    #[tokio::test]
    async fn settle_writer_drain_clean_passes_report_through() {
        let join = tokio::spawn(async {});
        let out = settle_writer_drain(
            Ok(ok_report()),
            Some(join),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("clean drain must pass the report through");
        assert!(!out.dry_run);
        let out = settle_writer_drain(Ok(ok_report()), None, std::time::Duration::from_secs(5))
            .await
            .expect("no handle means nothing to await");
        assert!(!out.dry_run);
    }

    /// 2: the already-taken arm of `take_writer_task_join_or_warn` — a
    /// file-backed, queue-enabled pool whose JoinHandle was already taken
    /// must get `None` back AND a loud warning naming the contract gap.
    #[serial]
    #[tokio::test]
    async fn take_writer_task_join_or_warn_already_taken_warns_loudly() {
        // The helper reads the resolved config only, but the pool's
        // `PoolConfig::default()` reads KHIVE_WRITE_QUEUE at construction, so
        // pin the variable unset to get the file-backed queue-ON default.
        let _write_queue_env = WriteQueueEnvGuard::unset();

        let dir = tempfile::TempDir::new().expect("temp dir");
        let pool = khive_db::ConnectionPool::new(khive_db::PoolConfig {
            path: Some(dir.path().join("already_taken.db")),
            ..khive_db::PoolConfig::default()
        })
        .expect("file-backed pool should open");
        assert!(
            pool.write_queue_active(),
            "unset preference on a file-backed pool must resolve the queue ON"
        );
        pool.writer_task_handle()
            .expect("runtime is present")
            .expect("queue-ON file-backed pool must spawn");
        let taken = pool
            .take_writer_task_join()
            .expect("the first take must return the handle");

        // Capture the tracing output of the second (already-taken) attempt.
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(MakeCapture(capture.clone()))
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let second = take_writer_task_join_or_warn(&pool);
        drop(guard);

        assert!(
            second.is_none(),
            "the one-shot take must yield None once the handle is gone"
        );
        let log = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
        assert!(
            log.contains("JoinHandle was stored but is absent"),
            "the already-taken arm must warn loudly about the drain contract gap: {log}"
        );

        // Clean exit: await the taken handle (drop the pool first so the
        // writer task's last handle clone goes and the task can exit).
        drop(pool);
        tokio::time::timeout(std::time::Duration::from_secs(5), taken)
            .await
            .expect("writer task must exit once every handle clone is dropped")
            .expect("writer task must not panic");
    }

    /// 3(a) end-to-end half: a real ingest that fails mid-write still drains
    /// the writer task before returning, and the PRIMARY ingest error — not
    /// any drain outcome — is what surfaces. A `BEFORE INSERT` trigger on
    /// `notes` is installed in `runtime_setup` so the entity writes land
    /// (spawning the writer task) and the finding-note upsert then fails.
    /// (A trigger, not `DROP TABLE`: `notes_for_namespace` re-runs
    /// `ensure_notes_schema` on every store acquisition, which would silently
    /// heal a dropped table — see crates/khive-db/src/backend.rs:229-230.)
    #[serial]
    #[tokio::test]
    async fn code_ingest_failed_ingest_still_drains_and_surfaces_primary_error() {
        let _write_queue_env = WriteQueueEnvGuard::unset();

        let tmp = tempfile::TempDir::new().expect("temp dir");
        let findings = write_valid_findings(tmp.path());
        let db = tmp.path().join("failed_ingest_drain.db");

        let err =
            code_ingest_batch_with_runtime_setup(base_args(findings, db.clone()), |runtime| {
                // Sabotage AFTER migrations but BEFORE any store is acquired:
                // entity writes succeed through the writer task, then the
                // finding-note INSERT trips the trigger and fails the ingest.
                let writer = runtime
                    .backend()
                    .pool()
                    .try_writer()
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                writer
                    .execute_batch(
                        "CREATE TRIGGER code_ingest_test_block_notes \
                     BEFORE INSERT ON notes \
                     BEGIN SELECT RAISE(ABORT, 'synthetic notes insert failure'); END",
                    )
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            })
            .await
            .expect_err("ingest into a trigger-blocked notes table must fail");

        let msg = err.to_string();
        assert!(
            msg.contains("synthetic notes insert failure"),
            "the PRIMARY ingest error (the notes write) must surface, got: {msg}"
        );
        assert!(
            !msg.contains("writer task did not drain")
                && !msg.contains("writer task terminated abnormally"),
            "a drain problem must not mask the primary ingest error: {msg}"
        );

        // Drain proof: every connection — the pool's and the writer task's —
        // is closed by return time, so SQLite's last-close checkpoint has
        // removed the WAL sidecar even though the ingest failed mid-batch.
        assert!(
            !wal_sidecar_path(&db).exists(),
            "a failed ingest must still drain the writer task and settle the file"
        );
        assert!(!shm_sidecar_path(&db).exists());
    }

    /// Query the persisted `finding` note count for a scratch db, independent
    /// of any in-process `CodeIngestReport`, proving what was actually
    /// written to storage rather than trusting the report alone.
    async fn finding_note_count(db: &std::path::Path) -> u64 {
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(db.to_str().expect("utf8 path")),
            config: None,
            namespace: Namespace::parse("local").expect("valid namespace"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve runtime config");
        let runtime = KhiveRuntime::new(cfg).expect("runtime");
        let token = runtime
            .authorize(runtime.config().default_namespace.clone())
            .expect("authorize");
        runtime
            .notes(&token)
            .expect("notes store")
            .count_notes("local", Some("finding"))
            .await
            .expect("count notes")
    }
}
