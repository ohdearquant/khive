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

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use serde::Serialize;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_pack_code::{ingest_findings_json, CodeIngestOptions};
use khive_runtime::{entity_fts_document, note_fts_document, KhiveRuntime, Namespace};
use khive_storage::SubstrateKind;

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

    let runtime = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let resolved_ns = runtime.config().default_namespace.clone();
    let token = runtime
        .authorize(resolved_ns)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to authorize namespace")?;

    // Whole-document validation before any write (fail-closed): a malformed
    // findings.json returns Err here and the process exits nonzero without
    // touching storage.
    let batch = ingest_findings_json(
        &bytes,
        CodeIngestOptions {
            namespace: token.namespace().as_str(),
            observed_at: Utc::now(),
            source_run: args.source_run.as_deref(),
        },
    )
    .with_context(|| format!("{} failed validation", args.findings.display()))?;

    let mut report = CodeIngestReport {
        dry_run: args.dry_run,
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
        if args.dry_run {
            continue;
        }
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
                                "entity.name",
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
        if args.dry_run {
            continue;
        }
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
        if args.dry_run {
            continue;
        }
        graph
            .upsert_edge(edge.clone())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
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
