//! `kkernel reindex` — rebuild embedding vectors and FTS documents for entities and notes.
//!
//! This is an infrastructure-level operation that walks all entities and notes
//! in a database and (re-)embeds them using the specified model and backfills the
//! FTS index. It is NOT a pack verb — it operates on the raw runtime stores
//! regardless of which packs are loaded.

use crate::sql::sql;

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
use khive_types::{Pack, SubstrateKind};

mod fts_partition_sweep;
mod progress;
mod record_repair;

use fts_partition_sweep::sweep_stale_fts_partitions;
use progress::ProgressBar;

/// Arguments for `kkernel reindex` — rebuilds embedding vectors for entities,
/// notes, and the knowledge corpus, fanning out across every configured
/// embedding engine (resolved with the same config-file/env precedence as
/// `kkernel mcp`).
#[derive(Parser, Debug)]
pub struct ReindexArgs {
    /// Repair only this live entity or note: restore its FTS document and fill
    /// missing kind-eligible model vectors, preserving healthy indexes.
    #[arg(long, value_name = "UUID", conflicts_with_all = [
        "model", "batch_size", "knowledge_only", "no_sections", "sections_only", "rebuild_fts"
    ])]
    pub id: Option<Uuid>,

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

    /// Embedding model for entities/notes. When omitted, entities use all
    /// registered models and notes use their kind's embedding policy.
    /// Knowledge always uses the default embedder.
    #[arg(long)]
    pub model: Option<String>,

    /// Records embedded per batch — also the DB page and write batch (default
    /// 128, max 500). One `embed_document_batch` call processes this many records.
    #[arg(long, default_value_t = record_repair::DEFAULT_REINDEX_BATCH_SIZE)]
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
///
/// Returns `None` when no `[[backends]]` are declared (reindex keeps its
/// ordinary single-backend `--db` behavior); `Some` with the validated target
/// otherwise. Callers must open exactly the returned target's path — see
/// [`open_validated_reindex_backend`].
pub(crate) fn validate_declared_reindex_target(
    db: Option<&str>,
    config: Option<&std::path::Path>,
) -> Result<Option<khive_mcp::serve::ValidatedReindexTarget>> {
    let discovery_anchor = khive_mcp::serve::config_discovery_db_anchor(db);
    let loaded =
        KhiveConfig::load_with_home_fallback_and_source(config, discovery_anchor.as_deref())
            .context("load reindex khive config for backend-target validation")?;
    let config_source = loaded.as_ref().map(|(_, source)| source.as_path());
    let backends = loaded
        .as_ref()
        .map(|(config, _)| config.backends.as_slice())
        .unwrap_or_default();

    khive_mcp::serve::validate_effective_backend_alias_modes(backends)?;
    khive_mcp::serve::validate_reindex_db_target_with_source(db, backends, config_source)
}

