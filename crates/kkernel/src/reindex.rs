//! `kkernel reindex` — rebuild embedding vectors and FTS documents for entities and notes.
//!
//! This is an infrastructure-level operation that walks all entities and notes
//! in a database and (re-)embeds them using the specified model and backfills the
//! FTS index. It is NOT a pack verb — it operates on the raw runtime stores
//! regardless of which packs are loaded.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use uuid::Uuid;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_runtime::retrieval::EmbeddingTruncationReport;
use khive_runtime::{
    entity_embedding_text, entity_fts_document, note_embedding_text, note_fts_document,
    KhiveConfig, KhiveRuntime, Namespace,
};
use khive_storage::entity::Entity;
use khive_storage::error::StorageError;
use khive_storage::note::Note;
use khive_storage::types::VectorRecord;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;

// ─── progress bar ─────────────────────────────────────────────────────────────

struct ProgressBar {
    label: &'static str,
    start: Instant,
    current: AtomicU64,
    total: AtomicU64,
    window_current: AtomicU64,
    window_nanos: AtomicU64,
    rate: std::sync::Mutex<f64>,
}

const RATE_WINDOW_SECS: f64 = 10.0;

impl ProgressBar {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            start: Instant::now(),
            current: AtomicU64::new(0),
            total: AtomicU64::new(0),
            window_current: AtomicU64::new(0),
            window_nanos: AtomicU64::new(0),
            rate: std::sync::Mutex::new(0.0),
        }
    }

    fn update(&self, current: u64, total: u64) {
        self.current.store(current, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);

        let now_ns = self.start.elapsed().as_nanos() as u64;
        let prev_ns = self.window_nanos.load(Ordering::Relaxed);
        let delta_secs = (now_ns - prev_ns) as f64 / 1e9;

        if delta_secs >= RATE_WINDOW_SECS {
            let prev_current = self.window_current.load(Ordering::Relaxed);
            let delta_items = current.saturating_sub(prev_current);
            if delta_secs > 0.1 {
                let window_rate = delta_items as f64 / delta_secs;
                if let Ok(mut r) = self.rate.lock() {
                    if *r < 0.1 {
                        *r = window_rate;
                    } else {
                        *r = 0.3 * *r + 0.7 * window_rate;
                    }
                }
            }
            self.window_current.store(current, Ordering::Relaxed);
            self.window_nanos.store(now_ns, Ordering::Relaxed);
        }

        self.render();
    }

    fn render(&self) {
        use std::io::Write;
        let current = self.current.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let pct = if total > 0 {
            (current as f64 / total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        const BAR_WIDTH: usize = 30;
        let filled = (pct / 100.0 * BAR_WIDTH as f64) as usize;
        let empty = BAR_WIDTH.saturating_sub(filled);
        let bar: String = format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty),);

        let rate = self.rate.lock().map(|r| *r).unwrap_or(0.0);
        let eta = if rate > 0.1 && current < total {
            let remaining = (total - current) as f64 / rate;
            if remaining >= 60.0 {
                format!(
                    "ETA {}m {:02}s",
                    remaining as u64 / 60,
                    remaining as u64 % 60
                )
            } else {
                format!("ETA {:.0}s", remaining)
            }
        } else if current >= total && total > 0 {
            "done".into()
        } else {
            "warming up…".into()
        };

        eprint!(
            "\r  {:<10} [{bar}] {pct:>5.1}% ({current}/{total}) {rate:>6.0}/s {eta}    ",
            self.label,
        );
        let _ = std::io::stderr().flush();
    }

    fn finish(&self) {
        self.render();
        eprintln!();
    }
}

/// Arguments for `kkernel reindex` — rebuilds embedding vectors for entities,
/// notes, and the knowledge corpus, fanning out across every configured
/// embedding engine (resolved with the same config-file/env precedence as
/// `kkernel mcp`).
#[derive(Parser, Debug)]
pub struct ReindexArgs {
    /// Database path (defaults to `~/.khive/khive.db`). `:memory:` selects an
    /// ephemeral in-memory database in single-backend mode. When discovered
    /// config declares `[[backends]]`, this must explicitly match one declared
    /// persistent SQLite path.
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,

    /// Path to a khive TOML config file (env `KHIVE_CONFIG`). When provided,
    /// embedding engines and actor namespace are resolved from it with the same
    /// precedence as `kkernel mcp`, so reindex writes vectors for the SAME
    /// engine set the MCP server serves recall from. Absent → home-fallback
    /// search (./khive.toml, ./.khive/config.toml, ~/.khive/config.toml).
    #[arg(long = "config", env = "KHIVE_CONFIG")]
    pub config: Option<PathBuf>,

    /// Embedding model for entities/notes. When omitted, fans out to ALL
    /// registered models. (Knowledge always uses the default embedder.)
    #[arg(long)]
    pub model: Option<String>,

    /// Records embedded per batch — also the DB page and write batch (default
    /// 128, max 500). One `embed_document_batch` call processes this many records.
    #[arg(long, default_value = "128")]
    pub batch_size: u32,

    /// Keep existing vectors instead of dropping before re-embedding.
    #[arg(long)]
    pub keep_existing: bool,

    /// Namespace to operate on. When omitted, the config file `[actor] id` (if
    /// any) is honored — matching the same precedence as `kkernel mcp`. An
    /// explicit `--namespace` / `KHIVE_NAMESPACE` overrides the config tier.
    #[arg(long, env = "KHIVE_NAMESPACE")]
    pub namespace: Option<String>,

    /// Only reindex the knowledge corpus (skip entities and notes).
    #[arg(long, conflicts_with = "no_knowledge")]
    pub knowledge_only: bool,

    /// Skip the knowledge corpus (reindex only entities and notes).
    #[arg(long)]
    pub no_knowledge: bool,

    /// Downgrade partial failures (failed model, failed vector insert, failed
    /// knowledge pass) to a warning and still exit 0. Without this flag,
    /// reindex FAILS CLOSED: any failure returns a non-zero exit so automation
    /// does not treat a partial rebuild as a clean one.
    #[arg(long)]
    pub best_effort: bool,

    /// Skip knowledge section embeddings (embed atoms but not sections).
    #[arg(long, conflicts_with = "sections_only")]
    pub no_sections: bool,

    /// Only embed knowledge sections (skip entities, notes, and atoms).
    #[arg(long, conflicts_with = "no_knowledge")]
    pub sections_only: bool,

    /// Rebuild and rank-1 integrity-check both global knowledge FTS indexes
    /// (`fts_knowledge`, `fts_sections`). Off by default: these indexes cover
    /// the whole database, while a reindex run always targets one namespace
    /// (an omitted `--namespace` resolves to the configured one), so no run
    /// scope implies the rebuild. The rebuild runs after the knowledge pass,
    /// so it conflicts with `--no-knowledge` rather than silently doing
    /// nothing under it.
    #[arg(long, conflicts_with = "no_knowledge")]
    pub rebuild_fts: bool,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,
}

/// Load the same discovered config as runtime resolution and ensure that a
/// one-database reindex cannot silently escape a declared backend topology.
fn validate_declared_reindex_target(
    db: Option<&str>,
    config: Option<&std::path::Path>,
) -> Result<()> {
    let discovery_anchor = khive_mcp::serve::config_discovery_db_anchor(db);
    let loaded =
        KhiveConfig::load_with_home_fallback_and_source(config, discovery_anchor.as_deref())
            .context("load reindex khive config for backend-target validation")?;
    let config_source = loaded.as_ref().map(|(_, source)| source.as_path());
    let backends = loaded
        .as_ref()
        .map(|(config, _)| config.backends.as_slice())
        .unwrap_or_default();

    khive_mcp::serve::validate_reindex_db_target_with_source(db, backends, config_source)
}

/// What a `--rebuild-fts` run actually did, so a caller never has to take
/// "it rebuilt the FTS indexes" on faith — the names, wall time, and the
/// rank-1 integrity-check outcome are all reported.
#[derive(Serialize)]
struct KnowledgeFtsRebuildReport {
    indexes: Vec<String>,
    elapsed_ms: u64,
    integrity_ok: bool,
}

#[derive(Serialize)]
struct ReindexReport {
    entities_processed: u64,
    notes_processed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    knowledge_atoms_indexed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    knowledge_sections_indexed: Option<u64>,
    /// Present only when `--rebuild-fts` actually ran the global FTS rebuild.
    #[serde(skip_serializing_if = "Option::is_none")]
    knowledge_fts_rebuild: Option<KnowledgeFtsRebuildReport>,
    /// Atoms whose vector write failed during the knowledge pass.
    knowledge_atoms_failed: u64,
    /// True when the knowledge pass itself errored (could not run to completion).
    knowledge_pass_errored: bool,
    /// True when the Vamana ANN build or snapshot persist failed during the
    /// knowledge pass. Distinct from atom-level failures: atom vectors DID
    /// persist; the ANN snapshot is the failure dimension.
    knowledge_ann_failed: bool,
    /// Section-level embed or SQL-write failures during the knowledge pass.
    /// Distinct from atom-level failures; sections still index atoms even if
    /// section embedding fails.
    knowledge_sections_failed: u64,
    models_used: Vec<String>,
    /// Actual per-model input bounding observed at the embedding seam.
    truncation_by_model: BTreeMap<String, EmbeddingTruncationReport>,
    elapsed_ms: u64,
    /// Entity/note vector inserts that failed across all engines.
    errors_skipped: u64,
    /// Entity FTS upserts that failed during the backfill pass.
    entities_fts_failed: u64,
    /// Note FTS upserts that failed during the backfill pass.
    notes_fts_failed: u64,
    /// True when the completion ("settled") durable memory-ANN epoch bump
    /// failed after entity/note mutations were already committed (#812). The
    /// start-of-pass bump
    /// (`begin_reindex_epoch`) aborts the whole run before any mutation on
    /// failure, so there is nothing left to "abort" here — but a swallowed
    /// failure at this point is exactly the bug this fix closes, so it now
    /// surfaces as a fail-closed exit instead of a silent warning.
    epoch_bump_failed: bool,
}

impl ReindexReport {
    /// Did any part of the run fail? Drives the fail-closed exit decision.
    fn has_failures(&self) -> bool {
        self.errors_skipped > 0
            || self.entities_fts_failed > 0
            || self.notes_fts_failed > 0
            || self.knowledge_atoms_failed > 0
            || self.knowledge_pass_errored
            || self.knowledge_ann_failed
            || self.knowledge_sections_failed > 0
            || self.epoch_bump_failed
    }
}

fn entity_has_embedding_text(entity: &Entity) -> bool {
    !entity.name.trim().is_empty()
        || entity
            .description
            .as_deref()
            .is_some_and(|description| !description.trim().is_empty())
}

fn note_has_embedding_text(note: &Note) -> bool {
    !note.content.trim().is_empty()
}

