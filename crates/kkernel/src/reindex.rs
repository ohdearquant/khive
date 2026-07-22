//! `kkernel reindex` — rebuild embedding vectors and FTS documents for entities and notes.
//!
//! This is an infrastructure-level operation that walks all entities and notes
//! in a database and (re-)embeds them using the specified model and backfills the
//! FTS index. It is NOT a pack verb — it operates on the raw runtime stores
//! regardless of which packs are loaded.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use uuid::Uuid;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_runtime::{entity_fts_document, note_fts_document, KhiveRuntime, Namespace};
use khive_storage::entity::Entity;
use khive_storage::error::StorageError;
use khive_storage::note::Note;
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

/// Arguments for `kkernel reindex` — rebuilds embedding vectors for entities
/// and notes, fanning out across every configured embedding engine (resolved
/// with the same config-file/env precedence as `kkernel mcp`).
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
    /// registered models.
    #[arg(long)]
    pub model: Option<String>,

    /// Records embedded per batch — also the DB page and write batch (default
    /// 128, max 500). One `embed_document_batch` call processes this many records.
    #[arg(long, default_value = "128")]
    pub batch_size: u32,

    /// Keep existing vectors instead of dropping before re-embedding.
    #[arg(long)]
    pub keep_existing: bool,

    /// Repair only missing embeddings. Skips FTS backfill and reads only base
    /// rows without a vector for each target model.
    #[arg(long)]
    pub embeds_only: bool,

    /// Namespace to operate on. When omitted, the config file `[actor] id` (if
    /// any) is honored — matching the same precedence as `kkernel mcp`. An
    /// explicit `--namespace` / `KHIVE_NAMESPACE` overrides the config tier.
    #[arg(long, env = "KHIVE_NAMESPACE")]
    pub namespace: Option<String>,

    /// Downgrade partial failures (failed model, failed vector insert) to a
    /// warning and still exit 0. Without this flag, reindex FAILS CLOSED: any
    /// failure returns a non-zero exit so automation does not treat a partial
    /// rebuild as a clean one.
    #[arg(long)]
    pub best_effort: bool,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,
}

#[derive(Serialize)]
struct ReindexReport {
    entities_processed: u64,
    notes_processed: u64,
    models_used: Vec<String>,
    elapsed_ms: u64,
    /// Entity/note vector inserts that failed across all engines.
    errors_skipped: u64,
    /// Entity FTS upserts that failed during the backfill pass.
    entities_fts_failed: u64,
    /// Note FTS upserts that failed during the backfill pass.
    notes_fts_failed: u64,
}

impl ReindexReport {
    /// Did any part of the run fail? Drives the fail-closed exit decision.
    fn has_failures(&self) -> bool {
        self.errors_skipped > 0 || self.entities_fts_failed > 0 || self.notes_fts_failed > 0
    }
}

/// Drop ALL existing vector rows for `subject_ids` in the model's canonical table,
/// regardless of their stored namespace. This is required before re-embedding
/// because the vec table's PRIMARY KEY is `(subject_id)` — not `(subject_id,
/// namespace)` — so a row written by a different namespace would collide on
/// re-insert. By deleting on subject_id alone we ensure the subsequent INSERT
/// lands cleanly with the base row's current namespace.
///
/// Resolves the store via `rt.vectors_for_model(token, model_name)` — the SAME
/// call the insert path uses — so alias resolution (`paraphrase` → canonical table
/// name) is handled identically in both directions. Best-effort: a failure to
/// resolve the store or delete is logged but does not abort; the subsequent INSERT
/// will either collide (counted as an error) or succeed.
async fn drop_vectors_for_subjects(
    rt: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    model_name: &str,
    ids: &[Uuid],
) {
    if ids.is_empty() {
        return;
    }
    let store = match rt.vectors_for_model(token, model_name) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(model = %model_name, error = %e, "drop_vectors_for_subjects: could not resolve store; skipping delete");
            return;
        }
    };
    if let Err(e) = store.delete_subjects(ids).await {
        tracing::warn!(model = %model_name, error = %e, "subject-scoped vector drop failed (continuing)");
    }
}

