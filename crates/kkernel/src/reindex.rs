//! `kkernel reindex` — rebuild embedding vectors for entities and notes.
//!
//! This is an infrastructure-level operation that walks all entities and notes
//! in a database and (re-)embeds them using the specified model. It is NOT a
//! pack verb — it operates on the raw runtime stores regardless of which packs
//! are loaded.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::error::StorageError;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;

const MAX_EMBED_BYTES: usize = 32_768;

/// Arguments for `kkernel reindex` — rebuilds embedding vectors for all entities and notes.
#[derive(Parser, Debug)]
pub struct ReindexArgs {
    /// Database path (defaults to `~/.khive/khive-graph.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Embedding model name (uses runtime default when omitted).
    #[arg(long)]
    pub model: Option<String>,

    /// Records per embedding batch (default 100, max 500).
    #[arg(long, default_value = "100")]
    pub batch_size: u32,

    /// Keep existing vectors instead of dropping before re-embedding.
    #[arg(long)]
    pub keep_existing: bool,

    /// Namespace to operate on.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,
}

#[derive(Serialize)]
struct ReindexReport {
    entities_processed: u64,
    notes_processed: u64,
    model_used: Option<String>,
    elapsed_ms: u64,
    errors_skipped: u64,
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

/// Re-embed all entities and notes using the configured or specified embedding model.
pub async fn run_reindex(args: ReindexArgs) -> Result<()> {
    let mut cfg = RuntimeConfig::default();
    if let Some(ref db) = args.db {
        cfg.db_path = Some(db.clone());
    }

    let rt = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let ns = Namespace::parse(&args.namespace).map_err(|e| anyhow::anyhow!("{e}"))?;
    let token = rt
        .authorize(ns)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to authorize namespace")?;

    let model_name: String = match args.model.as_deref().filter(|s| !s.is_empty()) {
        Some(name) => name.to_string(),
        None => {
            let default = rt.default_embedder_name();
            if default.is_empty() {
                let report = ReindexReport {
                    entities_processed: 0,
                    notes_processed: 0,
                    model_used: None,
                    elapsed_ms: 0,
                    errors_skipped: 0,
                };
                print_report(&report, args.human);
                eprintln!("warning: no embedding model configured");
                return Ok(());
            }
            default.to_string()
        }
    };

    let batch_size = args.batch_size.clamp(1, 500);
    let drop_existing = !args.keep_existing;
    let ns_str = token.namespace().as_str().to_owned();
    let start = std::time::Instant::now();

    let mut entities_processed: u64 = 0;
    let mut errors_skipped: u64 = 0;

    // ── entities ─────────────────────────────────────────────────────────────
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
            // When keeping existing vectors, skip IDs that already have embeddings.
            if !drop_existing {
                if let Ok(vectors) = rt.vectors_for_model(&token, &model_name) {
                    let all_ids: Vec<Uuid> = staged.iter().map(|(id, _)| *id).collect();
                    match filter_unembedded(vectors.as_ref(), &all_ids, &ns_str).await {
                        Ok(unembedded) => {
                            let keep: HashSet<Uuid> = unembedded.into_iter().collect();
                            staged.retain(|(id, _)| keep.contains(id));
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "filter_unembedded failed; skipping entity batch to honour --keep-existing");
                            errors_skipped += staged.len() as u64;
                            staged.clear();
                        }
                    }
                }
            }