/// Embed `staged` with every model in `model_names` and store one vector record
/// per model via a single [`VectorStore::insert_batch`] call — mirroring the
/// multi-model write path in the runtime. Returns the number of vector inserts
/// that failed.
///
/// With `drop_existing`, all staged ids are (re)embedded and replaced ATOMICALLY
/// by `insert_batch`: its per-record `SAVEPOINT` deletes and re-inserts a
/// subject's row inside the SAME transaction (`replace_vector_row_dml` in
/// khive-db), including the namespace-agnostic replace needed when a relabeled
/// database has a stale row under a prior namespace (the vec table's PRIMARY KEY
/// is `subject_id` alone). There is deliberately no separate pre-delete pass: a
/// committed delete ahead of the embed/insert step would leave the OLD vector
/// permanently absent (not just stale) if the embed call or the insert itself
/// then failed. `insert_batch` fails a record no worse than leaving the prior
/// vector in place. With `--keep-existing`, existing vectors are preserved and
/// ids already embedded are skipped.
// REASON: each argument is a distinct embed dimension (runtime, token, models,
// namespace, batch, substrate kind, field, drop flag); a struct would add
// indirection without grouping anything cohesive.
#[allow(clippy::too_many_arguments)]
async fn embed_and_store_batch(
    rt: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    model_names: &[String],
    namespace: &str,
    staged: &[(Uuid, String)],
    kind: SubstrateKind,
    field: &str,
    drop_existing: bool,
    truncation_by_model: &mut BTreeMap<String, EmbeddingTruncationReport>,
) -> u64 {
    let mut errors: u64 = 0;

    for model_name in model_names {
        let vectors = match rt.vectors_for_model(token, model_name) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(model = %model_name, error = %e, "vector store unavailable");
                errors += staged.len() as u64;
                continue;
            }
        };

        // Narrow to the records this model still needs when keeping existing vectors.
        let subset: Vec<&(Uuid, String)> = if drop_existing {
            staged.iter().collect()
        } else {
            let ids: Vec<Uuid> = staged.iter().map(|(id, _)| *id).collect();
            match filter_unembedded(vectors.as_ref(), &ids, namespace).await {
                Ok(unembedded) => {
                    let keep: HashSet<Uuid> = unembedded.into_iter().collect();
                    staged.iter().filter(|(id, _)| keep.contains(id)).collect()
                }
                Err(e) => {
                    tracing::error!(model = %model_name, error = %e, "filter_unembedded failed; skipping batch for this model");
                    errors += staged.len() as u64;
                    continue;
                }
            }
        };
        if subset.is_empty() {
            continue;
        }

        let texts: Vec<String> = subset.iter().map(|(_, t)| t.clone()).collect();
        match rt
            .embed_document_batch_with_model_outcomes(model_name, &texts)
            .await
        {
            Ok(outcomes) if outcomes.len() == subset.len() => {
                let model_report = truncation_by_model.entry(model_name.clone()).or_default();
                for outcome in &outcomes {
                    model_report.observe(outcome);
                }
                let expected = subset.len() as u64;
                let now = chrono::Utc::now();
                let records = subset
                    .iter()
                    .zip(outcomes)
                    .map(|((id, _), outcome)| VectorRecord {
                        subject_id: *id,
                        kind,
                        namespace: namespace.to_string(),
                        field: field.to_string(),
                        embedding_model: Some(model_name.clone()),
                        vectors: vec![outcome.vector],
                        updated_at: now,
                    })
                    .collect();
                match vectors.insert_batch(records).await {
                    Ok(summary)
                        if summary.attempted == expected
                            && summary.affected.saturating_add(summary.failed) == expected =>
                    {
                        if summary.failed > 0 {
                            tracing::warn!(
                                model = %model_name,
                                failed = summary.failed,
                                first_error = %summary.first_error,
                                "vector batch insert partially failed"
                            );
                            errors += summary.failed;
                        }
                    }
                    Ok(summary) => {
                        tracing::warn!(
                            model = %model_name,
                            expected,
                            attempted = summary.attempted,
                            affected = summary.affected,
                            failed = summary.failed,
                            "vector batch insert returned inconsistent accounting"
                        );
                        errors += expected;
                    }
                    Err(e) => {
                        tracing::warn!(model = %model_name, error = %e, "vector batch insert failed");
                        errors += expected;
                    }
                }
            }
            Ok(_) => {
                tracing::warn!(model = %model_name, "embedding count mismatch for batch");
                errors += subset.len() as u64;
            }
            Err(e) => {
                tracing::warn!(model = %model_name, error = %e, "embed_batch failed");
                errors += subset.len() as u64;
            }
        }
    }
    errors
}

/// Upsert FTS documents for a batch of notes into the namespace text index. Returns the
/// number of per-note upsert failures. Idempotent: calling again for an already-indexed
/// note replaces the existing row (FTS upsert semantics). Fails per-note, never panics.
async fn fts_backfill_notes_batch(
    rt: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    batch: &[Note],
) -> u64 {
    let fts = match rt.text_for_notes(token) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "FTS store unavailable; counting whole batch as failed");
            return batch.len() as u64;
        }
    };
    let mut errors: u64 = 0;
    for note in batch {
        let doc = note_fts_document(note);
        if let Err(e) = fts.upsert_document(doc).await {
            tracing::warn!(id = %note.id, error = %e, "FTS upsert failed for note");
            errors += 1;
        }
    }
    errors
}

/// Upsert FTS documents for a batch of entities into the namespace text index. Returns the
/// number of per-entity upsert failures. Idempotent: calling again for an already-indexed
/// entity replaces the existing row (FTS upsert semantics). Fails per-entity, never panics.
async fn fts_backfill_entities_batch(
    rt: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    batch: &[Entity],
) -> u64 {
    let fts = match rt.text(token) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "FTS store unavailable; counting whole batch as failed");
            return batch.len() as u64;
        }
    };
    let mut errors: u64 = 0;
    for entity in batch {
        let doc = entity_fts_document(entity);
        if let Err(e) = fts.upsert_document(doc).await {
            tracing::warn!(id = %entity.id, error = %e, "FTS upsert failed for entity");
            errors += 1;
        }
    }
    errors
}

/// Return the subset of `ids` that do NOT already have an embedding in `vectors`
/// for the given `namespace`. When `batch_exists` is unsupported (e.g. a custom
/// backend), conservatively returns all IDs so every record gets embedded.
async fn filter_unembedded(
    vectors: &dyn VectorStore,
    ids: &[Uuid],
    namespace: &str,
) -> Result<Vec<Uuid>> {
    match vectors.batch_exists(ids, namespace).await {
        Ok(existing) => Ok(ids
            .iter()
            .filter(|id| !existing.contains(id))
            .copied()
            .collect()),
        Err(StorageError::Unsupported { .. }) => Ok(ids.to_vec()),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// Re-embed entities, notes, and the knowledge corpus, fanning out across every
/// configured embedding engine. Engines, db path, and config are resolved with
/// the same precedence as `kkernel mcp` so reindex writes the SAME vectors the
/// MCP server serves recall from. Fails closed on any partial failure unless
/// `--best-effort` is set.
pub async fn run_reindex(args: ReindexArgs) -> Result<()> {
    validate_declared_reindex_target(args.db.as_deref(), args.config.as_deref())?;

    // Namespace precedence mirrors `kkernel mcp`:
    //   1. --namespace / KHIVE_NAMESPACE (explicit CLI/env) — skips config tier
    //   2. [actor] id in the config file
    //   3. Default "local"
    let explicit = args.namespace.is_some();
    let raw = args.namespace.as_deref().unwrap_or("local");
    let ns = Namespace::parse(raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace: ns,
        namespace_explicit: explicit,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })?;

    // Capture the resolved namespace BEFORE `new` consumes cfg — when
    // `!explicit`, `resolve_runtime_config` may have applied `[actor] id` from
    // the config file, making `cfg.default_namespace` differ from the CLI value.
    let resolved_ns = cfg.default_namespace.clone();
    let rt = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let token = rt
        .authorize(resolved_ns)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to authorize namespace")?;

    // `--sections-only` is the narrowest scope: knowledge sections alone.
    let do_graph = !args.knowledge_only && !args.sections_only; // entities + notes
    let do_knowledge = !args.no_knowledge; // knowledge corpus
    let do_atoms = do_knowledge && !args.sections_only;
    let do_sections = do_knowledge && !args.no_sections;

    let rebuild_fts = args.rebuild_fts;

    // Explicit --model targets a single engine; otherwise fan out to ALL
    // registered engines, matching the runtime's multi-model write path so a
    // reindex reproduces exactly what create/update would have embedded.
    // Only needed for the entity/note pass (knowledge uses the default embedder).
    //
    // When no embedding model is configured, model_names is empty: the embedding
    // loop is a no-op but the note loop still runs for FTS backfill, which needs
    // no embedder and must never be skipped due to a missing embedding config.
    let model_names: Vec<String> = if !do_graph {
        vec![]
    } else {
        match args.model.as_deref().filter(|s| !s.is_empty()) {
            Some(name) => vec![name.to_string()],
            None => {
                let names = rt.registered_embedding_model_names();
                if names.is_empty() {
                    eprintln!("warning: no embedding model configured — skipping vector embedding; FTS backfill will still run");
                }
                names
            }
        }
    };

    let batch_size = args.batch_size.clamp(1, 500);
    let drop_existing = !args.keep_existing;
    let ns_str = token.namespace().as_str().to_owned();
    let start = std::time::Instant::now();

    let mut entities_processed: u64 = 0;
    let mut notes_processed: u64 = 0;
    let mut errors_skipped: u64 = 0;
    let mut entities_fts_failed: u64 = 0;
    let mut notes_fts_failed: u64 = 0;
    let mut truncation_by_model = BTreeMap::new();

    let mut epoch_bump_failed = false;

    // ── entities + notes (graph substrate) ────────────────────────────────────
    if do_graph {
        begin_reindex_epoch(&rt)
            .await
            .context("aborting reindex before any vector mutation")?;

        let entity_total = rt.count_entities(&token, None).await.unwrap_or(0);
        let entity_bar = ProgressBar::new("entities");
        entity_bar.update(0, entity_total);

        let mut entity_offset: u32 = 0;
        loop {
            let batch = rt
                .list_entities(&token, None, None, batch_size, entity_offset)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let n = batch.len();
            if n == 0 {
                break;
            }

            let embeddable = if model_names.is_empty() {
                batch
                    .iter()
                    .filter(|entity| entity_has_embedding_text(entity))
                    .count()
            } else {
                let mut staged = Vec::with_capacity(n);
                for entity in &batch {
                    if entity_has_embedding_text(entity) {
                        staged.push((entity.id, entity_embedding_text(entity)));
                    }
                }

                if !staged.is_empty() {
                    errors_skipped += embed_and_store_batch(
                        &rt,
                        &token,
                        &model_names,
                        &ns_str,
                        &staged,
                        SubstrateKind::Entity,
                        "entity.body",
                        drop_existing,
                        &mut truncation_by_model,
                    )
                    .await;
                }
                staged.len()
            };
            entities_processed += embeddable as u64;

            // FTS backfill: index every entity in this batch regardless of whether
            // it had content to embed. Mirrors the upsert_document call in
            // operations.rs — see entity_fts_document for the parity contract.
            entities_fts_failed += fts_backfill_entities_batch(&rt, &token, &batch).await;

            entity_bar.update(entities_processed, entity_total);

            if n < batch_size as usize {
                break;
            }
            entity_offset += n as u32;
        }
        entity_bar.finish();

        // ── notes ─────────────────────────────────────────────────────────────────
        let note_total = count_notes(&rt, &ns_str).await;
        let note_bar = ProgressBar::new("notes");
        note_bar.update(0, note_total);

        let mut note_offset: u32 = 0;
        loop {
            let batch = rt
                .list_notes(&token, None, batch_size, note_offset)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let n = batch.len();
            if n == 0 {
                break;
            }

            let embeddable = if model_names.is_empty() {
                batch
                    .iter()
                    .filter(|note| note_has_embedding_text(note))
                    .count()
            } else {
                let mut staged = Vec::with_capacity(n);
                for note in &batch {
                    if note_has_embedding_text(note) {
                        staged.push((note.id, note_embedding_text(note)));
                    }
                }

                if !staged.is_empty() {
                    errors_skipped += embed_and_store_batch(
                        &rt,
                        &token,
                        &model_names,
                        &ns_str,
                        &staged,
                        SubstrateKind::Note,
                        "note.content",
                        drop_existing,
                        &mut truncation_by_model,
                    )
                    .await;
                }
                staged.len()
            };
            notes_processed += embeddable as u64;

            // FTS backfill: index every note in this batch regardless of whether
            // it had content to embed. Mirrors the upsert_document call in
            // operations.rs — see note_fts_document for the parity contract.
            notes_fts_failed += fts_backfill_notes_batch(&rt, &token, &batch).await;

            note_bar.update(notes_processed, note_total);

            if n < batch_size as usize {
                break;
            }
            note_offset += n as u32;
        }
        note_bar.finish();

        // Invalidate Vamana snapshots so the next warm-load triggers a rebuild
        // against the freshly re-embedded entity/note vectors.
        if let Err(e) = invalidate_vamana_snapshots(&rt, &ns_str).await {
            tracing::warn!(error = %e, "failed to invalidate Vamana snapshots after reindex");
        }

        // Purge stale per-namespace memory Vamana snapshot rows (legacy key format
        // `{ns}::memory_vamana::*`). After FTS+ANN consolidation the unified key is
        // `global::memory_vamana::*`; old per-ns rows are orphaned and waste space.
        purge_stale_memory_vamana_snapshots(&rt).await;

        // Invalidate the ACTIVE global memory Vamana snapshot too (#812).
        // Its key (`global::memory_vamana::*`)
        // never matched `invalidate_vamana_snapshots`'s `{namespace}::vamana::%`
        // pattern above, so the note re-embed this pass just did left that
        // snapshot installed and untouched — the content-hash restart check in
        // `khive-pack-memory::ann` is the primary defense against a daemon
        // trusting it afterward, but deleting it here forces a rebuild on the
        // very next warm regardless, without depending on that check alone.
        //
        // This also performs the completion ("settled") durable epoch bump
        // (#812); see its own doc comment for
        // why a failure here is reported rather than warned-and-ignored.
        epoch_bump_failed = !invalidate_active_memory_vamana_snapshot(&rt).await;

        // Drop per-namespace FTS partition tables that survived the V4 migration
        // (tables created by the runtime before the migration ran, or on databases
        // that were migrated but not swept). The sweep is guarded: it only runs
        // when this reindex pass covered every distinct namespace in the base
        // entities/notes tables. If any namespace is uncovered, sweeping would
        // orphan those rows (they were dropped from the old partition and never
        // written to the new unified table). On a single-namespace (post-relabel)
        // db the guard always passes and the sweep runs normally.
        sweep_stale_fts_partitions(&rt, &ns_str).await;
    } // end if do_graph

    // ── knowledge corpus ───────────────────────────────────────────────────────
    // Reindex through the knowledge library directly (the `knowledge.index`
    // handler over the full corpus), not the verb-DSL shell.
    let mut knowledge_atoms_indexed: Option<u64> = None;
    let mut knowledge_sections_indexed: Option<u64> = None;
    let mut knowledge_atoms_failed: u64 = 0;
    let mut knowledge_pass_errored = false;
    let mut knowledge_ann_failed = false;
    let mut knowledge_sections_failed: u64 = 0;
    let mut knowledge_fts_rebuild: Option<KnowledgeFtsRebuildReport> = None;
    if do_atoms || do_sections {
        let atom_bar = ProgressBar::new("atoms");
        let section_bar = ProgressBar::new("sections");
        let on_atom = |c: u64, t: u64| atom_bar.update(c, t);
        let on_section = |c: u64, t: u64| section_bar.update(c, t);

        let opts = khive_pack_knowledge::KnowledgeReindexOptions {
            atoms: do_atoms,
            sections: do_sections,
            drop_existing,
            rebuild_ann: true,
            batch_size: Some(batch_size),
        };
        match khive_pack_knowledge::reindex_knowledge(
            &rt,
            &token,
            opts,
            if do_atoms { Some(&on_atom) } else { None },
            if do_sections { Some(&on_section) } else { None },
        )
        .await
        {
            Ok(v) => {
                if let Some(per_model) = v.get("truncation_by_model").and_then(|v| v.as_object()) {
                    for (model, value) in per_model {
                        if let Ok(report) =
                            serde_json::from_value::<EmbeddingTruncationReport>(value.clone())
                        {
                            truncation_by_model
                                .entry(model.clone())
                                .or_default()
                                .merge(report);
                        }
                    }
                }
                if do_atoms {
                    knowledge_atoms_indexed =
                        Some(v.get("atoms_indexed").and_then(|n| n.as_u64()).unwrap_or(0));
                    knowledge_atoms_failed = v.get("failed").and_then(|n| n.as_u64()).unwrap_or(0);
                    knowledge_ann_failed = v
                        .get("ann_failed")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                }
                if do_sections {
                    knowledge_sections_indexed = Some(
                        v.get("sections_indexed")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0),
                    );
                    knowledge_sections_failed = v
                        .get("sections_failed")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0);
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "knowledge reindex failed");
                eprintln!("\nerror: knowledge reindex failed: {e}");
                knowledge_pass_errored = true;
            }
        }
        if do_atoms {
            atom_bar.finish();
        }
        if do_sections {
            section_bar.finish();
        }
    }

    // The FTS rebuild is a whole-database operation, so it goes through the
    // operator entry point rather than the namespace-scoped reindex options.
    // It runs only after a clean knowledge pass: a failed pass already exits
    // non-zero, and a rebuild on top of it would report evidence for a run
    // the operator is about to be told failed.
    if rebuild_fts && !knowledge_pass_errored {
        match khive_pack_knowledge::rebuild_knowledge_fts_indexes(&rt).await {
            Ok(fts) => knowledge_fts_rebuild = Some(fts_rebuild_report(&fts)),
            Err(e) => {
                tracing::error!(error = %e, "knowledge FTS rebuild failed");
                eprintln!("\nerror: knowledge FTS rebuild failed: {e}");
                knowledge_pass_errored = true;
            }
        }
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;

    let report = ReindexReport {
        entities_processed,
        notes_processed,
        knowledge_atoms_indexed,
        knowledge_sections_indexed,
        knowledge_fts_rebuild,
        knowledge_atoms_failed,
        knowledge_pass_errored,
        knowledge_ann_failed,
        knowledge_sections_failed,
        models_used: model_names,
        truncation_by_model,
        elapsed_ms,
        errors_skipped,
        entities_fts_failed,
        notes_fts_failed,
        epoch_bump_failed,
    };

    print_report(&report, args.human);
    finish(&report, args.best_effort)
}

