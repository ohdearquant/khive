//! Index handler: embed atoms and build/persist the Vamana ANN index.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue, VectorRecord};
use khive_types::SubstrateKind;

use super::schema::{Atom, IndexParams};
use super::util::{atom_embed_text, atom_from_row, deser, sql_err, DEFAULT_EMBED_BATCH};
use super::vamana;
use super::KnowledgeHandlers;

impl KnowledgeHandlers {
    pub(crate) async fn index(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
        on_progress: Option<&(dyn Fn(u64, u64) + Send + Sync)>,
    ) -> Result<Value, RuntimeError> {
        let p: IndexParams = deser(params)?;
        let rebuild_ann = p.rebuild_ann.unwrap_or(false);
        let ns = token.namespace().as_str().to_owned();

        let default_model_name = runtime.default_embedder_name();
        if default_model_name.is_empty() {
            return Ok(
                json!({ "indexed": 0, "skipped": 0, "failed": 0, "total": 0, "reason": "no embedding model configured" }),
            );
        }
        let sql = runtime.sql();
        let batch_size = p.batch_size.unwrap_or(DEFAULT_EMBED_BATCH).clamp(1, 1000);
        // insert_only is accepted for API compatibility but no longer drives a
        // pre-delete loop: SqliteVecStore::insert atomically replaces via its own
        // transacted DELETE+INSERT regardless of this flag.
        let _insert_only = p.insert_only.unwrap_or(false);

        let atoms: Vec<Atom> = if let Some(ref ids) = p.ids {
            let mut out = Vec::with_capacity(ids.len());
            let mut reader = sql.reader().await.map_err(|e| sql_err("index reader", e))?;
            for id_or_slug in ids {
                let row = reader
                    .query_row(SqlStatement {
                        sql: "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND (id = ?2 OR slug = ?2) AND deleted_at IS NULL LIMIT 1".into(),
                        params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id_or_slug.clone())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("index atom lookup", e))?;
                if let Some(r) = row {
                    if let Some(a) = atom_from_row(&r) {
                        out.push(a);
                    }
                }
            }
            out
        } else {
            let mut out = Vec::new();
            let mut offset = 0i64;
            loop {
                let mut reader = sql
                    .reader()
                    .await
                    .map_err(|e| sql_err("index page reader", e))?;
                let rows = reader
                    .query_all(SqlStatement {
                        sql: "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL ORDER BY created_at LIMIT ?2 OFFSET ?3".into(),
                        params: vec![
                            SqlValue::Text(ns.clone()),
                            SqlValue::Integer(batch_size as i64),
                            SqlValue::Integer(offset),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("index page", e))?;
                let n = rows.len();
                out.extend(rows.iter().filter_map(atom_from_row));
                if n < batch_size {
                    break;
                }
                offset += n as i64;
            }
            out
        };

        let total = atoms.len();
        let mut indexed = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        let mut truncation_by_model =
            BTreeMap::<String, khive_runtime::retrieval::EmbeddingTruncationReport>::new();

        if let Some(cb) = on_progress {
            cb(0, total as u64);
        }

        for chunk in atoms.chunks(batch_size) {
            let mut staged: Vec<(uuid::Uuid, String)> = Vec::with_capacity(chunk.len());
            for atom in chunk {
                let text = atom_embed_text(atom);
                if text.trim().is_empty() {
                    skipped += 1;
                    continue;
                }
                staged.push((atom.id, text));
            }
            if staged.is_empty() {
                continue;
            }

            let texts: Vec<String> = staged.iter().map(|(_, text)| text.clone()).collect();

            // Track which atoms in this chunk had their vector persisted, so a
            // failed insert is reported as `failed` rather than silently counted
            // as `indexed`. A failed vector write means recall cannot retrieve
            // that atom — that is a failure, not a success.
            //
            // No pre-delete loop: SqliteVecStore::insert_batch wraps each record's
            // own DELETE+INSERT in a savepoint, so a failed INSERT rolls back only
            // that record's DELETE and the prior vector survives (no-worse-than-
            // stale). A separate pre-delete committed before insert would
            // re-introduce the stranding window.
            // Knowledge retrieval embeds and probes only the default model. Keep
            // atom indexing on that same model so every vector written has a read
            // path; secondary-model fan-out belongs with model-aware retrieval.
            // Await in the dispatch task so usage accounting observes the work.
            let outcome =
                embed_and_insert_default_model(runtime, token, default_model_name, &staged, &texts)
                    .await;
            indexed += outcome.affected as usize;
            failed += outcome.failed as usize;
            truncation_by_model
                .entry(default_model_name.to_string())
                .or_default()
                .merge(outcome.truncation);

            if let Some(cb) = on_progress {
                cb(indexed as u64, total as u64);
            }
        }

        // Any vector write invalidates the existing snapshot — the corpus has changed.
        if indexed > 0 {
            vamana::invalidate_snapshot(runtime, &ns).await;
            vamana::clear_namespace(ann, &ns).await;
        }

        let mut ann_count: Option<usize> = None;
        let mut ann_failed = false;
        let is_full_corpus = p.ids.is_none();
        if rebuild_ann && is_full_corpus && indexed > 0 {
            if let Some(cb) = on_progress {
                cb(total as u64, total as u64);
            }
            let model_name = runtime.default_embedder_name();
            // Capture the namespace's write-generation floor before scanning the
            // corpus, mirroring `ensure_ann_for_model`'s `target_generation` capture
            // (PR #815); `install_if_fresher` then fences this direct
            // rebuild insertion with the same generation check the warm path uses,
            // instead of the old presence-only `insert_ann_if_absent`.
            let build_generation = vamana::current_generation(ann, &ns);
            let key = vamana::AnnKey::new(&ns, model_name);
            let scan_authority = match vamana::prepare_full_corpus_scan(runtime, ann, &key).await {
                Ok(authority) => Some(authority),
                Err(e) => {
                    tracing::error!(error = %e, "failed to establish Vamana full-scan authority");
                    ann_failed = true;
                    None
                }
            };
            // Build from the shared corpus scan (ORDER BY subject_id) so the persisted
            // v2 content_hash matches the warm-path live_content_hash. Building from
            // atom-iteration order persists a hash the warm path always reads as stale.
            if let Some(scan_authority) = scan_authority {
                match vamana::load_and_build_from_vector_store(runtime, token, model_name).await {
                    Ok(Some(bridge)) => {
                        let n = bridge.num_vectors();
                        ann_count = Some(n);
                        let published = vamana::checkpoint_raise_compact_readopt(
                            runtime,
                            ann,
                            &key,
                            bridge,
                            build_generation,
                            scan_authority,
                        )
                        .await;
                        if published {
                            eprintln!("  Vamana ANN built ({n} vectors)");
                        } else {
                            tracing::warn!(
                                "Vamana ANN publication was fenced; retrying on the next rebuild"
                            );
                            ann_failed = true;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(error = %e, "failed to build Vamana ANN index");
                        eprintln!("  Vamana ANN build failed: {e}");
                        ann_failed = true;
                    }
                }
            }
        }

        Ok(json!({
            "indexed": indexed,
            "skipped": skipped,
            "failed": failed,
            "total": total,
            "ann_vectors": ann_count,
            "ann_failed": ann_failed,
            "truncation_by_model": truncation_by_model,
        }))
    }
}

/// One default-model indexing outcome, counted in vectors.
struct ModelIndexOutcome {
    affected: u64,
    failed: u64,
    truncation: khive_runtime::retrieval::EmbeddingTruncationReport,
}

/// Embed one chunk's staged atoms with the default model and batch-insert the
/// resulting vectors. Knowledge search has no model selector or multi-model
/// fusion, so this helper deliberately cannot target a secondary model.
async fn embed_and_insert_default_model(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    model_name: &str,
    staged: &[(uuid::Uuid, String)],
    texts: &[String],
) -> ModelIndexOutcome {
    let outcomes = match runtime
        .embed_document_batch_with_model_outcomes(model_name, texts)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                batch_size = staged.len(),
                error = %e,
                "embed_batch failed; atoms cannot be recalled until reindexed"
            );
            return ModelIndexOutcome {
                affected: 0,
                failed: staged.len() as u64,
                truncation: Default::default(),
            };
        }
    };
    if outcomes.len() != staged.len() {
        tracing::warn!(
            expected = staged.len(),
            got = outcomes.len(),
            "embed_batch returned wrong number of vectors; atoms cannot be recalled until reindexed"
        );
        return ModelIndexOutcome {
            affected: 0,
            failed: staged.len() as u64,
            truncation: Default::default(),
        };
    }