/// Open the runtime `kkernel reindex` writes to, binding the open to the
/// filesystem identity [`validate_declared_reindex_target`] already checked.
///
/// When a declared-backend topology produced a validated target, this
/// overrides `cfg.db_path` to that target's canonical path — never the raw
/// `--db`/`KHIVE_DB` string, which may still name a symlink at this point —
/// and calls [`khive_mcp::serve::reverify_reindex_target_identity`]
/// immediately beforehand so a symlink retargeted, or the declared file
/// replaced in place, since validation is refused before `KhiveRuntime::new`
/// opens (and migrates) anything. Config discovery is unaffected: `cfg` was
/// already fully resolved by the caller against the raw `--db` input, and
/// only the field that decides which file gets opened is overridden here.
///
/// With no validated target (no `[[backends]]` declared), `cfg` is used
/// unchanged — ordinary single-backend reindex behavior.
pub(crate) fn open_validated_reindex_backend(
    mut cfg: khive_runtime::RuntimeConfig,
    validated: Option<&khive_mcp::serve::ValidatedReindexTarget>,
) -> Result<KhiveRuntime> {
    if let Some(validated) = validated {
        khive_mcp::serve::reverify_reindex_target_identity(validated)?;
        cfg.db_path = Some(validated.path.clone());
    }
    KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))
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
    /// Sections whose embedding input changed before a conditional vector
    /// write. A later keep-existing pass can fill their NULL vectors.
    #[serde(skip_serializing_if = "Option::is_none")]
    knowledge_sections_superseded: Option<u64>,
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
    /// Namespace Vamana snapshots could not be invalidated after graph writes.
    vamana_snapshot_invalidation_failed: bool,
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
            || self.vamana_snapshot_invalidation_failed
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
                    .map(|((id, _text), outcome)| VectorRecord {
                        subject_id: *id,
                        kind,
                        namespace: namespace.to_string(),
                        field: field.to_string(),
                        embedding_model: Some(model_name.clone()),
                        vectors: vec![outcome.vector],
                        text_fingerprint: outcome.prepared_text_fingerprint,
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

/// Re-embed entities, notes, and the knowledge corpus using each substrate's
/// embedding policy. Engines, db path, and config are resolved with
/// the same precedence as `kkernel mcp` so reindex writes the SAME vectors the
/// MCP server serves recall from. Fails closed on any partial failure unless
/// `--best-effort` is set.
pub async fn run_reindex(args: ReindexArgs) -> Result<()> {
    run_reindex_with_setup(args, |cfg| cfg, |_| Ok(())).await
}

async fn run_reindex_with_setup(
    args: ReindexArgs,
    config_setup: impl FnOnce(khive_runtime::RuntimeConfig) -> khive_runtime::RuntimeConfig,
    runtime_setup: impl FnOnce(&KhiveRuntime) -> Result<()>,
) -> Result<()> {
    record_repair::validate_args(&args)?;
    let validated_target =
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

    let cfg = config_setup(cfg);

    // Capture the resolved namespace BEFORE `new` consumes cfg — when
    // `!explicit`, `resolve_runtime_config` may have applied `[actor] id` from
    // the config file, making `cfg.default_namespace` differ from the CLI value.
    let resolved_ns = cfg.default_namespace.clone();
    let rt = open_validated_reindex_backend(cfg, validated_target.as_ref())?;
    runtime_setup(&rt)?;
    // Reindex does not construct a verb registry, so install the comm pack's
    // declared policy on this runtime just as MCP boot does for selected packs.
    if rt.config().packs.iter().any(|pack| pack == "comm") {
        rt.install_note_embedding_policies(
            <khive_pack_comm::CommPack as Pack>::NOTE_EMBEDDING_POLICIES,
        );
    }
    let token = rt
        .authorize(resolved_ns)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to authorize namespace")?;

    if let Some(id) = args.id {
        return record_repair::run(&rt, &token, id, args.human, args.best_effort).await;
    }

    // `--sections-only` is the narrowest scope: knowledge sections alone.
    let do_graph = !args.knowledge_only && !args.sections_only; // entities + notes
    let do_knowledge = !args.no_knowledge; // knowledge corpus
    let do_atoms = do_knowledge && !args.sections_only;
    let do_sections = do_knowledge && !args.no_sections;

    let rebuild_fts = args.rebuild_fts;

    // Explicit --model targets a single engine; otherwise entities fan out to
    // all registered engines and notes follow their kind's installed policy.
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
    let mut vamana_snapshot_invalidation_failed = false;

    // ── entities + notes (graph substrate) ────────────────────────────────────
    if do_graph {
        begin_reindex_epoch(&rt)
            .await
            .context("aborting reindex before any vector mutation")?;

        let entity_total = rt.count_entities(&token, None).await.unwrap_or(0);
        let entity_bar = ProgressBar::new("entities");
        entity_bar.update(0, entity_total);

        let mut entity_after = None;
        loop {
            let (batch, next_after) = rt
                .list_entities_after(&token, None, None, &[], entity_after, batch_size)
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

            entity_after = next_after;
            if entity_after.is_none() {
                break;
            }
        }
        entity_bar.finish();

        // ── notes ─────────────────────────────────────────────────────────────────
        let note_total = count_notes(&rt, &ns_str).await;
        let note_bar = ProgressBar::new("notes");
        note_bar.update(0, note_total);

        let mut note_after = None;
        loop {
            let (batch, next_after) = rt
                .list_notes_after(&token, None, note_after, batch_size)
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
                let mut staged_by_kind: BTreeMap<String, Vec<(Uuid, String)>> = BTreeMap::new();
                for note in &batch {
                    if note_has_embedding_text(note) {
                        staged_by_kind
                            .entry(note.kind.clone())
                            .or_default()
                            .push((note.id, note_embedding_text(note)));
                    }
                }

                let embeddable = staged_by_kind.values().map(Vec::len).sum();
                for (kind, staged) in staged_by_kind {
                    let eligible_models = if args.model.as_deref().is_some_and(|s| !s.is_empty()) {
                        model_names.clone()
                    } else {
                        rt.embedding_models_for_note_kind(&kind)
                    };
                    errors_skipped += embed_and_store_batch(
                        &rt,
                        &token,
                        &eligible_models,
                        &ns_str,
                        &staged,
                        SubstrateKind::Note,
                        "note.content",
                        drop_existing,
                        &mut truncation_by_model,
                    )
                    .await;
                }
                embeddable
            };
            notes_processed += embeddable as u64;

            // FTS backfill: index every note in this batch regardless of whether
            // it had content to embed. Mirrors the upsert_document call in
            // operations.rs — see note_fts_document for the parity contract.
            notes_fts_failed += fts_backfill_notes_batch(&rt, &token, &batch).await;

            note_bar.update(notes_processed, note_total);

            note_after = next_after;
            if note_after.is_none() {
                break;
            }
        }
        note_bar.finish();

        // Invalidate Vamana snapshots so the next warm-load triggers a rebuild
        // against the freshly re-embedded entity/note vectors.
        if let Err(e) = invalidate_vamana_snapshots(&rt, &ns_str).await {
            tracing::warn!(error = %e, "failed to invalidate Vamana snapshots after reindex");
            vamana_snapshot_invalidation_failed = true;
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
    let mut knowledge_sections_superseded: Option<u64> = None;
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
                    let superseded = v
                        .get("sections_superseded")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0);
                    knowledge_sections_superseded = (superseded > 0).then_some(superseded);
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
        knowledge_sections_superseded,
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
        vamana_snapshot_invalidation_failed,
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

async fn invalidate_vamana_snapshots(rt: &KhiveRuntime, namespace: &str) -> anyhow::Result<()> {
    let sql = rt.sql();
    let mut writer = sql
        .writer()
        .await
        .context("open SQL writer for Vamana snapshot invalidation")?;

    match khive_pack_knowledge::invalidate_legacy_vamana_snapshots(
        writer.as_mut(),
        namespace,
        "invalidate_vamana_snapshots",
    )
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
    use khive_storage::types::{SqlStatement, SqlValue};
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
            sql: sql!("retrieval_snapshots_delete_stale_memory_vamana").into(),
            params: vec![SqlValue::Text("global::memory_vamana::*".into())],
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
                sql: sql!("retrieval_snapshots_delete_active_memory_vamana").into(),
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
            sql: sql!("base_namespaces_list").into(),
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
            sql: sql!("notes_count").into(),
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
    if let Some(superseded) = report.knowledge_sections_superseded {
        output.push_str(&format!(
            "Knowledge sections: {superseded} changed during embedding; run a keep-existing reindex to fill remaining NULL vectors\n"
        ));
    }
    if report.vamana_snapshot_invalidation_failed {
        output.push_str(
            "Vamana snapshot invalidation: FAILED (snapshots may be stale; prior writes remain committed)\n",
        );
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
#[path = "reindex/tests/mod.rs"]
mod tests;