/// Parse the `{indexes, elapsed_ms, integrity_ok}` value returned by the
/// knowledge FTS rebuild into the report shape.
fn fts_rebuild_report(fts: &serde_json::Value) -> KnowledgeFtsRebuildReport {
    KnowledgeFtsRebuildReport {
        indexes: fts
            .get("indexes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        elapsed_ms: fts.get("elapsed_ms").and_then(|n| n.as_u64()).unwrap_or(0),
        integrity_ok: fts
            .get("integrity_ok")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
    }
}

/// Decide the process exit from a completed report: `Ok(())` when clean or in
/// best-effort mode, `Err` (non-zero exit) when fail-closed and any part failed.
/// Pure decision logic, unit-tested without running embedders.
fn decide_result(has_failures: bool, best_effort: bool) -> Result<()> {
    if has_failures && !best_effort {
        anyhow::bail!(
            "reindex completed with failures; recall/search state may be stale. \
             Re-run, or pass --best-effort to accept a partial rebuild."
        );
    }
    Ok(())
}

/// Surface the fail-closed decision after printing the report.
fn finish(report: &ReindexReport, best_effort: bool) -> Result<()> {
    let result = decide_result(report.has_failures(), best_effort);
    if report.has_failures() && best_effort {
        eprintln!("warning: reindex completed with failures (best-effort mode; exiting 0)");
    }
    result
}

/// Escape SQLite `LIKE` wildcard characters (`%`, `_`) and the escape
/// character itself (`\`) so a caller-supplied namespace is matched literally
/// under `LIKE ... ESCAPE '\'` rather than as a pattern (#819: an
/// underscore-bearing namespace like `a_b` must not also match `aXb`).
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn invalidate_vamana_snapshots(rt: &KhiveRuntime, namespace: &str) -> anyhow::Result<()> {
    use khive_storage::types::{SqlStatement, SqlValue};

    let pattern = format!("{}::vamana::%", escape_like(namespace));
    let sql = rt.sql();
    let mut writer = sql
        .writer()
        .await
        .context("open SQL writer for Vamana snapshot invalidation")?;

    match writer
        .execute(SqlStatement {
            sql: "DELETE FROM retrieval_snapshots WHERE namespace LIKE ?1 ESCAPE '\\'".into(),
            params: vec![SqlValue::Text(pattern)],
            label: Some("invalidate_vamana_snapshots".into()),
        })
        .await
    {
        Ok(deleted) => {
            tracing::info!(
                deleted,
                namespace,
                "invalidated Vamana snapshots after reindex"
            );
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no such table") {
                tracing::debug!("retrieval_snapshots absent; no Vamana snapshots to invalidate");
                Ok(())
            } else {
                Err(anyhow::anyhow!("{e}"))
            }
        }
    }
}