    let mut truncation = khive_runtime::retrieval::EmbeddingTruncationReport::default();
    for outcome in &outcomes {
        truncation.observe(outcome);
    }

    let vectors = match runtime.vectors_for_model(token, model_name) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "knowledge vector store unavailable");
            return ModelIndexOutcome {
                affected: 0,
                failed: staged.len() as u64,
                truncation,
            };
        }
    };

    let ns_str = token.namespace().as_str().to_string();
    let records: Vec<VectorRecord> = staged
        .iter()
        .zip(outcomes.iter())
        .map(|((id, _), outcome)| VectorRecord {
            subject_id: *id,
            kind: SubstrateKind::Entity,
            namespace: ns_str.clone(),
            field: "knowledge.atom".to_string(),
            embedding_model: Some(model_name.to_string()),
            vectors: vec![outcome.vector.clone()],
            updated_at: chrono::Utc::now(),
        })
        .collect();

    match vectors.insert_batch(records).await {
        Ok(summary) => {
            if summary.failed > 0 {
                tracing::warn!(
                    failed = summary.failed,
                    error = %summary.first_error,
                    "knowledge vector batch insert had failures"
                );
            }
            ModelIndexOutcome {
                affected: summary.affected,
                failed: summary.failed,
                truncation,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "knowledge vector store unavailable");
            ModelIndexOutcome {
                affected: 0,
                failed: staged.len() as u64,
                truncation,
            }
        }
    }
}