/// Embed `staged` with every model in `model_names` and store one vector record
/// per model — mirroring the multi-model write path in the runtime. Returns the
/// number of vector inserts that failed.
///
/// With `drop_existing`, all staged ids are (re)embedded. Before inserting, a
/// subject-scoped delete removes ANY existing row for each `subject_id` in the
/// model table, regardless of its stored namespace. This prevents UNIQUE
/// constraint violations when the database was relabeled and vec rows from a
/// prior namespace survive. The subsequent INSERT writes the current base-row
/// namespace. With `--keep-existing`, existing vectors are preserved and ids
/// already embedded are skipped.
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

    // Subject-scoped drop: remove ANY existing vec rows for these subject_ids
    // in each model table, regardless of stored namespace. This ensures the
    // re-insert never hits a UNIQUE collision when vec rows from a prior
    // namespace survive a relabel operation. Done once per model here; the
    // SqliteVecStore::insert DELETE is namespace-scoped and would miss rows
    // stored under a different namespace.
    if drop_existing && !staged.is_empty() {
        let subject_ids: Vec<Uuid> = staged.iter().map(|(id, _)| *id).collect();
        for model_name in model_names {
            drop_vectors_for_subjects(rt, token, model_name, &subject_ids).await;
        }
    }

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

        let texts: Vec<String> = subset.iter().map(|(_, text)| text.clone()).collect();
        match rt.embed_document_batch_with_model(model_name, &texts).await {
            Ok(embeddings) if embeddings.len() == subset.len() => {
                // No pre-delete: SqliteVecStore::insert wraps DELETE+INSERT in
                // a single transaction so a failed INSERT rolls back the DELETE
                // and the prior vector survives (no-worse-than-stale). A separate
                // committed delete before insert re-introduces the stranding window.
                for ((id, _), emb) in subset.iter().zip(embeddings.iter()) {
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

fn vector_table_name(model_key: &str) -> Result<String> {
    if model_key.is_empty()
        || !model_key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        anyhow::bail!("invalid vector model key {model_key:?}");
    }
    Ok(format!("vec_{model_key}"))
}

async fn missing_embedding_batch(
    rt: &KhiveRuntime,
    namespace: &str,
    vector_table: &str,
    embedding_model: &str,
    kind: SubstrateKind,
    after: &str,
    limit: u32,
) -> Result<Vec<(Uuid, String)>> {
    use khive_storage::types::{SqlStatement, SqlValue};

    let (select, table, text_predicate) = match kind {
        SubstrateKind::Entity => (
            "base.id, base.name, base.description",
            "entities",
            "TRIM(base.name) <> ''",
        ),
        SubstrateKind::Note => ("base.id, base.content", "notes", "TRIM(base.content) <> ''"),
        _ => anyhow::bail!("embeds-only reindex does not support {kind}"),
    };
    let sql = format!(
        "SELECT {select} FROM {table} AS base \
         WHERE base.namespace = ?1 AND base.deleted_at IS NULL AND base.id > ?2 \
         AND {text_predicate} \
         AND NOT EXISTS (SELECT 1 FROM {vector_table} AS vectors \
             WHERE vectors.subject_id = base.id AND vectors.namespace = ?1 \
             AND vectors.embedding_model = ?3) \
         ORDER BY base.id LIMIT ?4"
    );
    let rows = {
        let mut reader = rt
            .sql()
            .reader()
            .await
            .context("open SQL reader for embeds-only reindex")?;
        reader
            .query_all(SqlStatement {
                sql,
                params: vec![
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text(after.to_string()),
                    SqlValue::Text(embedding_model.to_string()),
                    SqlValue::Integer(i64::from(limit)),
                ],
                label: Some("reindex_missing_embeddings".into()),
            })
            .await
            .context("select rows missing embeddings")?
    };

    rows.into_iter()
        .map(|row| {
            let text = |name: &str| match row.get(name) {
                Some(SqlValue::Text(value)) => Ok(value.clone()),
                Some(SqlValue::Null) => Ok(String::new()),
                _ => anyhow::bail!("missing or invalid {name} in embeds-only row"),
            };
            let id = text("id")?
                .parse::<Uuid>()
                .context("invalid subject id in embeds-only row")?;
            let content = match kind {
                SubstrateKind::Entity => {
                    let name = text("name")?;
                    let description = text("description")?;
                    if description.is_empty() {
                        name
                    } else {
                        format!("{name} {description}")
                    }
                }
                SubstrateKind::Note => text("content")?,
                _ => unreachable!("kind checked above"),
            };
            Ok((id, content))
        })
        .collect()
}

struct RepairModelTarget {
    model_name: String,
    embedding_model: String,
    vector_table: String,
}

async fn prepare_repair_models(
    rt: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    model_names: &[String],
) -> Result<Vec<RepairModelTarget>> {
    let mut targets = Vec::with_capacity(model_names.len());
    let mut identities_by_table = BTreeMap::<String, BTreeSet<String>>::new();

    for model_name in model_names {
        let vectors = rt
            .vectors_for_model(token, model_name)
            .with_context(|| format!("resolve vector store for model {model_name}"))?;
        let info = vectors
            .info()
            .await
            .with_context(|| format!("inspect vector store for model {model_name}"))?;
        let vector_table = vector_table_name(&info.model_name)?;
        let embedding_model = rt
            .resolve_embedding_model(Some(model_name))
            .map(|model| model.to_string())
            .unwrap_or_else(|_| model_name.clone());

        identities_by_table
            .entry(vector_table.to_ascii_lowercase())
            .or_default()
            .insert(embedding_model.clone());
        targets.push(RepairModelTarget {
            model_name: model_name.clone(),
            embedding_model,
            vector_table,
        });
    }

    if let Some((table, identities)) = identities_by_table
        .into_iter()
        .find(|(_, identities)| identities.len() > 1)
    {
        let identities: Vec<_> = identities.into_iter().collect();
        anyhow::bail!(
            "embeds-only repair models {identities:?} share vector table {table:?}; colliding registered model names cannot be repaired or served from one table"
        );
    }

    Ok(targets)
}

async fn repair_missing_embeddings(
    rt: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    model_names: &[String],
    namespace: &str,
    batch_size: u32,
) -> Result<(u64, u64, u64)> {
    let targets = prepare_repair_models(rt, token, model_names).await?;
    let mut entities_processed = 0u64;
    let mut notes_processed = 0u64;
    let mut errors = 0u64;

    for target in targets {
        for (kind, field) in [
            (SubstrateKind::Entity, "entity.body"),
            (SubstrateKind::Note, "note.content"),
        ] {
            let mut after = String::new();
            loop {
                let batch = missing_embedding_batch(
                    rt,
                    namespace,
                    &target.vector_table,
                    &target.embedding_model,
                    kind,
                    &after,
                    batch_size,
                )
                .await?;
                let Some((last_id, _)) = batch.last() else {
                    break;
                };
                after = last_id.to_string();
                // A selected subject may still have a stale row in another namespace.
                errors += embed_and_store_batch(
                    rt,
                    token,
                    std::slice::from_ref(&target.model_name),
                    namespace,
                    &batch,
                    kind,
                    field,
                    true,
                )
                .await;
                match kind {
                    SubstrateKind::Entity => entities_processed += batch.len() as u64,
                    SubstrateKind::Note => notes_processed += batch.len() as u64,
                    _ => unreachable!("repair kinds are fixed above"),
                }
            }
        }
    }

    Ok((entities_processed, notes_processed, errors))
}

/// Re-embed entities and notes, fanning out across every configured embedding
/// engine. Engines, db path, and config are resolved with the same precedence
/// as `kkernel mcp` so reindex writes the SAME vectors the MCP server serves
/// recall from. Fails closed on any partial failure unless `--best-effort` is
/// set.
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

    // Explicit --model targets a single engine; otherwise fan out to ALL
    // registered engines, matching the runtime's multi-model write path so a
    // reindex reproduces exactly what create/update would have embedded.
    //
    // When no embedding model is configured, model_names is empty: the embedding
    // loop is a no-op but the note loop still runs for FTS backfill, which needs
    // no embedder and must never be skipped due to a missing embedding config.
    let model_names: Vec<String> = match args.model.as_deref().filter(|s| !s.is_empty()) {
        Some(name) => vec![name.to_string()],
        None => {
            let names = rt.registered_embedding_model_names();
            if names.is_empty() && !args.embeds_only {
                eprintln!("warning: no embedding model configured — skipping vector embedding; FTS backfill will still run");
            }
            names
        }
    };

    if args.embeds_only && model_names.is_empty() {
        anyhow::bail!("--embeds-only requires at least one configured embedding model");
    }

    let batch_size = args.batch_size.clamp(1, 500);
    let drop_existing = !args.keep_existing;
    let ns_str = token.namespace().as_str().to_owned();
    let start = std::time::Instant::now();

    let mut entities_processed: u64 = 0;
    let mut notes_processed: u64 = 0;
    let mut errors_skipped: u64 = 0;
    let mut entities_fts_failed: u64 = 0;
    let mut notes_fts_failed: u64 = 0;

    if args.embeds_only {
        (entities_processed, notes_processed, errors_skipped) =
            repair_missing_embeddings(&rt, &token, &model_names, &ns_str, batch_size).await?;
        if entities_processed + notes_processed > 0 {
            if let Err(e) = invalidate_vamana_snapshots(&rt, &ns_str).await {
                tracing::warn!(error = %e, "failed to invalidate Vamana snapshots after reindex");
            }
        }
        let report = ReindexReport {
            entities_processed,
            notes_processed,
            models_used: model_names,
            elapsed_ms: start.elapsed().as_millis() as u64,
            errors_skipped,
            entities_fts_failed,
            notes_fts_failed,
        };
        print_report(&report, args.human);
        return finish(&report, args.best_effort);
    }

    // ── entities + notes (graph substrate) ────────────────────────────────────
    {
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

        // Drop per-namespace FTS partition tables that survived the V4 migration
        // (tables created by the runtime before the migration ran, or on databases
        // that were migrated but not swept). The sweep is guarded: it only runs
        // when this reindex pass covered every distinct namespace in the base
        // entities/notes tables. If any namespace is uncovered, sweeping would
        // orphan those rows (they were dropped from the old partition and never
        // written to the new unified table). On a single-namespace (post-relabel)
        // db the guard always passes and the sweep runs normally.
        sweep_stale_fts_partitions(&rt, &ns_str).await;
    } // entities + notes

    let elapsed_ms = start.elapsed().as_millis() as u64;

    let report = ReindexReport {
        entities_processed,
        notes_processed,
        models_used: model_names,
        elapsed_ms,
        errors_skipped,
        entities_fts_failed,
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

fn print_report(report: &ReindexReport, human: bool) {
    if human {
        let status = if report.has_failures() {
            "Reindex completed WITH FAILURES"
        } else {
            "Reindex complete"
        };
        let fts_errors = report.entities_fts_failed + report.notes_fts_failed;
        println!(
            "{status}: {} entities, {} notes ({} vector errors, {} FTS errors) in {}ms",
            report.entities_processed,
            report.notes_processed,
            report.errors_skipped,
            fts_errors,
            report.elapsed_ms
        );
        if report.entities_fts_failed > 0 {
            println!(
                "FTS backfill: {} entity upserts FAILED",
                report.entities_fts_failed
            );
        }
        if report.notes_fts_failed > 0 {
            println!(
                "FTS backfill: {} note upserts FAILED",
                report.notes_fts_failed
            );
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

    const REPAIR_MODEL: &str = "repair-test-model";
    const REPAIR_TABLE: &str = "vec_repair_test_model";
    const REPAIR_DIMS: usize = 4;

    struct RepairEmbeddingService;

    #[async_trait::async_trait]
    impl lattice_embed::EmbeddingService for RepairEmbeddingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: lattice_embed::EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            Ok(vec![vec![0.75; REPAIR_DIMS]; texts.len()])
        }

        fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "repair-test"
        }
    }

    struct RepairEmbedderProvider {
        model_name: &'static str,
    }

    #[async_trait::async_trait]
    impl khive_runtime::EmbedderProvider for RepairEmbedderProvider {
        fn name(&self) -> &str {
            self.model_name
        }

        fn dimensions(&self) -> usize {
            REPAIR_DIMS
        }

        async fn build(
            &self,
        ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, khive_runtime::RuntimeError>
        {
            Ok(std::sync::Arc::new(RepairEmbeddingService))
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

    fn report_with(errors: u64) -> ReindexReport {
        ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            models_used: vec![],
            elapsed_ms: 0,
            errors_skipped: errors,
            entities_fts_failed: 0,
            notes_fts_failed: 0,
        }
    }

    #[test]
    fn has_failures_flags_each_failure_source() {
        assert!(!report_with(0).has_failures());
        assert!(report_with(1).has_failures(), "entity/note errors");
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
    fn embeds_only_arg_is_opt_in() {
        let default_args = ReindexArgs::parse_from(["reindex"]);
        assert!(!default_args.embeds_only);

        let repair_args = ReindexArgs::parse_from(["reindex", "--embeds-only"]);
        assert!(repair_args.embeds_only);
    }

    #[tokio::test]
    async fn embeds_only_selection_returns_only_notes_missing_target_model() {
        use khive_runtime::RuntimeConfig;
        use khive_storage::types::VectorRecord;
        use lattice_embed::EmbeddingModel;

        const MODEL: &str = "paraphrase-multilingual-minilm-l12-v2";
        const TABLE: &str = "vec_paraphrase_multilingual_minilm_l12_v2";

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![EmbeddingModel::ParaphraseMultilingualMiniLmL12V2],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        let token = rt
            .authorize(Namespace::parse("local").expect("namespace"))
            .expect("authorize");
        let notes = rt.notes(&token).expect("notes");
        let existing = Note::new("local", "observation", "already embedded");
        let missing = Note::new("local", "observation", "needs repair");
        notes
            .upsert_note(existing.clone())
            .await
            .expect("seed existing note");
        notes
            .upsert_note(missing.clone())
            .await
            .expect("seed missing note");

        rt.vectors_for_model(&token, MODEL)
            .expect("vectors")
            .insert_batch(vec![VectorRecord {
                subject_id: existing.id,
                kind: SubstrateKind::Note,
                namespace: "local".to_string(),
                field: "note.content".to_string(),
                embedding_model: Some(MODEL.to_string()),
                vectors: vec![vec![0.1; 384]],
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("seed vector");

        let selected =
            missing_embedding_batch(&rt, "local", TABLE, MODEL, SubstrateKind::Note, "", 100)
                .await
                .expect("select missing notes");

        assert_eq!(selected, vec![(missing.id, missing.content)]);
    }

    #[tokio::test]
    async fn embeds_only_repair_replaces_stale_cross_namespace_vector() {
        use khive_runtime::RuntimeConfig;
        use khive_storage::types::VectorRecord;

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(RepairEmbedderProvider {
            model_name: REPAIR_MODEL,
        });
        let token = rt
            .authorize(Namespace::parse("local").expect("namespace"))
            .expect("authorize");
        let note = Note::new("local", "observation", "repair this embedding");
        rt.notes(&token)
            .expect("notes")
            .upsert_note(note.clone())
            .await
            .expect("seed note");

        let vectors = rt.vectors_for_model(&token, REPAIR_MODEL).expect("vectors");
        vectors
            .insert_batch(vec![VectorRecord {
                subject_id: note.id,
                kind: SubstrateKind::Note,
                namespace: "stale-namespace".to_string(),
                field: "note.content".to_string(),
                embedding_model: Some(REPAIR_MODEL.to_string()),
                vectors: vec![vec![0.1; REPAIR_DIMS]],
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("seed stale vector");

        let result =
            repair_missing_embeddings(&rt, &token, &[REPAIR_MODEL.to_string()], "local", 100)
                .await
                .expect("repair embeddings");
        assert_eq!(result, (0, 1, 0));

        let mut reader = rt.sql().reader().await.expect("reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: format!(
                    "SELECT namespace FROM {REPAIR_TABLE} WHERE subject_id = ?1 ORDER BY namespace"
                ),
                params: vec![SqlValue::Text(note.id.to_string())],
                label: None,
            })
            .await
            .expect("read repaired vector");
        assert_eq!(rows.len(), 1, "repair must leave one vector row");
        assert!(
            matches!(rows[0].get("namespace"), Some(SqlValue::Text(value)) if value == "local"),
            "repair must replace the stale row with the base row namespace"
        );
    }

    #[tokio::test]
    async fn embeds_only_repair_does_not_treat_colliding_model_as_target_embedding() {
        use khive_runtime::RuntimeConfig;
        use khive_storage::types::VectorRecord;

        const MODEL_A: &str = "collision-model";
        const MODEL_B: &str = "collision_model";
        const TABLE: &str = "vec_collision_model";

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(RepairEmbedderProvider {
            model_name: MODEL_A,
        });
        rt.register_embedder(RepairEmbedderProvider {
            model_name: MODEL_B,
        });
        let token = rt
            .authorize(Namespace::parse("local").expect("namespace"))
            .expect("authorize");
        let note = Note::new("local", "observation", "repair colliding model");
        rt.notes(&token)
            .expect("notes")
            .upsert_note(note.clone())
            .await
            .expect("seed note");

        rt.vectors_for_model(&token, MODEL_A)
            .expect("model A vectors")
            .insert_batch(vec![VectorRecord {
                subject_id: note.id,
                kind: SubstrateKind::Note,
                namespace: "local".to_string(),
                field: "note.content".to_string(),
                embedding_model: Some(MODEL_A.to_string()),
                vectors: vec![vec![0.1; REPAIR_DIMS]],
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("seed model A vector");

        let result = repair_missing_embeddings(&rt, &token, &[MODEL_B.to_string()], "local", 100)
            .await
            .expect("repair model B embeddings");
        assert_eq!(result, (0, 1, 0));

        let mut reader = rt.sql().reader().await.expect("reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: format!(
                    "SELECT embedding_model FROM {TABLE} WHERE subject_id = ?1 AND namespace = ?2"
                ),
                params: vec![
                    SqlValue::Text(note.id.to_string()),
                    SqlValue::Text("local".to_string()),
                ],
                label: None,
            })
            .await
            .expect("read repaired vector");
        assert_eq!(rows.len(), 1, "repair must leave one vector row");
        assert!(
            matches!(rows[0].get("embedding_model"), Some(SqlValue::Text(value)) if value == MODEL_B),
            "repair must replace the colliding model A row with model B"
        );
    }

    #[tokio::test]
    async fn embeds_only_repair_refuses_colliding_models_before_vector_writes() {
        use khive_runtime::RuntimeConfig;

        const MODEL_A: &str = "collision-model";
        const MODEL_B: &str = "collision_model";
        const TABLE: &str = "vec_collision_model";

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(RepairEmbedderProvider {
            model_name: MODEL_A,
        });
        rt.register_embedder(RepairEmbedderProvider {
            model_name: MODEL_B,
        });
        let token = rt
            .authorize(Namespace::parse("local").expect("namespace"))
            .expect("authorize");
        let note = Note::new("local", "observation", "do not repair colliding models");
        rt.notes(&token)
            .expect("notes")
            .upsert_note(note)
            .await
            .expect("seed note");
        let vectors = rt.vectors_for_model(&token, MODEL_A).expect("vectors");
        assert_eq!(vectors.count().await.expect("count before repair"), 0);

        let error = repair_missing_embeddings(
            &rt,
            &token,
            &[MODEL_A.to_string(), MODEL_B.to_string()],
            "local",
            100,
        )
        .await
        .expect_err("colliding repair models must be refused");

        let message = error.to_string();
        assert!(message.contains(MODEL_A), "missing first model: {message}");
        assert!(message.contains(MODEL_B), "missing second model: {message}");
        assert!(message.contains(TABLE), "missing shared table: {message}");
        assert!(
            message.contains(
                "colliding registered model names cannot be repaired or served from one table"
            ),
            "missing refusal rationale: {message}"
        );
        assert_eq!(
            vectors.count().await.expect("count after refusal"),
            0,
            "collision refusal must happen before vector writes"
        );
    }

    #[tokio::test]
    async fn embeds_only_repair_refuses_case_distinct_models_before_vector_writes() {
        use khive_runtime::RuntimeConfig;

        const MODEL_A: &str = "collision";
        const MODEL_B: &str = "Collision";
        const TABLE: &str = "vec_collision";

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        rt.register_embedder(RepairEmbedderProvider {
            model_name: MODEL_A,
        });
        rt.register_embedder(RepairEmbedderProvider {
            model_name: MODEL_B,
        });
        let token = rt
            .authorize(Namespace::parse("local").expect("namespace"))
            .expect("authorize");
        let note = Note::new("local", "observation", "do not repair case-distinct models");
        rt.notes(&token)
            .expect("notes")
            .upsert_note(note)
            .await
            .expect("seed note");
        let vectors = rt.vectors_for_model(&token, MODEL_A).expect("vectors");
        assert_eq!(vectors.count().await.expect("count before repair"), 0);

        let error = repair_missing_embeddings(
            &rt,
            &token,
            &[MODEL_A.to_string(), MODEL_B.to_string()],
            "local",
            100,
        )
        .await
        .expect_err("case-distinct repair models must be refused");

        let message = error.to_string();
        assert!(message.contains(MODEL_A), "missing first model: {message}");
        assert!(message.contains(MODEL_B), "missing second model: {message}");
        assert!(message.contains(TABLE), "missing shared table: {message}");
        assert!(
            message.contains(
                "colliding registered model names cannot be repaired or served from one table"
            ),
            "missing refusal rationale: {message}"
        );
        assert_eq!(
            vectors.count().await.expect("count after refusal"),
            0,
            "case-distinct collision refusal must happen before vector writes"
        );
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

    // C3 regression: drop_vectors_for_subjects must target the SAME table as the insert path.
    //
    // The old code hand-sanitized model_name to derive the table name, which diverged from
    // the insert path when the model is registered under a canonical name (e.g.
    // "all-minilm-l6-v2" → table "vec_all_minilm_l6_v2" but a different sanitization of the
    // raw env-var alias would yield a different key). The fix routes both drop and insert
    // through rt.vectors_for_model() — the same Arc<dyn VectorStore> — so the table is
    // always consistent.
    //
    // This test inserts a vector via vectors_for_model, calls drop_vectors_for_subjects with
    // the same model name, and asserts the row is gone.
    #[tokio::test]
    async fn drop_vectors_for_subjects_targets_same_table_as_insert() {
        use async_trait::async_trait;
        use khive_runtime::{EmbedderProvider, RuntimeConfig, RuntimeError};
        use khive_storage::types::VectorRecord;
        use khive_types::SubstrateKind;
        use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
        use std::sync::Arc;

        struct StubService;

        #[async_trait]
        impl EmbeddingService for StubService {
            async fn embed(
                &self,
                _texts: &[String],
                _model: EmbeddingModel,
            ) -> Result<Vec<Vec<f32>>, EmbedError> {
                panic!("StubService::embed must not be called in this test")
            }

            fn supports_model(&self, _model: EmbeddingModel) -> bool {
                true
            }

            fn name(&self) -> &'static str {
                "stub-c3"
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
                Ok(Arc::new(StubService))
            }
        }

        const MODEL: &str = "stub-model-c3";
        const DIMS: usize = 4;

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

        let ns = khive_runtime::Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        // Obtain the store via the same path as the insert path uses.
        let store = rt.vectors_for_model(&token, MODEL).expect("store");

        // Insert one vector record.
        let subject_id = Uuid::new_v4();
        store
            .insert_batch(vec![VectorRecord {
                subject_id,
                kind: SubstrateKind::Note,
                namespace: "local".to_string(),
                field: "content".to_string(),
                embedding_model: Some(MODEL.to_string()),
                vectors: vec![vec![0.1_f32; DIMS]],
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("insert_batch");

        // Confirm the row exists.
        let before = store.count().await.expect("count before");
        assert_eq!(before, 1, "row must exist before drop");

        // Drop via drop_vectors_for_subjects — uses the same vectors_for_model path.
        drop_vectors_for_subjects(&rt, &token, MODEL, &[subject_id]).await;

        // Row must be gone.
        let after = store.count().await.expect("count after");
        assert_eq!(after, 0, "row must be deleted by drop_vectors_for_subjects");
    }

    // C3 alias regression: drop_vectors_for_subjects via a lattice ALIAS must target
    // the SAME canonical table as the insert path that used the full canonical name.
    //
    // Previously the old hand-sanitized path would diverge on aliases like "paraphrase"
    // (→ "paraphrase-multilingual-minilm-l12-v2" canonical, table
    // "vec_paraphrase_multilingual_minilm_l12_v2"). Both the insert path and the drop
    // path go through rt.vectors_for_model() which resolves the alias to the same
    // canonical VectorStore — the bug cannot happen with the current implementation.
    //
    // This test registers a stub under the canonical name, inserts via the canonical
    // name, drops via the short alias "paraphrase", and asserts the row is gone.
    // It FAILS if either path hand-derives the table name from the raw string instead
    // of routing through vectors_for_model().
    #[tokio::test]
    async fn drop_vectors_for_subjects_paraphrase_alias_targets_same_table_as_insert() {
        use khive_runtime::RuntimeConfig;
        use khive_storage::types::VectorRecord;
        use khive_types::SubstrateKind;
        use lattice_embed::EmbeddingModel;

        // "paraphrase" is a short alias for the built-in lattice model.
        // vectors_for_model resolves both the alias and the canonical name to the
        // same physical table (key = sanitize_key(CANONICAL), dims = 384).
        //
        // We register the model in RuntimeConfig so vectors_for_model(CANONICAL)
        // can resolve it without an Unknown model error.  We write raw VectorRecords
        // directly — the embedder is never called — so DIMS must equal the lattice
        // model's declared output dimension.
        const CANONICAL: &str = "paraphrase-multilingual-minilm-l12-v2";
        const ALIAS: &str = "paraphrase";
        // Must equal EmbeddingModel::ParaphraseMultilingualMiniLmL12V2::dimensions().
        const DIMS: usize = 384;

        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![EmbeddingModel::ParaphraseMultilingualMiniLmL12V2],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        // The paraphrase model is now in the registry under its canonical name.
        // vectors_for_model("paraphrase") and vectors_for_model(CANONICAL) must
        // both resolve to the same table — that is what this test verifies.

        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize");

        // Insert via the canonical name (same as the normal embed-and-store path).
        let store_canonical = rt
            .vectors_for_model(&token, CANONICAL)
            .expect("canonical store");
        let subject_id = Uuid::new_v4();
        store_canonical
            .insert_batch(vec![VectorRecord {
                subject_id,
                kind: SubstrateKind::Note,
                namespace: "local".to_string(),
                field: "content".to_string(),
                embedding_model: Some(CANONICAL.to_string()),
                vectors: vec![vec![0.1_f32; DIMS]],
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("insert_batch via canonical name");

        let before = store_canonical.count().await.expect("count before");
        assert_eq!(before, 1, "row must exist before alias-drop");

        // Drop via the ALIAS "paraphrase".  vectors_for_model resolves alias →
        // EmbeddingModel::ParaphraseMultilingualMiniLmL12V2 → same canonical table.
        // If the implementation ever diverges (hand-sanitizes the raw alias string),
        // the delete targets a different table and `after` stays 1 → test fails.
        drop_vectors_for_subjects(&rt, &token, ALIAS, &[subject_id]).await;

        let after = store_canonical.count().await.expect("count after");
        assert_eq!(
            after, 0,
            "alias-routed drop must delete from the same canonical table as insert; \
             after={after} (expected 0). \
             Failure means alias 'paraphrase' resolved to a different table than CANONICAL."
        );
    }

    #[test]
    fn has_failures_flags_notes_fts_failed() {
        let report = ReindexReport {
            entities_processed: 0,
            notes_processed: 0,
            models_used: vec![],
            elapsed_ms: 0,
            errors_skipped: 0,
            entities_fts_failed: 0,
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

        // Seed notes via a runtime opened on the same file BEFORE calling run_reindex.
        {
            let cfg = resolve_runtime_config(RuntimeConfigInputs {
                db: Some(&db_path),
                config: None,
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

        // run_reindex with no embedding model.
        let args = ReindexArgs {
            db: Some(db_path.clone()),
            config: None,
            model: None,
            batch_size: 100,
            keep_existing: false,
            embeds_only: false,
            namespace: Some("local".to_string()),
            best_effort: true,
            human: false,
        };
        run_reindex(args).await.expect("run_reindex must succeed");

        // Verify FTS was populated by re-opening the db.
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_path),
            config: None,
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

        // Seed entities via EntityStore (bypassing runtime FTS write).
        {
            let cfg = resolve_runtime_config(RuntimeConfigInputs {
                db: Some(&db_path),
                config: None,
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
            config: None,
            model: None,
            batch_size: 100,
            keep_existing: false,
            embeds_only: false,
            namespace: Some("local".to_string()),
            best_effort: true,
            human: false,
        };
        run_reindex(args).await.expect("run_reindex must succeed");

        // Verify entity FTS was populated.
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_path),
            config: None,
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
            models_used: vec![],
            elapsed_ms: 0,
            errors_skipped: 0,
            entities_fts_failed: 1,
            notes_fts_failed: 0,
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