/// Remove per-namespace memory Vamana snapshot rows (legacy `{ns}::memory_vamana::*` format).
/// After FTS+ANN consolidation the active, retained key is `global::memory_vamana::{model}`
/// (ADR-062, corrected by ADR-116 (PR #1080)); old per-ns rows are orphaned. Best-effort —
/// missing table or SQL failure is logged and ignored.
async fn purge_stale_memory_vamana_snapshots(rt: &KhiveRuntime) {
    use khive_storage::types::SqlStatement;
    let sql = rt.sql();
    let Ok(mut writer) = sql.writer().await else {
        return;
    };
    match writer
        .execute(SqlStatement {
            // `retrieval_snapshots.namespace` holds the FULL composite key produced by
            // `ann::snapshot_key` (`"global::memory_vamana::{model}"`), not a bare
            // namespace — `namespace != 'global'` never matches that literal string and
            // so purged every memory_vamana row unconditionally, including current,
            // still-valid `global::memory_vamana::*` snapshots (ADR-116 (PR #1080)
            // condition 4). Match the retained key's prefix instead, mirroring
            // `invalidate_active_memory_vamana_snapshot`'s LIKE pattern below — but with
            // GLOB, not LIKE: SQLite's LIKE is ASCII case-insensitive, so a legacy
            // `GLOBAL::memory_vamana::*` row (a valid namespace per namespace validation)
            // would otherwise be treated as the retained lowercase key and never purged.
            // GLOB is case-sensitive (uses `*`/`?` globbing, not `%`/`_`).
            sql: "DELETE FROM retrieval_snapshots \
                  WHERE index_type = 'memory_vamana' \
                    AND namespace NOT GLOB 'global::memory_vamana::*'"
                .into(),
            params: vec![],
            label: Some("purge_stale_memory_vamana_snapshots".into()),
        })
        .await
    {
        Ok(deleted) => {
            if deleted > 0 {
                tracing::info!(deleted, "purged stale per-ns memory Vamana snapshot rows");
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if !msg.contains("no such table") {
                tracing::warn!(error = %e, "failed to purge stale memory Vamana snapshots");
            }
        }
    }
}

/// Durably marks the reindex-in-progress epoch, BEFORE any vector mutation in
/// this pass (#812, ADR-107 §4). Fail-closed: an error here (schema creation OR
/// the epoch write) aborts the whole reindex before any mutation runs — never
/// warn-and-continue. See
/// `crates/kkernel/docs/design.md#reindex-memory-vamana-epoch-protocol-812-adr-107-4`
/// for the in-progress/completed epoch protocol this is half of.
async fn begin_reindex_epoch(rt: &KhiveRuntime) -> Result<()> {
    khive_pack_memory::ensure_ann_epoch_schema(rt)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to ensure memory_ann_epoch schema before reindex")?;
    khive_pack_memory::bump_memory_ann_epoch(rt)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to durably mark reindex-in-progress epoch")?;
    Ok(())
}

/// Delete the ACTIVE global memory Vamana snapshot row (`global::memory_vamana::*`),
/// distinct from `purge_stale_memory_vamana_snapshots`'s legacy-row cleanup above
/// (#812). Reindex rewrites note
/// embeddings directly, bypassing `memory.remember`, so it never bumps the memory
/// pack's in-memory write-generation counter — that daemon-side signal simply
/// cannot see this change. The DELETE itself stays best-effort (a missing table
/// or SQL failure is logged and ignored, matching this file's other
/// snapshot-maintenance helpers) — it is a defense-in-depth optimization, not
/// the correctness mechanism.
///
/// Returns `false` when the completion ("settled") durable epoch bump below
/// fails — see `begin_reindex_epoch`'s doc comment for the two-phase
/// protocol this half completes. Unlike `begin_reindex_epoch`, mutations have
/// already committed by this point, so there is nothing left to abort; the
/// caller instead folds this into `ReindexReport::epoch_bump_failed`, which
/// drives a fail-closed non-zero exit instead of the old warn-and-continue.
async fn invalidate_active_memory_vamana_snapshot(rt: &KhiveRuntime) -> bool {
    use khive_storage::types::{SqlStatement, SqlValue};
    let sql = rt.sql();
    if let Ok(mut writer) = sql.writer().await {
        // `retrieval_snapshots.namespace` holds the FULL composite key produced
        // by `ann::snapshot_key` (`"global::memory_vamana::{model}"`), not a
        // bare namespace — matching on `namespace = 'global'` would never
        // match any row.
        match writer
            .execute(SqlStatement {
                sql: "DELETE FROM retrieval_snapshots \
                      WHERE index_type = 'memory_vamana' AND namespace LIKE ?1"
                    .into(),
                params: vec![SqlValue::Text("global::memory_vamana::%".into())],
                label: Some("invalidate_active_memory_vamana_snapshot".into()),
            })
            .await
        {
            Ok(deleted) => {
                if deleted > 0 {
                    tracing::info!(
                        deleted,
                        "invalidated active global memory Vamana snapshot after reindex"
                    );
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("no such table") {
                    tracing::warn!(error = %e, "failed to invalidate active memory Vamana snapshot");
                }
            }
        }
    }

    // #812: the completion half of the
    // in-progress/completed epoch protocol described on `begin_reindex_epoch`.
    // A khive daemon that warmed its in-memory ANN index before this reindex
    // ran shares no process, and therefore no in-memory write-generation
    // state, with this `kkernel reindex` invocation — its `common.rs` recall
    // path would keep trusting that cached index forever with no way to
    // observe this mutation at all. Bumping the durable epoch here gives that
    // daemon's amortized freshness check
    // (`khive_pack_memory::ann::maybe_check_durable_epoch`, sampled from the
    // recall path) a signal written to the shared database file instead of
    // one confined to this process.
    if let Err(e) = khive_pack_memory::bump_memory_ann_epoch(rt).await {
        tracing::warn!(error = %e, "failed to bump durable memory ANN epoch after reindex");
        return false;
    }
    true
}

/// Return the set of distinct namespaces present in base `entities` and `notes`
/// (non-deleted rows only). Used by the FTS sweep guard.
async fn distinct_base_namespaces(rt: &KhiveRuntime) -> HashSet<String> {
    use khive_storage::types::SqlStatement;
    let sql = rt.sql();
    let Ok(mut reader) = sql.reader().await else {
        return HashSet::new();
    };
    // Union of entity and note namespaces; soft-deleted rows are excluded so
    // we only guard against losing rows that are still live in the base table.
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT DISTINCT namespace FROM entities WHERE deleted_at IS NULL \
                  UNION \
                  SELECT DISTINCT namespace FROM notes WHERE deleted_at IS NULL"
                .into(),
            params: vec![],
            label: Some("distinct_base_namespaces".into()),
        })
        .await
        .unwrap_or_default();
    rows.into_iter()
        .filter_map(|row| {
            row.get("namespace").and_then(|v| {
                if let khive_storage::types::SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
        })
        .collect()
}

/// Drop per-namespace FTS5 partition tables (`fts_entities_*`, `fts_notes_*`) that
/// may exist in databases that were not yet migrated or were created before V4.
/// Canonical tables (`fts_entities`, `fts_notes`, `fts_knowledge`, `fts_sections`)
/// and their FTS5 shadow tables are never dropped.
/// Safe to run repeatedly; a no-op on fresh databases.
///
/// **Sweep guard**: only drops partition tables when every distinct namespace
/// present in the base `entities`/`notes` tables was covered by this reindex
/// pass (i.e. the operating namespace `covered_ns` is the only namespace in the
/// base). If uncovered namespaces exist, the sweep is skipped and a warning is
/// emitted so operators know a manual or multi-namespace reindex is needed.
async fn sweep_stale_fts_partitions(rt: &KhiveRuntime, covered_ns: &str) {
    use khive_storage::types::{SqlStatement, SqlValue};

    // Guard: only sweep when every distinct namespace present in base
    // entities/notes was covered by this reindex pass. A single-namespace
    // (post-relabel) db has exactly {covered_ns} and passes immediately. A
    // multi-namespace db would be partially swept — rows in other namespaces
    // were dropped from old partitions but never carried to the unified table —
    // so we skip and warn instead.
    let base_namespaces = distinct_base_namespaces(rt).await;
    let uncovered: Vec<&str> = base_namespaces
        .iter()
        .filter(|ns| ns.as_str() != covered_ns)
        .map(String::as_str)
        .collect();
    if !uncovered.is_empty() {
        tracing::warn!(
            covered = covered_ns,
            uncovered = ?uncovered,
            "skipping stale FTS partition sweep: base tables contain namespaces not \
             covered by this reindex pass; run reindex for each namespace first, \
             or normalize all rows to one namespace before sweeping"
        );
        return;
    }

    // Canonical base names that must never be dropped.
    let canonical: &[&str] = &["fts_entities", "fts_notes", "fts_knowledge", "fts_sections"];

    // FTS5 shadow table suffixes that must never be dropped (the extension drops
    // them automatically when the virtual table itself is dropped; we only drop
    // the virtual table, so these patterns must be excluded from discovery).
    let shadow_suffixes: &[&str] = &["_data", "_idx", "_docsize", "_config", "_content"];

    let sql = rt.sql();
    let Ok(mut reader) = sql.reader().await else {
        return;
    };

    // Find candidate tables: type='table', name starts with `fts_entities_` or `fts_notes_`.
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT name FROM sqlite_master \
                  WHERE type IN ('table', 'shadow') \
                    AND (name LIKE 'fts_entities_%' OR name LIKE 'fts_notes_%')"
                .into(),
            params: vec![],
            label: Some("sweep_stale_fts_partitions_discover".into()),
        })
        .await;

    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to discover stale FTS partition tables");
            return;
        }
    };

    let mut to_drop: Vec<String> = Vec::new();
    for row in &rows {
        let name = match row.get("name") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => continue,
        };
        // Skip canonical tables.
        if canonical.contains(&name.as_str()) {
            continue;
        }
        // Skip FTS5 shadow tables (they are dropped automatically with the virtual table).
        if shadow_suffixes.iter().any(|suf| name.ends_with(suf)) {
            continue;
        }
        to_drop.push(name);
    }
    drop(reader);

    if to_drop.is_empty() {
        return;
    }

    let Ok(mut writer) = sql.writer().await else {
        return;
    };
    for table in &to_drop {
        let ddl = format!("DROP TABLE IF EXISTS {}", quote_sqlite_identifier(table));
        match writer
            .execute(SqlStatement {
                sql: ddl,
                params: vec![],
                label: Some("sweep_stale_fts_partitions_drop".into()),
            })
            .await
        {
            Ok(_) => {
                tracing::info!(table, "dropped stale FTS partition table");
            }
            Err(e) => {
                tracing::warn!(error = %e, table, "failed to drop stale FTS partition table");
            }
        }
    }
}

/// Quote a SQLite identifier for safe interpolation into generated DDL,
/// doubling any embedded double quotes so the identifier cannot terminate
/// early and inject additional statements.
fn quote_sqlite_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn count_notes(rt: &KhiveRuntime, ns: &str) -> u64 {
    use khive_storage::types::{SqlStatement, SqlValue};
    let sql = rt.sql();
    let Ok(mut reader) = sql.reader().await else {
        return 0;
    };
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT count(*) AS cnt FROM notes WHERE namespace = ?1 AND deleted_at IS NULL"
                .into(),
            params: vec![SqlValue::Text(ns.to_owned())],
            label: None,
        })
        .await;
    match row {
        Ok(Some(r)) => match r.get("cnt") {
            Some(SqlValue::Integer(n)) => *n as u64,
            _ => 0,
        },
        _ => 0,
    }
}

fn render_human_report(report: &ReindexReport) -> String {
    let atoms = report
        .knowledge_atoms_indexed
        .map(|n| format!(", {n} knowledge atoms"))
        .unwrap_or_default();
    let sections = report
        .knowledge_sections_indexed
        .map(|n| format!(", {n} sections"))
        .unwrap_or_default();
    let status = if report.has_failures() {
        "Reindex completed WITH FAILURES"
    } else {
        "Reindex complete"
    };
    let fts_errors = report.entities_fts_failed + report.notes_fts_failed;
    let mut output = format!(
        "{status}: {} entities, {} notes{}{} ({} vector errors, {} FTS errors) in {}ms\n",
        report.entities_processed,
        report.notes_processed,
        atoms,
        sections,
        report.errors_skipped,
        fts_errors,
        report.elapsed_ms
    );
    if report.entities_fts_failed > 0 {
        output.push_str(&format!(
            "FTS backfill: {} entity upserts FAILED\n",
            report.entities_fts_failed
        ));
    }
    if report.notes_fts_failed > 0 {
        output.push_str(&format!(
            "FTS backfill: {} note upserts FAILED\n",
            report.notes_fts_failed
        ));
    }
    if report.knowledge_pass_errored {
        output.push_str("Knowledge pass: FAILED (did not run to completion)\n");
    } else if report.knowledge_atoms_failed > 0 {
        output.push_str(&format!(
            "Knowledge pass: {} atom vector inserts FAILED\n",
            report.knowledge_atoms_failed
        ));
    }
    if report.knowledge_sections_failed > 0 {
        output.push_str(&format!(
            "Knowledge sections: {} section embed/write failures\n",
            report.knowledge_sections_failed
        ));
    }
    if report.knowledge_ann_failed {
        output.push_str("Knowledge ANN: FAILED (snapshot not rebuilt/persisted)\n");
    }
    if let Some(fts) = &report.knowledge_fts_rebuild {
        output.push_str(&format!(
            "Knowledge FTS rebuild: {} in {}ms, integrity {}\n",
            fts.indexes.join(", "),
            fts.elapsed_ms,
            if fts.integrity_ok { "OK" } else { "FAILED" }
        ));
    }
    if !report.models_used.is_empty() {
        output.push_str(&format!("Models: {}\n", report.models_used.join(", ")));
    }
    for (model, truncation) in &report.truncation_by_model {
        let input_label = if truncation.truncated == 1 {
            "input"
        } else {
            "inputs"
        };
        output.push_str(&format!(
            "Embedding truncation ({model}): {} {input_label} truncated, {} bytes discarded\n",
            truncation.truncated, truncation.discarded_bytes
        ));
    }
    output
}

