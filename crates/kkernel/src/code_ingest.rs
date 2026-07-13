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
}

/// Run one `kkernel code-ingest` pass: resolve config, validate the
/// `findings.json` document as a whole (fail-closed, before any write), then
/// persist the deterministic entity/note/edge batch record-by-record.
/// Records whose content-derived ID already exists are reported as skipped,
/// not overwritten: a `finding` note's lifecycle state (`kind_status`) is
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
    let resolved_ns = runtime.config().default_namespace.clone();
    let token = runtime
        .authorize(resolved_ns)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to authorize namespace")?;

    let mut report = CodeIngestReport {
        dry_run: false,
        ..CodeIngestReport::default()
    };

    let entities = runtime
        .entities(&token)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    for entity in &batch.entities {
        let existing = entities
            .get_entity(entity.id)
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
        for model_name in runtime.registered_embedding_model_names() {
            match runtime
                .embed_document_with_model(&model_name, &embed_body)
                .await
            {
                Ok(vector) => {
                    if let Ok(vs) = runtime.vectors_for_model(&token, &model_name) {
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
                                vec![vector],
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
            .get_note(note.id)
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
        for model_name in runtime.registered_embedding_model_names() {
            match runtime
                .embed_document_with_model(&model_name, &note.content)
                .await
            {
                Ok(vector) => {
                    if let Ok(vs) = runtime.vectors_for_model(&token, &model_name) {
                        if let Err(e) = vs
                            .insert(
                                note.id,
                                SubstrateKind::Note,
                                token.namespace().as_str(),
                                "note.content",
                                vec![vector],
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
            .get_edge(edge.id)
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
/// When the path exists, existence is checked against a snapshot copy of
/// it: `StorageBackend::sqlite_read_only`'s `SQLITE_OPEN_READ_ONLY` plus
/// `PRAGMA query_only = ON` blocks logical writes, but SQLite still performs
/// ordinary WAL shared-memory maintenance on open, which creates or updates
/// the `-shm` sidecar next to whatever path it is pointed at. Opening the
/// target path directly would therefore still touch it. Instead, the
/// database file (and its `-wal` sidecar, if one exists — an existing WAL
/// file holds uncheckpointed rows that a plain copy of the main db file
/// alone would miss) are copied into a scratch temp directory first, and
/// the read-only checks run against that copy. No migrations run and no
/// embedding models are registered, unlike `KhiveRuntime::new`.
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
                sql: "SELECT 1 FROM entities WHERE id = ?1 AND deleted_at IS NULL".to_string(),
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
                sql: "SELECT 1 FROM notes WHERE id = ?1 AND deleted_at IS NULL".to_string(),
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
                sql: "SELECT 1 FROM graph_edges WHERE id = ?1 AND deleted_at IS NULL".to_string(),
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

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

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

        code_ingest_batch(base_args(findings, db.clone()))
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