            if !staged.is_empty() {
                let texts: Vec<String> = staged.iter().map(|(_, t)| truncate_text(t)).collect();

                match rt.embed_batch_with_model(&model_name, &texts).await {
                    Ok(embeddings) if embeddings.len() == staged.len() => {
                        match rt.vectors_for_model(&token, &model_name) {
                            Ok(vectors) => {
                                for ((id, _), emb) in staged.iter().zip(embeddings.iter()) {
                                    if drop_existing {
                                        let _ = vectors.delete(*id).await;
                                    }
                                    if let Err(e) = vectors
                                        .insert(
                                            *id,
                                            SubstrateKind::Entity,
                                            &ns_str,
                                            "entity.body",
                                            vec![emb.clone()],
                                        )
                                        .await
                                    {
                                        tracing::warn!(entity_id = %id, error = %e, "entity vector insert failed");
                                        errors_skipped += 1;
                                    }
                                }
                                entities_processed += staged.len() as u64;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to get vector store for model");
                                errors_skipped += staged.len() as u64;
                            }
                        }
                    }
                    Ok(_) => {
                        tracing::warn!("embedding count mismatch for entity batch");
                        errors_skipped += staged.len() as u64;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "entity embed_batch failed");
                        errors_skipped += staged.len() as u64;
                    }
                }
            }
        }

        if n < batch_size as usize {
            break;
        }
        entity_offset += n as u32;
    }

    // ── notes ─────────────────────────────────────────────────────────────────
    let mut notes_processed: u64 = 0;
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
            let text = match &note.name {
                Some(name) if !name.is_empty() => format!("{name} {}", note.content),
                _ => note.content.clone(),
            };
            if !text.trim().is_empty() {
                staged.push((note.id, text));
            }
        }

        if !staged.is_empty() {
            // When keeping existing vectors, skip IDs that already have embeddings.
            if !drop_existing {
                if let Ok(vectors) = rt.vectors_for_model(&token, &model_name) {
                    let all_ids: Vec<Uuid> = staged.iter().map(|(id, _)| *id).collect();
                    match filter_unembedded(vectors.as_ref(), &all_ids, &ns_str).await {
                        Ok(unembedded) => {
                            let keep: HashSet<Uuid> = unembedded.into_iter().collect();
                            staged.retain(|(id, _)| keep.contains(id));
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "filter_unembedded failed; skipping note batch to honour --keep-existing");
                            errors_skipped += staged.len() as u64;
                            staged.clear();
                        }
                    }
                }
            }

            if !staged.is_empty() {
                let texts: Vec<String> = staged.iter().map(|(_, t)| truncate_text(t)).collect();

                match rt.embed_batch_with_model(&model_name, &texts).await {
                    Ok(embeddings) if embeddings.len() == staged.len() => {
                        match rt.vectors_for_model(&token, &model_name) {
                            Ok(vectors) => {
                                for ((id, _), emb) in staged.iter().zip(embeddings.iter()) {
                                    if drop_existing {
                                        let _ = vectors.delete(*id).await;
                                    }
                                    if let Err(e) = vectors
                                        .insert(
                                            *id,
                                            SubstrateKind::Note,
                                            &ns_str,
                                            "note.content",
                                            vec![emb.clone()],
                                        )
                                        .await
                                    {
                                        tracing::warn!(note_id = %id, error = %e, "note vector insert failed");
                                        errors_skipped += 1;
                                    }
                                }
                                notes_processed += staged.len() as u64;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to get vector store for model (notes)");
                                errors_skipped += staged.len() as u64;
                            }
                        }
                    }
                    Ok(_) => {
                        tracing::warn!("embedding count mismatch for note batch");
                        errors_skipped += staged.len() as u64;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "note embed_batch failed");
                        errors_skipped += staged.len() as u64;
                    }
                }
            }
        }

        if n < batch_size as usize {
            break;
        }
        note_offset += n as u32;
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;

    // Invalidate Vamana snapshots so the next warm-load triggers a rebuild
    // against the freshly re-embedded vectors.
    if let Err(e) = invalidate_vamana_snapshots(&rt, &ns_str).await {
        tracing::warn!(error = %e, "failed to invalidate Vamana snapshots after reindex");
    }

    let report = ReindexReport {
        entities_processed,
        notes_processed,
        model_used: Some(model_name),
        elapsed_ms,
        errors_skipped,
    };

    print_report(&report, args.human);
    Ok(())
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
        println!(
            "Reindex complete: {} entities, {} notes ({} errors skipped) in {}ms",
            report.entities_processed,
            report.notes_processed,
            report.errors_skipped,
            report.elapsed_ms
        );
        if let Some(ref model) = report.model_used {
            println!("Model: {model}");
        }
    } else {
        let json = serde_json::to_string(report).expect("serialize ReindexReport");
        println!("{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_storage::types::{SqlStatement, SqlValue};

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
}