fn print_report(report: &ReindexReport, human: bool) {
    if human {
        print!("{}", render_human_report(report));
    } else {
        let json = serde_json::to_string(report).expect("serialize ReindexReport");
        println!("{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbpath::resolve_db_override;
    use clap::Parser;
    use khive_storage::types::{SqlStatement, SqlValue};
    use serial_test::serial;

    fn write_empty_test_config(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("empty-khive-config.toml");
        std::fs::write(&path, "").expect("write isolated empty config");
        path
    }

    fn write_declared_backend_test_config(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
        let main = dir.join("main.db");
        let knowledge = dir.join("knowledge.db");
        let config = dir.join("khive.toml");
        std::fs::write(
            &config,
            format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "knowledge"
kind = "sqlite"
path = "{}"
"#,
                main.display(),
                knowledge.display(),
            ),
        )
        .expect("write declared-backend config");
        (config, main, knowledge)
    }

    /// One writable `main` backend plus one `read_only = true` `archive`
    /// backend, so a reindex validator test can assert the read-only path is
    /// refused while the writable path (the control) still passes.
    fn write_read_only_backend_test_config(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
        let main = dir.join("main.db");
        let archive = dir.join("archive.db");
        let config = dir.join("khive.toml");
        std::fs::write(
            &config,
            format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "archive"
kind = "sqlite"
path = "{}"
read_only = true
"#,
                main.display(),
                archive.display(),
            ),
        )
        .expect("write read-only-backend config");
        (config, main, archive)
    }

    #[test]
    fn allocation_free_embedding_eligibility_matches_canonical_text() {
        let entities = [
            Entity::new("eligibility", "concept", "named"),
            Entity::new("eligibility", "concept", "").with_description("description only"),
            Entity::new("eligibility", "concept", "   ").with_description("\t"),
        ];
        for entity in &entities {
            assert_eq!(
                entity_has_embedding_text(entity),
                !entity_embedding_text(entity).trim().is_empty()
            );
        }

        let notes = [
            Note::new("eligibility", "observation", "content"),
            Note::new("eligibility", "observation", "  \n\t"),
        ];
        for note in &notes {
            assert_eq!(
                note_has_embedding_text(note),
                !note_embedding_text(note).trim().is_empty()
            );
        }
    }

    #[tokio::test]
    async fn test_reindex_invalidates_vamana_snapshots() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        // Create retrieval_snapshots table and seed rows.
        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        for (ns, idx_type) in &[
            ("local::vamana::model-a", "vamana"),
            ("local::vamana::model-b", "vamana"),
            ("other::vamana::model-a", "vamana"),
            ("local::hnsw::model-a", "hnsw"),
        ] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(idx_type.to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert row");
        }
        drop(w);

        invalidate_vamana_snapshots(&rt, "local")
            .await
            .expect("invalidate");

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            remaining.contains(&"other::vamana::model-a".to_string()),
            "other namespace must survive: {remaining:?}"
        );
        assert!(
            remaining.contains(&"local::hnsw::model-a".to_string()),
            "HNSW rows must survive: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"local::vamana::model-a".to_string()),
            "local vamana model-a must be deleted: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"local::vamana::model-b".to_string()),
            "local vamana model-b must be deleted: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn test_reindex_invalidate_does_not_cross_underscore_namespace() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        // "a_b" and "aXb" are distinct namespaces (the `_` in "a_b" is a
        // literal underscore, not a wildcard). Before #819's fix, invalidating
        // "a_b" also deleted "aXb"'s row because `_` is a single-character
        // LIKE wildcard.
        for ns in &["a_b::vamana::model-a", "aXb::vamana::model-a"] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text("vamana".to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert row");
        }
        drop(w);

        invalidate_vamana_snapshots(&rt, "a_b")
            .await
            .expect("invalidate");

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            remaining.contains(&"aXb::vamana::model-a".to_string()),
            "unrelated namespace 'aXb' must survive invalidating 'a_b': {remaining:?}"
        );
        assert!(
            !remaining.contains(&"a_b::vamana::model-a".to_string()),
            "'a_b' own snapshot must still be deleted: {remaining:?}"
        );
    }

    /// Regression test (#812): the active global memory Vamana snapshot row
    /// must
    /// be deleted by `invalidate_active_memory_vamana_snapshot`, since
    /// `invalidate_vamana_snapshots`'s `{namespace}::vamana::%` pattern never
    /// matches the memory pack's distinct `global::memory_vamana::*` key.
    #[tokio::test]
    async fn test_reindex_invalidates_active_memory_vamana_snapshot() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        for (ns, idx_type) in &[
            ("global::memory_vamana::model-a", "memory_vamana"),
            ("local::vamana::model-a", "vamana"),
            ("local::memory_vamana::model-a", "memory_vamana"),
        ] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(idx_type.to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert row");
        }
        drop(w);

        invalidate_active_memory_vamana_snapshot(&rt).await;

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            !remaining.contains(&"global::memory_vamana::model-a".to_string()),
            "the active global memory Vamana snapshot must be deleted: {remaining:?}"
        );
        assert!(
            remaining.contains(&"local::vamana::model-a".to_string()),
            "unrelated knowledge Vamana rows must survive: {remaining:?}"
        );
        assert!(
            remaining.contains(&"local::memory_vamana::model-a".to_string()),
            "legacy per-namespace memory Vamana rows are purge_stale_memory_vamana_snapshots's \
             job, not this function's: {remaining:?}"
        );
    }

    /// Regression test (ADR-116 (PR #1080) condition 4): `purge_stale_memory_vamana_snapshots` must
    /// keep the current, retained `global::memory_vamana::{model}` key (ADR-062) and purge
    /// only legacy per-namespace `{ns}::memory_vamana::*` rows. The prior predicate
    /// (`namespace != 'global'`) matched every row unconditionally, since the namespace
    /// column stores the full composite key and is never the bare string `'global'`.
    #[tokio::test]
    async fn test_purge_stale_memory_vamana_snapshots_keeps_current_key() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        for (ns, idx_type) in &[
            ("global::memory_vamana::model-a", "memory_vamana"),
            ("local::memory_vamana::model-a", "memory_vamana"),
            ("tenant-a::memory_vamana::model-b", "memory_vamana"),
            ("local::vamana::model-a", "vamana"),
        ] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(idx_type.to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert row");
        }
        drop(w);

        purge_stale_memory_vamana_snapshots(&rt).await;

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            remaining.contains(&"global::memory_vamana::model-a".to_string()),
            "current-key global memory Vamana snapshot must be retained: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"local::memory_vamana::model-a".to_string()),
            "legacy per-namespace memory Vamana snapshot must be purged: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"tenant-a::memory_vamana::model-b".to_string()),
            "legacy per-namespace memory Vamana snapshot must be purged: {remaining:?}"
        );
        assert!(
            remaining.contains(&"local::vamana::model-a".to_string()),
            "unrelated knowledge Vamana rows must survive: {remaining:?}"
        );
    }

    /// Regression test (PR #1081 review): SQLite `LIKE` is ASCII case-insensitive, so
    /// `NOT LIKE 'global::memory_vamana::%'` treated a legacy `GLOBAL::memory_vamana::*`
    /// row (a valid namespace per namespace validation) as the retained lowercase key and
    /// never purged it. `GLOB` is case-sensitive and must tell the two apart.
    #[tokio::test]
    async fn test_purge_stale_memory_vamana_snapshots_is_case_sensitive() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let sql = rt.sql();

        let mut w = sql.writer().await.expect("writer");
        w.execute_script(
            "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
                .into(),
        )
        .await
        .expect("create table");

        for (ns, idx_type) in &[
            ("global::memory_vamana::model-a", "memory_vamana"),
            ("GLOBAL::memory_vamana::model-a", "memory_vamana"),
        ] {
            w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(idx_type.to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert row");
        }
        drop(w);

        purge_stale_memory_vamana_snapshots(&rt).await;

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query");

        let remaining: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("namespace") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert!(
            remaining.contains(&"global::memory_vamana::model-a".to_string()),
            "current-key lowercase global memory Vamana snapshot must be retained: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"GLOBAL::memory_vamana::model-a".to_string()),
            "legacy uppercase GLOBAL::memory_vamana snapshot must be purged, not mistaken for \
             the retained lowercase key: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn stale_fts_sweep_quotes_malicious_table_name_and_preserves_entities() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        // Seed a base row so `distinct_base_namespaces` returns only `local`
        // and the sweep guard does not skip the drop loop.
        rt.create_entity(&token, "concept", None, "seed", None, None, vec![])
            .await
            .expect("seed entity");

        let sql = rt.sql();
        let malicious = "fts_entities_x\"; DROP TABLE entities; --";
        {
            let mut w = sql.writer().await.expect("writer");
            let ddl = format!(
                "CREATE TABLE {} (rowid INTEGER)",
                quote_sqlite_identifier(malicious)
            );
            w.execute(SqlStatement {
                sql: ddl,
                params: vec![],
                label: None,
            })
            .await
            .expect("create malicious stale table");
        }

        sweep_stale_fts_partitions(&rt, "local").await;

        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT COUNT(*) AS c FROM entities".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("entities table must still exist and be queryable");
        let count = rows
            .first()
            .and_then(|row| row.get("c"))
            .map(|v| matches!(v, SqlValue::Integer(n) if *n >= 1))
            .unwrap_or(false);
        assert!(
            count,
            "entities table must survive the sweep with its seeded row intact"
        );

        let survivors = r
            .query_all(SqlStatement {
                sql: "SELECT name FROM sqlite_master WHERE name = ?1".into(),
                params: vec![SqlValue::Text(malicious.to_string())],
                label: None,
            })
            .await
            .expect("query sqlite_master");
        assert!(
            survivors.is_empty(),
            "malicious stale table should have been dropped"
        );
    }

    fn report_with(errors: u64, k_failed: u64, k_errored: bool) -> ReindexReport {
        ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            knowledge_atoms_indexed: Some(0),
            knowledge_sections_indexed: None,
            knowledge_fts_rebuild: None,
            knowledge_atoms_failed: k_failed,
            knowledge_pass_errored: k_errored,
            knowledge_ann_failed: false,
            knowledge_sections_failed: 0,
            models_used: vec![],
            truncation_by_model: BTreeMap::new(),
            elapsed_ms: 0,
            errors_skipped: errors,
            entities_fts_failed: 0,
            notes_fts_failed: 0,
            epoch_bump_failed: false,
        }
    }

    #[test]
    fn report_serializes_per_model_truncation_accounting() {
        let mut report = report_with(0, 0, false);
        report.truncation_by_model.insert(
            "strict-model".to_string(),
            EmbeddingTruncationReport {
                truncated: 2,
                discarded_bytes: 17,
            },
        );
        let json = serde_json::to_value(report).expect("serialize report");
        assert_eq!(json["truncation_by_model"]["strict-model"]["truncated"], 2);
        assert_eq!(
            json["truncation_by_model"]["strict-model"]["discarded_bytes"],
            17
        );
    }

    #[test]
    fn human_report_renders_per_model_truncation_in_sorted_order() {
        let mut report = report_with(0, 0, false);
        report.truncation_by_model.insert(
            "zeta-model".to_string(),
            EmbeddingTruncationReport {
                truncated: 3,
                discarded_bytes: 29,
            },
        );
        report.truncation_by_model.insert(
            "alpha-model".to_string(),
            EmbeddingTruncationReport {
                truncated: 1,
                discarded_bytes: 7,
            },
        );

        let rendered = render_human_report(&report);
        let truncation_lines: Vec<&str> = rendered
            .lines()
            .filter(|line| line.starts_with("Embedding truncation"))
            .collect();
        assert_eq!(
            truncation_lines,
            [
                "Embedding truncation (alpha-model): 1 input truncated, 7 bytes discarded",
                "Embedding truncation (zeta-model): 3 inputs truncated, 29 bytes discarded",
            ]
        );
    }

    #[test]
    fn has_failures_flags_each_failure_source() {
        assert!(!report_with(0, 0, false).has_failures());
        assert!(
            report_with(1, 0, false).has_failures(),
            "entity/note errors"
        );
        assert!(
            report_with(0, 1, false).has_failures(),
            "knowledge atom fails"
        );
        assert!(
            report_with(0, 0, true).has_failures(),
            "knowledge pass error"
        );
    }

    #[test]
    fn has_failures_flags_knowledge_ann_failed() {
        let report = ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            knowledge_atoms_indexed: Some(10),
            knowledge_sections_indexed: None,
            knowledge_fts_rebuild: None,
            knowledge_atoms_failed: 0,
            knowledge_pass_errored: false,
            knowledge_ann_failed: true,
            knowledge_sections_failed: 0,
            models_used: vec![],
            truncation_by_model: BTreeMap::new(),
            elapsed_ms: 0,
            errors_skipped: 0,
            entities_fts_failed: 0,
            notes_fts_failed: 0,
            epoch_bump_failed: false,
        };
        assert!(
            report.has_failures(),
            "knowledge_ann_failed alone must drive has_failures() = true"
        );
        assert!(
            decide_result(report.has_failures(), false).is_err(),
            "knowledge_ann_failed must fail closed (non-zero exit)"
        );
        assert!(
            decide_result(report.has_failures(), true).is_ok(),
            "best-effort downgrades knowledge_ann_failed to exit 0"
        );
    }

    #[test]
    fn has_failures_flags_knowledge_sections_failed() {
        let report = ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            knowledge_atoms_indexed: None,
            knowledge_sections_indexed: Some(0),
            knowledge_fts_rebuild: None,
            knowledge_atoms_failed: 0,
            knowledge_pass_errored: false,
            knowledge_ann_failed: false,
            knowledge_sections_failed: 3,
            models_used: vec![],
            truncation_by_model: BTreeMap::new(),
            elapsed_ms: 0,
            errors_skipped: 0,
            entities_fts_failed: 0,
            notes_fts_failed: 0,
            epoch_bump_failed: false,
        };
        assert!(
            report.has_failures(),
            "knowledge_sections_failed > 0 alone must drive has_failures() = true"
        );
        assert!(
            decide_result(report.has_failures(), false).is_err(),
            "knowledge_sections_failed must fail closed (non-zero exit)"
        );
        assert!(
            decide_result(report.has_failures(), true).is_ok(),
            "best-effort downgrades knowledge_sections_failed to exit 0"
        );
    }

    #[test]
    fn decide_result_fails_closed_by_default() {
        assert!(decide_result(false, false).is_ok(), "clean run exits 0");
        assert!(
            decide_result(true, false).is_err(),
            "failures fail closed (non-zero exit)"
        );
    }

    #[test]
    fn decide_result_best_effort_downgrades_to_ok() {
        assert!(
            decide_result(true, true).is_ok(),
            "best-effort downgrades failures to exit 0"
        );
        assert!(decide_result(false, true).is_ok());
    }

    #[test]
    fn rebuild_fts_conflicts_with_no_knowledge() {
        // The rebuild lives inside the knowledge pass; skipping that pass
        // while asking for the rebuild must be refused at parse time instead
        // of accepted and ignored.
        let err = ReindexArgs::try_parse_from(["reindex", "--rebuild-fts", "--no-knowledge"])
            .expect_err("--rebuild-fts with --no-knowledge must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let ok = ReindexArgs::try_parse_from(["reindex", "--rebuild-fts"])
            .expect("--rebuild-fts alone parses");
        assert!(ok.rebuild_fts);
    }

    #[test]
    fn rebuild_fts_is_off_unless_requested() {
        // The FTS indexes are global while every run targets one namespace,
        // so no run shape implies the rebuild: only the explicit flag does.
        let default_run = ReindexArgs::try_parse_from(["reindex"]).expect("bare reindex parses");
        assert!(!default_run.rebuild_fts);
        let keep_existing_run = ReindexArgs::try_parse_from(["reindex", "--keep-existing"])
            .expect("keep-existing run parses");
        assert!(!keep_existing_run.rebuild_fts);
    }

    // DB resolution parity with `kkernel exec` / `kkernel mcp`. The shared
    // helper is unit-tested in `dbpath`; here we assert reindex consumes it
    // through clap (`--db` / `KHIVE_DB` / `:memory:`) the same way.
    #[test]
    fn db_memory_sentinel_resolves_to_none() {
        assert_eq!(resolve_db_override(Some(":memory:")), Some(None));
    }

    #[test]
    fn db_explicit_path_resolves_to_some() {
        assert_eq!(
            resolve_db_override(Some("/tmp/kkernel-reindex-test.db")),
            Some(Some(PathBuf::from("/tmp/kkernel-reindex-test.db")))
        );
    }

    #[test]
    fn db_absent_leaves_default() {
        assert_eq!(resolve_db_override(None), None);
    }

    #[test]
    #[serial]
    fn declared_backends_require_an_explicit_reindex_target() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (config, _, _) = write_declared_backend_test_config(dir.path());

        let error = validate_declared_reindex_target(None, Some(&config))
            .expect_err("a topology-backed reindex without a target must fail closed");
        let message = error.to_string();
        assert!(message.contains("requires an explicit persistent --db / KHIVE_DB target"));
        assert!(message.contains(&config.display().to_string()));
    }

    #[test]
    #[serial]
    fn declared_secondary_backend_is_a_valid_reindex_target() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (config, _, knowledge) = write_declared_backend_test_config(dir.path());

        validate_declared_reindex_target(knowledge.to_str(), Some(&config))
            .expect("reindex may target any explicitly declared SQLite backend");
    }

    #[test]
    #[serial]
    fn undeclared_reindex_target_is_rejected_with_config_source() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (config, _, _) = write_declared_backend_test_config(dir.path());
        let wrong = dir.path().join("typo.db");

        let error = validate_declared_reindex_target(wrong.to_str(), Some(&config))
            .expect_err("an undeclared target must never be reindexed");
        let message = error.to_string();
        assert!(message.contains("is not a path declared in [[backends]]"));
        assert!(message.contains(&config.display().to_string()));
    }

    #[test]
    #[serial]
    fn read_only_declared_backend_is_refused_as_reindex_target() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (config, main, archive) = write_read_only_backend_test_config(dir.path());

        // Expected before the fix: the read-only path passes the validator,
        // since it only matched declared paths and never inspected
        // `read_only` — reindex always writes, so that is wrong.
        let error = validate_declared_reindex_target(archive.to_str(), Some(&config))
            .expect_err("a backend declared read_only must never be reindexed");
        let message = error.to_string();
        assert!(message.contains(&archive.display().to_string()));
        assert!(message.contains("read_only"));

        // Control: the writable backend in the same config still passes.
        validate_declared_reindex_target(main.to_str(), Some(&config))
            .expect("a writable declared backend remains a valid reindex target");

        // Control: an undeclared path is still refused as before.
        let wrong = dir.path().join("typo.db");
        validate_declared_reindex_target(wrong.to_str(), Some(&config))
            .expect_err("an undeclared target must never be reindexed");
    }

    #[test]
    #[serial]
    fn single_backend_reindex_keeps_ordinary_db_override_behavior() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = write_empty_test_config(dir.path());
        let target = dir.path().join("ordinary.db");

        validate_declared_reindex_target(target.to_str(), Some(&config))
            .expect("without [[backends]], --db remains an ordinary target");
    }

    #[test]
    #[serial]
    fn khive_db_env_binds_to_db_arg() {
        std::env::set_var("KHIVE_DB", "/tmp/kkernel-reindex-env.db");
        let args = ReindexArgs::parse_from(["reindex"]);
        std::env::remove_var("KHIVE_DB");
        assert_eq!(args.db.as_deref(), Some("/tmp/kkernel-reindex-env.db"));
    }

    #[test]
    #[serial]
    fn khive_config_env_binds_to_config_arg() {
        std::env::set_var("KHIVE_CONFIG", "/tmp/kkernel-reindex.toml");
        let args = ReindexArgs::parse_from(["reindex"]);
        std::env::remove_var("KHIVE_CONFIG");
        assert_eq!(
            args.config.as_deref(),
            Some(std::path::Path::new("/tmp/kkernel-reindex.toml"))
        );
    }

    // Namespace resolution parity with `kkernel mcp` under ADR-007 Rev 4 Rule 0:
    // when --namespace is omitted, the config file `[actor] id` does NOT set
    // default_namespace — it stays `local` (writes pin to local). A non-`'local'`
    // actor.id IS folded into the default READ visible-set (Rule 3b), but that
    // does not affect default_namespace. When --namespace is explicit, it routes
    // storage (Rule 1 / reindex's explicit namespace channel) and overrides local.
    #[test]
    #[serial]
    fn namespace_absent_defers_to_local_not_config_actor_id() {
        use std::io::Write;
        std::env::remove_var("KHIVE_NAMESPACE");
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");

        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("khive.toml");
        let mut f = std::fs::File::create(&config_path).expect("create config");
        f.write_all(b"[actor]\nid = \"lambda:prod\"\n")
            .expect("write config");

        // No --namespace: config [actor] id is attribution only (Rule 0), so the
        // effective namespace stays `local` — it must NOT become lambda:prod.
        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&config_path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config");
        assert_eq!(
            resolved.default_namespace.as_str(),
            "local",
            "omitted --namespace must stay local; config [actor] id does NOT set \
             default_namespace (ADR-007 Rev 4 Rule 0)"
        );

        // Explicit --namespace must override [actor] id.
        let resolved_explicit = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&config_path),
            namespace: Namespace::parse("explicit-ns").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config explicit");
        assert_eq!(
            resolved_explicit.default_namespace.as_str(),
            "explicit-ns",
            "explicit --namespace must override config [actor] id"
        );
    }

    #[test]
    #[serial]
    fn namespace_env_var_sets_explicit_flag() {
        std::env::set_var("KHIVE_NAMESPACE", "env-ns");
        let args = ReindexArgs::parse_from(["reindex"]);
        std::env::remove_var("KHIVE_NAMESPACE");
        assert_eq!(
            args.namespace.as_deref(),
            Some("env-ns"),
            "KHIVE_NAMESPACE env var must bind to --namespace"
        );
        assert!(
            args.namespace.is_some(),
            "env var binding must make namespace Some (explicit)"
        );
    }

    #[test]
    #[serial]
    fn namespace_absent_defaults_to_none() {
        std::env::remove_var("KHIVE_NAMESPACE");
        let args = ReindexArgs::parse_from(["reindex"]);
        assert!(
            args.namespace.is_none(),
            "omitted --namespace must be None (not a String default)"
        );
    }

    // The old reindex path committed a
    // subject-scoped DELETE (`drop_vectors_for_subjects`) before embedding, so a
    // transient embed OR insert failure left the prior vector permanently ABSENT
    // instead of merely stale. `embed_and_store_batch` no longer pre-deletes —
    // it hands the runtime straight to `VectorStore::insert_batch`, which
    // replaces each subject's row atomically (DELETE+INSERT under one
    // per-record SAVEPOINT, see `replace_vector_row_dml` /
    // `insert_batch_rollback_restores_deleted_stale_after_post_delete_insert_failure`
    // in khive-db). This test proves the guarantee at the `embed_and_store_batch`
    // call boundary: force the embed step (not the storage layer) to fail after a
    // stale vector already exists, and assert the stale vector SURVIVES —
    // no-worse-than-stale, never absent.
    #[tokio::test]
    async fn embed_and_store_batch_preserves_stale_vector_on_embed_failure() {
        use async_trait::async_trait;
        use khive_runtime::{EmbedderProvider, RuntimeConfig, RuntimeError};
        use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
        use std::sync::Arc;

        struct FailingStubService;

        #[async_trait]
        impl EmbeddingService for FailingStubService {
            async fn embed(
                &self,
                _texts: &[String],
                _model: EmbeddingModel,
            ) -> Result<Vec<Vec<f32>>, EmbedError> {
                Err(EmbedError::ModelInitialization(
                    "simulated transient embed failure".into(),
                ))
            }

            fn supports_model(&self, _model: EmbeddingModel) -> bool {
                true
            }

            fn name(&self) -> &'static str {
                "stub-failing-embed"
            }
        }

        struct StubProvider {
            model_name: &'static str,
            dims: usize,
        }

        #[async_trait]
        impl EmbedderProvider for StubProvider {
            fn name(&self) -> &str {
                self.model_name
            }

            fn dimensions(&self) -> usize {
                self.dims
            }

            async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
                Ok(Arc::new(FailingStubService))
            }
        }

        const MODEL: &str = "stub-model-embed-fail";
        const DIMS: usize = 4;
        const NS: &str = "local";

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(StubProvider {
            model_name: MODEL,
            dims: DIMS,
        });

        let ns = Namespace::parse(NS).expect("ns");
        let token = rt.authorize(ns).expect("authorize");
        let store = rt.vectors_for_model(&token, MODEL).expect("store");

        // Stale row already present before the reindex pass runs.
        let subject_id = Uuid::new_v4();
        let stale_vec = vec![0.1_f32, 0.2, 0.3, 0.4];
        store
            .insert_batch(vec![VectorRecord {
                subject_id,
                kind: SubstrateKind::Note,
                namespace: NS.to_string(),
                field: "note.content".to_string(),
                embedding_model: Some(MODEL.to_string()),
                vectors: vec![stale_vec.clone()],
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("stale insert_batch");
        assert_eq!(store.count().await.expect("count before"), 1);

        // drop_existing = true — this is exactly the code path that used to
        // commit a pre-delete before the (now-failing) embed call.
        let staged = vec![(subject_id, "some note content".to_string())];
        let errors = embed_and_store_batch(
            &rt,
            &token,
            &[MODEL.to_string()],
            NS,
            &staged,
            SubstrateKind::Note,
            "note.content",
            true,
            &mut BTreeMap::new(),
        )
        .await;

        assert_eq!(
            errors, 1,
            "the forced embed failure must count as one error"
        );

        // The stale vector must still be present and unchanged: no pre-delete
        // ran, and embed failing means insert_batch was never even called.
        let after = store.count().await.expect("count after");
        assert_eq!(
            after, 1,
            "an embed failure must leave the prior vector in place, not absent"
        );
        assert!(
            store
                .batch_exists(&[subject_id], NS)
                .await
                .expect("batch_exists after failure")
                .contains(&subject_id),
            "stale subject must still resolve to a row after the embed failure"
        );

        let hits = store
            .search(khive_storage::types::VectorSearchRequest {
                query_vectors: vec![stale_vec],
                top_k: 1,
                namespace: Some(NS.to_string()),
                kind: Some(SubstrateKind::Note),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search after failure");
        assert_eq!(hits.len(), 1, "stale vector must still be searchable");
        assert_eq!(hits[0].subject_id, subject_id);
        assert!(
            hits[0].score.to_f64() > 0.999,
            "surviving row must be the original stale vector, not a partial write"
        );
    }

    #[test]
    fn has_failures_flags_notes_fts_failed() {
        let report = ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            knowledge_atoms_indexed: None,
            knowledge_sections_indexed: None,
            knowledge_fts_rebuild: None,
            knowledge_atoms_failed: 0,
            knowledge_pass_errored: false,
            knowledge_ann_failed: false,
            knowledge_sections_failed: 0,
            models_used: vec![],
            truncation_by_model: BTreeMap::new(),
            elapsed_ms: 0,
            errors_skipped: 0,
            entities_fts_failed: 0,
            notes_fts_failed: 1,
            epoch_bump_failed: false,
        };
        assert!(
            report.has_failures(),
            "notes_fts_failed > 0 alone must drive has_failures() = true"
        );
        assert!(
            decide_result(report.has_failures(), false).is_err(),
            "notes_fts_failed must fail closed (non-zero exit)"
        );
        assert!(
            decide_result(report.has_failures(), true).is_ok(),
            "best-effort downgrades notes_fts_failed to exit 0"
        );
    }

    // Parity: note_fts_document must produce the same body/title as operations.rs.
    #[test]
    fn note_fts_document_parity_with_name() {
        let mut note = Note::new("local", "memory", "the content body");
        note.name = Some("my title".to_string());
        let doc = note_fts_document(&note);
        assert_eq!(doc.subject_id, note.id);
        assert_eq!(doc.namespace, "local");
        assert_eq!(doc.title.as_deref(), Some("my title"));
        assert_eq!(doc.body, "my title the content body");
        assert_eq!(doc.kind, SubstrateKind::Note);
    }

    #[test]
    fn note_fts_document_parity_without_name() {
        let note = Note::new("local", "memory", "body only content");
        let doc = note_fts_document(&note);
        assert!(doc.title.is_none());
        assert_eq!(doc.body, "body only content");
    }

    // Regression: insert N notes via NoteStore (bypassing FTS), run
    // fts_backfill_notes_batch, assert FTS count == N and a keyword hit works.
    #[tokio::test]
    async fn fts_backfill_populates_pre_existing_notes() {
        use khive_storage::types::TextFilter;
        use khive_types::SubstrateKind;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        let notes: Vec<Note> = (0..5)
            .map(|i| {
                Note::new(
                    "local",
                    "memory",
                    format!("zxqsentinel{i} backfill content"),
                )
            })
            .collect();

        let note_store = rt.notes(&token).expect("note store");
        for note in &notes {
            note_store
                .upsert_note(note.clone())
                .await
                .expect("upsert note");
        }

        // FTS should be empty before backfill (notes inserted via store, not runtime).
        let fts = rt.text_for_notes(&token).expect("FTS store");
        let before = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Note],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("count before");
        assert_eq!(before, 0, "FTS must be empty before backfill");

        // Run the backfill.
        let errors = fts_backfill_notes_batch(&rt, &token, &notes).await;
        assert_eq!(errors, 0, "backfill must produce zero errors");

        // FTS must now contain one row per note.
        let after = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Note],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("count after");
        assert_eq!(after, 5, "FTS must contain exactly N docs after backfill");

        // A keyword from the first note must be retrievable.
        let hits = fts
            .search(khive_storage::types::TextSearchRequest {
                query: "zxqsentinel0".to_string(),
                mode: khive_storage::types::TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .expect("FTS search");
        assert!(
            hits.iter().any(|h| h.subject_id == notes[0].id),
            "pre-existing note must be findable by FTS after backfill"
        );
    }

    // Cross-path equality: a note created through the runtime (operations.rs path)
    // must produce a stored FTS document that is field-identical to calling
    // note_fts_document() on the same Note. Catches drift between the shared
    // constructor and any caller that previously built documents inline.
    // Properties are included so that metadata and updated_at are also under test.
    #[tokio::test]
    async fn note_fts_document_matches_runtime_create_path() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        // Create with a name AND properties so metadata, title+body composition,
        // and updated_at derivation are all exercised.
        let props = serde_json::json!({"key": "value", "score": 42});
        let note = rt
            .create_note(
                &token,
                "observation",
                Some("cross path title"),
                "cross path content body",
                None,
                Some(props),
                vec![],
            )
            .await
            .expect("create_note");

        // Retrieve the stored FTS document written by the create path.
        let fts = rt.text_for_notes(&token).expect("FTS store");
        let stored = fts
            .get_document("local", note.id)
            .await
            .expect("get_document")
            .expect("document must exist after create");

        // Build the expected document using the shared constructor on the same note.
        let expected = note_fts_document(&note);

        assert_eq!(stored.subject_id, expected.subject_id, "subject_id");
        assert_eq!(stored.kind, expected.kind, "kind");
        assert_eq!(stored.title, expected.title, "title");
        assert_eq!(stored.body, expected.body, "body");
        assert_eq!(stored.namespace, expected.namespace, "namespace");
        assert_eq!(stored.tags, expected.tags, "tags");
        assert_eq!(stored.metadata, expected.metadata, "metadata");
        // Compare at microsecond resolution — DateTime<Utc> round-trips through i64.
        assert_eq!(
            stored.updated_at.timestamp_micros(),
            note.updated_at,
            "updated_at must be derived from the note, not Utc::now()"
        );
    }

    // Regression: run_reindex with no embedding model must still populate FTS for
    // pre-existing notes. Guards against reintroduction of the early-return that
    // skipped the FTS pass when model_names was empty.
    #[tokio::test]
    async fn run_reindex_populates_fts_without_embedding_model() {
        use khive_storage::types::TextFilter;
        use khive_types::SubstrateKind;

        // Use a temp-file db so run_reindex (which builds its own runtime) and our
        // verification pass share the same on-disk state.
        let db_file = tempfile::NamedTempFile::new().expect("temp db file");
        let db_path = db_file.path().to_str().expect("utf8 path").to_string();
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let config = write_empty_test_config(config_dir.path());

        // Seed notes via a runtime opened on the same file BEFORE calling run_reindex.
        {
            let cfg = resolve_runtime_config(RuntimeConfigInputs {
                db: Some(&db_path),
                config: Some(&config),
                namespace: Namespace::parse("local").expect("ns"),
                namespace_explicit: true,
                actor_explicit: false,
                no_embed: true,
                packs: None,
                brain_profile: None,
            })
            .expect("resolve config for seed");
            let rt = KhiveRuntime::new(cfg).expect("seed runtime");
            let token = rt
                .authorize(Namespace::parse("local").expect("ns"))
                .expect("authorize");
            let note_store = rt.notes(&token).expect("note store");
            for i in 0..3usize {
                note_store
                    .upsert_note(Note::new(
                        "local",
                        "observation",
                        format!("run-reindex-sentinel{i} body"),
                    ))
                    .await
                    .expect("upsert seed note");
            }
        }

        // run_reindex with no embedding model and --no-knowledge.
        let args = ReindexArgs {
            db: Some(db_path.clone()),
            config: Some(config.clone()),
            model: None,
            batch_size: 100,
            keep_existing: false,
            namespace: Some("local".to_string()),
            knowledge_only: false,
            no_knowledge: true,
            best_effort: true,
            no_sections: false,
            sections_only: false,
            rebuild_fts: false,
            human: false,
        };
        run_reindex(args).await.expect("run_reindex must succeed");

        // Verify FTS was populated by re-opening the db.
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_path),
            config: Some(&config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config for verify");
        let rt = KhiveRuntime::new(cfg).expect("verify runtime");
        let token = rt
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let fts = rt.text_for_notes(&token).expect("FTS store");
        let count = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Note],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("fts count");
        assert_eq!(
            count, 3,
            "run_reindex must populate FTS even when no embedding model is configured"
        );
    }

    // No-embedding-model FTS: when no embedding model is registered, the note
    // loop and FTS backfill must still execute — FTS needs no embedder.
    #[tokio::test]
    async fn fts_backfill_runs_without_embedding_model() {
        use khive_storage::types::TextFilter;
        use khive_types::SubstrateKind;

        // KhiveRuntime::memory() has no embedding model configured.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        let notes: Vec<Note> = (0..3)
            .map(|i| {
                Note::new(
                    "local",
                    "observation",
                    format!("nomodel-sentinel{i} content"),
                )
            })
            .collect();

        let note_store = rt.notes(&token).expect("note store");
        for note in &notes {
            note_store.upsert_note(note.clone()).await.expect("upsert");
        }

        // With no embedding model, embed_and_store_batch is a no-op but
        // fts_backfill_notes_batch must still populate the FTS index.
        let errors = fts_backfill_notes_batch(&rt, &token, &notes).await;
        assert_eq!(
            errors, 0,
            "FTS backfill must succeed with no embedding model"
        );

        let fts = rt.text_for_notes(&token).expect("FTS store");
        let count = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Note],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("count");
        assert_eq!(
            count, 3,
            "FTS must be populated even when no embedding model is configured"
        );
    }

    // Idempotency: running backfill twice must not duplicate rows.
    #[tokio::test]
    async fn fts_backfill_is_idempotent() {
        use khive_storage::types::TextFilter;
        use khive_types::SubstrateKind;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        let notes: Vec<Note> = (0..3)
            .map(|i| Note::new("local", "memory", format!("idemnote{i} content")))
            .collect();

        let note_store = rt.notes(&token).expect("note store");
        for note in &notes {
            note_store
                .upsert_note(note.clone())
                .await
                .expect("upsert note");
        }

        let errors1 = fts_backfill_notes_batch(&rt, &token, &notes).await;
        let errors2 = fts_backfill_notes_batch(&rt, &token, &notes).await;
        assert_eq!(errors1, 0);
        assert_eq!(errors2, 0);

        let fts = rt.text_for_notes(&token).expect("FTS store");
        let count = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Note],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("count");
        assert_eq!(count, 3, "second backfill pass must not duplicate rows");
    }

    // Parity: entity_fts_document must produce the same body/title as
    // operations.rs create_entity.
    #[test]
    fn entity_fts_document_parity_with_description() {
        use khive_storage::entity::Entity;
        let mut entity = Entity::new("local", "concept", "TestEntity");
        entity = entity.with_description("detail text");
        let doc = entity_fts_document(&entity);
        assert_eq!(doc.subject_id, entity.id);
        assert_eq!(doc.namespace, "local");
        assert_eq!(doc.title.as_deref(), Some("TestEntity"));
        assert_eq!(doc.body, "TestEntity detail text");
        assert_eq!(doc.kind, SubstrateKind::Entity);
    }

    #[test]
    fn entity_fts_document_parity_without_description() {
        use khive_storage::entity::Entity;
        let entity = Entity::new("local", "concept", "NameOnly");
        let doc = entity_fts_document(&entity);
        assert_eq!(doc.title.as_deref(), Some("NameOnly"));
        assert_eq!(doc.body, "NameOnly");
    }

    // Regression: insert N entities via EntityStore (bypassing FTS), run
    // fts_backfill_entities_batch, assert FTS count == N and a keyword hit works.
    #[tokio::test]
    async fn fts_backfill_populates_pre_existing_entities() {
        use khive_storage::entity::Entity;
        use khive_storage::types::TextFilter;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        let entities: Vec<Entity> = (0..5)
            .map(|i| {
                Entity::new("local", "concept", format!("zxqentitysentinel{i}"))
                    .with_description(format!("backfill entity description {i}"))
            })
            .collect();

        let entity_store = rt.entities(&token).expect("entity store");
        for entity in &entities {
            entity_store
                .upsert_entity(entity.clone())
                .await
                .expect("upsert entity");
        }

        // FTS should be empty before backfill (entities inserted via store, not runtime).
        let fts = rt.text(&token).expect("FTS store");
        let before = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Entity],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("count before");
        assert_eq!(before, 0, "FTS must be empty before backfill");

        // Run the backfill.
        let errors = fts_backfill_entities_batch(&rt, &token, &entities).await;
        assert_eq!(errors, 0, "backfill must produce zero errors");

        // FTS must now contain one row per entity.
        let after = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Entity],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("count after");
        assert_eq!(after, 5, "FTS must contain exactly N docs after backfill");

        // A keyword from the first entity must be retrievable.
        let hits = fts
            .search(khive_storage::types::TextSearchRequest {
                query: "zxqentitysentinel0".to_string(),
                mode: khive_storage::types::TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .expect("FTS search");
        assert!(
            hits.iter().any(|h| h.subject_id == entities[0].id),
            "pre-existing entity must be findable by FTS after backfill"
        );
    }

    // run_reindex with no embedding model must populate entity FTS for pre-existing
    // entities. Guards the entity FTS path running independently of embedding.
    #[tokio::test]
    async fn run_reindex_populates_entity_fts_without_embedding_model() {
        use khive_storage::entity::Entity;
        use khive_storage::types::TextFilter;

        let db_file = tempfile::NamedTempFile::new().expect("temp db file");
        let db_path = db_file.path().to_str().expect("utf8 path").to_string();
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let config = write_empty_test_config(config_dir.path());

        // Seed entities via EntityStore (bypassing runtime FTS write).
        {
            let cfg = resolve_runtime_config(RuntimeConfigInputs {
                db: Some(&db_path),
                config: Some(&config),
                namespace: Namespace::parse("local").expect("ns"),
                namespace_explicit: true,
                actor_explicit: false,
                no_embed: true,
                packs: None,
                brain_profile: None,
            })
            .expect("resolve config for seed");
            let rt = KhiveRuntime::new(cfg).expect("seed runtime");
            let token = rt
                .authorize(Namespace::parse("local").expect("ns"))
                .expect("authorize");
            let entity_store = rt.entities(&token).expect("entity store");
            for i in 0..3usize {
                entity_store
                    .upsert_entity(Entity::new(
                        "local",
                        "concept",
                        format!("reindex-entity-sentinel{i}"),
                    ))
                    .await
                    .expect("upsert seed entity");
            }
        }

        let args = ReindexArgs {
            db: Some(db_path.clone()),
            config: Some(config.clone()),
            model: None,
            batch_size: 100,
            keep_existing: false,
            namespace: Some("local".to_string()),
            knowledge_only: false,
            no_knowledge: true,
            best_effort: true,
            no_sections: false,
            sections_only: false,
            rebuild_fts: false,
            human: false,
        };
        run_reindex(args).await.expect("run_reindex must succeed");

        // Verify entity FTS was populated.
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_path),
            config: Some(&config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config for verify");
        let rt = KhiveRuntime::new(cfg).expect("verify runtime");
        let token = rt
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let fts = rt.text(&token).expect("entity FTS store");
        let count = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Entity],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("fts count");
        assert_eq!(
            count, 3,
            "run_reindex must populate entity FTS even when no embedding model is configured"
        );
    }

    /// Seeds one knowledge atom and deliberately desynchronizes `fts_knowledge`
    /// against it (same technique as the pack-level FTS-repair regression),
    /// so a caller can observe whether a later `run_reindex` call repaired it.
    async fn seed_desynced_knowledge_fts(db_path: &str, config: &std::path::Path) {
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(db_path),
            config: Some(config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config for seed");
        let rt = KhiveRuntime::new(cfg).expect("seed runtime");
        let mut writer = rt.sql().writer().await.expect("knowledge writer");
        writer
            .execute_batch(vec![
                SqlStatement {
                    sql: "INSERT INTO knowledge_atoms \
                          (id, namespace, slug, name, content, created_at, updated_at) \
                          VALUES ('9de50000-0000-4000-8000-000000000001', 'local', \
                                  'reindex-fts-scope', 'Reindex FTS Scope', \
                                  'scopeable lexical atom document', 1, 1)"
                        .into(),
                    params: vec![],
                    label: Some("test.reindex_fts_scope.atom".into()),
                },
                SqlStatement {
                    sql: "INSERT INTO fts_knowledge \
                          (fts_knowledge, rowid, id, namespace, slug, name, content) \
                          SELECT 'delete', rowid, id, namespace, slug, name, content \
                          FROM knowledge_atoms \
                          WHERE id = '9de50000-0000-4000-8000-000000000001'"
                        .into(),
                    params: vec![],
                    label: Some("test.reindex_fts_scope.desync".into()),
                },
            ])
            .await
            .expect("seed and desynchronize fts_knowledge");
    }

    /// True once `fts_knowledge` again matches the seeded atom's content —
    /// i.e. the desync `seed_desynced_knowledge_fts` created was repaired.
    async fn knowledge_fts_repaired(db_path: &str, config: &std::path::Path) -> bool {
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(db_path),
            config: Some(config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config for verify");
        let rt = KhiveRuntime::new(cfg).expect("verify runtime");
        let mut reader = rt.sql().reader().await.expect("knowledge reader");
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT count(*) AS n FROM fts_knowledge \
                      WHERE fts_knowledge MATCH 'scopeable'"
                    .into(),
                params: vec![],
                label: Some("test.reindex_fts_scope.verify".into()),
            })
            .await
            .expect("query fts_knowledge")
            .expect("count row");
        matches!(row.get("n"), Some(SqlValue::Integer(1)))
    }

    // Regression for the FTS-rebuild scoping fix: an explicit `--namespace`
    // makes the run scoped, and `fts_knowledge`/`fts_sections` are global —
    // rebuilding them on a scoped run is exactly the wasted writer work this
    // fix removes. Before the fix, `rebuild_fts` was unconditionally `true`
    // and this desync would have been repaired regardless of scope.
    #[tokio::test]
    async fn run_reindex_scoped_run_does_not_rebuild_fts() {
        let db_file = tempfile::NamedTempFile::new().expect("temp db file");
        let db_path = db_file.path().to_str().expect("utf8 path").to_string();
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let config = write_empty_test_config(config_dir.path());

        seed_desynced_knowledge_fts(&db_path, &config).await;

        let args = ReindexArgs {
            db: Some(db_path.clone()),
            config: Some(config.clone()),
            model: None,
            batch_size: 100,
            keep_existing: false,
            namespace: Some("local".to_string()), // explicit → scoped run
            knowledge_only: false,
            no_knowledge: false,
            best_effort: true,
            no_sections: false,
            sections_only: false,
            rebuild_fts: false,
            human: false,
        };
        run_reindex(args).await.expect("run_reindex must succeed");

        assert!(
            !knowledge_fts_repaired(&db_path, &config).await,
            "a namespace-scoped run must NOT rebuild the global knowledge FTS indexes"
        );
    }

    // Companion to the scoped-run test above: a run with no explicit
    // --namespace still targets one namespace (the configured one), so it
    // does not imply the global rebuild either. Only the flag does.
    #[tokio::test]
    async fn run_reindex_without_the_flag_does_not_rebuild_fts() {
        let db_file = tempfile::NamedTempFile::new().expect("temp db file");
        let db_path = db_file.path().to_str().expect("utf8 path").to_string();
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let config = write_empty_test_config(config_dir.path());

        seed_desynced_knowledge_fts(&db_path, &config).await;

        let args = ReindexArgs {
            db: Some(db_path.clone()),
            config: Some(config.clone()),
            model: None,
            batch_size: 100,
            keep_existing: false,
            namespace: None, // omitted namespace resolves to the configured one
            knowledge_only: false,
            no_knowledge: false,
            best_effort: true,
            no_sections: false,
            sections_only: false,
            rebuild_fts: false,
            human: false,
        };
        run_reindex(args).await.expect("run_reindex must succeed");

        assert!(
            !knowledge_fts_repaired(&db_path, &config).await,
            "a run without --rebuild-fts must not rebuild the global knowledge FTS indexes"
        );
    }

    // The explicit flag routes through the operator entry point and repairs
    // the desync end to end.
    #[tokio::test]
    async fn run_reindex_with_the_flag_rebuilds_fts() {
        let db_file = tempfile::NamedTempFile::new().expect("temp db file");
        let db_path = db_file.path().to_str().expect("utf8 path").to_string();
        let config_dir = tempfile::tempdir().expect("config temp dir");
        let config = write_empty_test_config(config_dir.path());

        seed_desynced_knowledge_fts(&db_path, &config).await;

        let args = ReindexArgs {
            db: Some(db_path.clone()),
            config: Some(config.clone()),
            model: None,
            batch_size: 100,
            keep_existing: false,
            namespace: None,
            knowledge_only: false,
            no_knowledge: false,
            best_effort: true,
            no_sections: false,
            sections_only: false,
            rebuild_fts: true,
            human: false,
        };
        run_reindex(args).await.expect("run_reindex must succeed");

        assert!(
            knowledge_fts_repaired(&db_path, &config).await,
            "--rebuild-fts must rebuild and repair the global knowledge FTS indexes"
        );
    }

    // Idempotency: running entity FTS backfill twice must not duplicate rows.
    #[tokio::test]
    async fn fts_backfill_entities_is_idempotent() {
        use khive_storage::entity::Entity;
        use khive_storage::types::TextFilter;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        let entities: Vec<Entity> = (0..3)
            .map(|i| Entity::new("local", "concept", format!("idem-entity{i}")))
            .collect();

        let entity_store = rt.entities(&token).expect("entity store");
        for entity in &entities {
            entity_store
                .upsert_entity(entity.clone())
                .await
                .expect("upsert entity");
        }

        let errors1 = fts_backfill_entities_batch(&rt, &token, &entities).await;
        let errors2 = fts_backfill_entities_batch(&rt, &token, &entities).await;
        assert_eq!(errors1, 0);
        assert_eq!(errors2, 0);

        let fts = rt.text(&token).expect("FTS store");
        let count = fts
            .count(TextFilter {
                kinds: vec![SubstrateKind::Entity],
                record_kinds: vec![],
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("count");
        assert_eq!(
            count, 3,
            "second backfill pass must not duplicate entity rows"
        );
    }

    // has_failures must flag entities_fts_failed alone.
    #[test]
    fn has_failures_flags_entities_fts_failed() {
        let report = ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            knowledge_atoms_indexed: None,
            knowledge_sections_indexed: None,
            knowledge_fts_rebuild: None,
            knowledge_atoms_failed: 0,
            knowledge_pass_errored: false,
            knowledge_ann_failed: false,
            knowledge_sections_failed: 0,
            models_used: vec![],
            truncation_by_model: BTreeMap::new(),
            elapsed_ms: 0,
            errors_skipped: 0,
            entities_fts_failed: 1,
            notes_fts_failed: 0,
            epoch_bump_failed: false,
        };
        assert!(
            report.has_failures(),
            "entities_fts_failed > 0 alone must drive has_failures() = true"
        );
        assert!(
            decide_result(report.has_failures(), false).is_err(),
            "entities_fts_failed must fail closed (non-zero exit)"
        );
        assert!(
            decide_result(report.has_failures(), true).is_ok(),
            "best-effort downgrades entities_fts_failed to exit 0"
        );
    }
}
