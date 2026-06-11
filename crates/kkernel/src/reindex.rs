//! `kkernel reindex` — rebuild embedding vectors and FTS documents for entities and notes.
//!
//! This is an infrastructure-level operation that walks all entities and notes
//! in a database and (re-)embeds them using the specified model and backfills the
//! FTS index. It is NOT a pack verb — it operates on the raw runtime stores
//! regardless of which packs are loaded.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use uuid::Uuid;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_runtime::{note_fts_document, KhiveRuntime, Namespace};
use khive_storage::error::StorageError;
use khive_storage::note::Note;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;

const MAX_EMBED_BYTES: usize = 32_768;

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
    /// ephemeral in-memory database, matching `kkernel mcp`/`kkernel exec`.
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

    /// Records per embedding batch (default 100, max 500).
    #[arg(long, default_value = "100")]
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

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,
}

#[derive(Serialize)]
struct ReindexReport {
    entities_processed: u64,
    notes_processed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    knowledge_atoms_indexed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    knowledge_sections_indexed: Option<u64>,
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
    elapsed_ms: u64,
    /// Entity/note vector inserts that failed across all engines.
    errors_skipped: u64,
    /// Note FTS upserts that failed during the backfill pass.
    notes_fts_failed: u64,
}

impl ReindexReport {
    /// Did any part of the run fail? Drives the fail-closed exit decision.
    fn has_failures(&self) -> bool {
        self.errors_skipped > 0
            || self.notes_fts_failed > 0
            || self.knowledge_atoms_failed > 0
            || self.knowledge_pass_errored
            || self.knowledge_ann_failed
            || self.knowledge_sections_failed > 0
    }
}

/// Embed `staged` with every model in `model_names` and store one vector record
/// per model — mirroring the multi-model write path in the runtime. Returns the
/// number of vector inserts that failed.
///
/// With `drop_existing`, each model's prior vector for an id is deleted before
/// insert. Otherwise (`--keep-existing`), ids already embedded in a given model
/// are skipped for that model only.
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

        let texts: Vec<String> = subset.iter().map(|(_, t)| truncate_text(t)).collect();
        match rt.embed_batch_with_model(model_name, &texts).await {
            Ok(embeddings) if embeddings.len() == subset.len() => {
                for ((id, _), emb) in subset.iter().zip(embeddings.iter()) {
                    if drop_existing {
                        let _ = vectors.delete(*id).await;
                    }
                    if let Err(e) = vectors
                        .insert(*id, kind, namespace, field, vec![emb.clone()])
                        .await
                    {
                        tracing::warn!(id = %id, model = %model_name, error = %e, "vector insert failed");
                        errors += 1;
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
    let mut notes_fts_failed: u64 = 0;

    // ── entities + notes (graph substrate) ────────────────────────────────────
    if do_graph {
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

            let mut staged: Vec<(Uuid, String)> = Vec::with_capacity(n);
            for entity in &batch {
                let text = match &entity.description {
                    Some(d) if !d.is_empty() => format!("{} {}", entity.name, d),
                    _ => entity.name.clone(),
                };
                if !text.trim().is_empty() {
                    staged.push((entity.id, text));
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
                )
                .await;
                entities_processed += staged.len() as u64;
            }
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

            let mut staged: Vec<(Uuid, String)> = Vec::with_capacity(n);
            for note in &batch {
                let text = note.content.clone();
                if !text.trim().is_empty() {
                    staged.push((note.id, text));
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
                )
                .await;
                notes_processed += staged.len() as u64;
            }

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

    let elapsed_ms = start.elapsed().as_millis() as u64;

    let report = ReindexReport {
        entities_processed,
        notes_processed,
        knowledge_atoms_indexed,
        knowledge_sections_indexed,
        knowledge_atoms_failed,
        knowledge_pass_errored,
        knowledge_ann_failed,
        knowledge_sections_failed,
        models_used: model_names,
        elapsed_ms,
        errors_skipped,
        notes_fts_failed,
    };

    print_report(&report, args.human);
    finish(&report, args.best_effort)
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

async fn invalidate_vamana_snapshots(rt: &KhiveRuntime, namespace: &str) -> anyhow::Result<()> {
    use khive_storage::types::{SqlStatement, SqlValue};

    let pattern = format!("{namespace}::vamana::%");
    let sql = rt.sql();
    let mut writer = sql
        .writer()
        .await
        .context("open SQL writer for Vamana snapshot invalidation")?;

    match writer
        .execute(SqlStatement {
            sql: "DELETE FROM retrieval_snapshots WHERE namespace LIKE ?1".into(),
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

fn truncate_text(t: &str) -> String {
    if t.len() <= MAX_EMBED_BYTES {
        t.to_string()
    } else {
        let mut end = MAX_EMBED_BYTES;
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t[..end].to_string()
    }
}

fn print_report(report: &ReindexReport, human: bool) {
    if human {
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
        println!(
            "{status}: {} entities, {} notes{}{} ({} vector errors, {} FTS errors) in {}ms",
            report.entities_processed,
            report.notes_processed,
            atoms,
            sections,
            report.errors_skipped,
            report.notes_fts_failed,
            report.elapsed_ms
        );
        if report.notes_fts_failed > 0 {
            println!(
                "FTS backfill: {} note upserts FAILED",
                report.notes_fts_failed
            );
        }
        if report.knowledge_pass_errored {
            println!("Knowledge pass: FAILED (did not run to completion)");
        } else if report.knowledge_atoms_failed > 0 {
            println!(
                "Knowledge pass: {} atom vector inserts FAILED",
                report.knowledge_atoms_failed
            );
        }
        if report.knowledge_sections_failed > 0 {
            println!(
                "Knowledge sections: {} section embed/write failures",
                report.knowledge_sections_failed
            );
        }
        if report.knowledge_ann_failed {
            println!("Knowledge ANN: FAILED (snapshot not rebuilt/persisted)");
        }
        if !report.models_used.is_empty() {
            println!("Models: {}", report.models_used.join(", "));
        }
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

    fn report_with(errors: u64, k_failed: u64, k_errored: bool) -> ReindexReport {
        ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            knowledge_atoms_indexed: Some(0),
            knowledge_sections_indexed: None,
            knowledge_atoms_failed: k_failed,
            knowledge_pass_errored: k_errored,
            knowledge_ann_failed: false,
            knowledge_sections_failed: 0,
            models_used: vec![],
            elapsed_ms: 0,
            errors_skipped: errors,
            notes_fts_failed: 0,
        }
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
            knowledge_atoms_failed: 0,
            knowledge_pass_errored: false,
            knowledge_ann_failed: true,
            knowledge_sections_failed: 0,
            models_used: vec![],
            elapsed_ms: 0,
            errors_skipped: 0,
            notes_fts_failed: 0,
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
            knowledge_atoms_failed: 0,
            knowledge_pass_errored: false,
            knowledge_ann_failed: false,
            knowledge_sections_failed: 3,
            models_used: vec![],
            elapsed_ms: 0,
            errors_skipped: 0,
            notes_fts_failed: 0,
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

    // Namespace resolution parity with `kkernel mcp`: when --namespace is omitted,
    // the config file `[actor] id` must set the effective namespace — same as the
    // MCP path. When --namespace is explicit, it must override the config tier.
    #[test]
    #[serial]
    fn namespace_absent_honors_config_actor_id() {
        use std::io::Write;
        std::env::remove_var("KHIVE_NAMESPACE");
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");

        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("khive.toml");
        let mut f = std::fs::File::create(&config_path).expect("create config");
        f.write_all(b"[actor]\nid = \"lambda:prod\"\n")
            .expect("write config");

        // No --namespace: must pick up [actor] id from config file.
        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&config_path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config");
        assert_eq!(
            resolved.default_namespace.as_str(),
            "lambda:prod",
            "omitted --namespace must defer to config [actor] id"
        );

        // Explicit --namespace must override [actor] id.
        let resolved_explicit = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&config_path),
            namespace: Namespace::parse("explicit-ns").expect("ns"),
            namespace_explicit: true,
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
    fn namespace_absent_defaults_to_none() {
        let args = ReindexArgs::parse_from(["reindex"]);
        assert!(
            args.namespace.is_none(),
            "omitted --namespace must be None (not a String default)"
        );
    }

    #[test]
    fn has_failures_flags_notes_fts_failed() {
        let report = ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            knowledge_atoms_indexed: None,
            knowledge_sections_indexed: None,
            knowledge_atoms_failed: 0,
            knowledge_pass_errored: false,
            knowledge_ann_failed: false,
            knowledge_sections_failed: 0,
            models_used: vec![],
            elapsed_ms: 0,
            errors_skipped: 0,
            notes_fts_failed: 1,
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
    #[tokio::test]
    async fn note_fts_document_matches_runtime_create_path() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        // Create with a name so the title+body composition is exercised.
        let note = rt
            .create_note(
                &token,
                "observation",
                Some("cross path title"),
                "cross path content body",
                None,
                None,
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
                namespaces: vec!["local".to_string()],
                ids: vec![],
            })
            .await
            .expect("count");
        assert_eq!(count, 3, "second backfill pass must not duplicate rows");
    }
}
