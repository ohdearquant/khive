//! Index handler: embed atoms and build/persist the Vamana ANN index.

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::SubstrateKind;

use super::schema::{Atom, IndexParams};
use super::util::{atom_embed_text, atom_from_row, deser, sql_err, EMBED_BATCH, MAX_EMBED_BYTES};
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

        if runtime.default_embedder_name().is_empty() {
            return Ok(
                json!({ "indexed": 0, "skipped": 0, "failed": 0, "total": 0, "reason": "no embedding model configured" }),
            );
        }

        let sql = runtime.sql();
        let batch_size = p.batch_size.unwrap_or(500).clamp(1, 1000);
        let insert_only = p.insert_only.unwrap_or(false);

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

        if let Some(cb) = on_progress {
            cb(0, total as u64);
        }

        let mut ann_vectors: Vec<f32> = Vec::new();
        let mut ann_ids: Vec<uuid::Uuid> = Vec::new();
        let mut ann_dim: usize = 0;

        for chunk in atoms.chunks(EMBED_BATCH) {
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

            let texts: Vec<String> = staged
                .iter()
                .map(|(_, t)| {
                    if t.len() <= MAX_EMBED_BYTES {
                        t.clone()
                    } else {
                        let mut end = MAX_EMBED_BYTES;
                        while !t.is_char_boundary(end) {
                            end -= 1;
                        }
                        t[..end].to_string()
                    }
                })
                .collect();

            let embeddings = match runtime.embed_document_batch(&texts).await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        batch_size = staged.len(),
                        error = %e,
                        "embed_batch failed; atoms cannot be recalled until reindexed"
                    );
                    failed += staged.len();
                    continue;
                }
            };
            if embeddings.len() != staged.len() {
                tracing::warn!(
                    expected = staged.len(),
                    got = embeddings.len(),
                    "embed_batch returned wrong number of vectors; atoms cannot be recalled until reindexed"
                );
                failed += staged.len();
                continue;
            }

            // Track which atoms in this chunk had their vector persisted, so a
            // failed insert is reported as `failed` rather than silently counted
            // as `indexed`. A failed vector write means recall cannot retrieve
            // that atom — that is a failure, not a success.
            let mut chunk_ok: Vec<bool> = vec![true; staged.len()];
            match runtime.vectors(token) {
                Ok(vectors) => {
                    let ns_str = token.namespace().as_str();
                    if !insert_only {
                        for (id, _) in &staged {
                            let _ = vectors.delete(*id).await;
                        }
                    }
                    for (i, ((id, _), emb)) in staged.iter().zip(embeddings.iter()).enumerate() {
                        if let Err(e) = vectors
                            .insert(
                                *id,
                                SubstrateKind::Entity,
                                ns_str,
                                "knowledge.atom",
                                vec![emb.clone()],
                            )
                            .await
                        {
                            tracing::warn!(id = %id, error = %e, "knowledge vector insert failed");
                            chunk_ok[i] = false;
                            failed += 1;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "knowledge vector store unavailable");
                    for ok in chunk_ok.iter_mut() {
                        *ok = false;
                    }
                    failed += staged.len();
                }
            }

            if rebuild_ann {
                for (i, ((id, _), emb)) in staged.iter().zip(embeddings.iter()).enumerate() {
                    if !chunk_ok[i] {
                        continue;
                    }
                    if ann_dim == 0 {
                        ann_dim = emb.len();
                    }
                    if emb.len() == ann_dim {
                        ann_ids.push(*id);
                        ann_vectors.extend_from_slice(emb);
                    }
                }
            }

            indexed += chunk_ok.iter().filter(|ok| **ok).count();

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
        if rebuild_ann && is_full_corpus && !ann_vectors.is_empty() && ann_dim > 0 {
            let n_vecs = ann_ids.len();
            tracing::info!(
                vectors = n_vecs,
                dim = ann_dim,
                "building Vamana ANN index…"
            );
            if let Some(cb) = on_progress {
                cb(total as u64, total as u64);
            }
            eprintln!("\n  building Vamana ANN ({n_vecs} vectors, dim={ann_dim})…");
            match vamana::AnnBridge::build(ann_vectors, ann_dim, ann_ids) {
                Ok(bridge) => {
                    ann_count = Some(bridge.num_vectors());
                    let model_name = runtime.default_embedder_name();
                    match vamana::compute_fingerprint(runtime, token, model_name).await {
                        Some(fp) => {
                            if let Err(e) =
                                vamana::persist_snapshot(runtime, &ns, model_name, &bridge, fp)
                                    .await
                            {
                                tracing::error!(error = %e, "failed to persist Vamana snapshot");
                                ann_failed = true;
                            }
                        }
                        None => {
                            tracing::warn!(
                                "failed to compute corpus fingerprint; Vamana snapshot will not be persisted"
                            );
                            ann_failed = true;
                        }
                    }
                    let n = bridge.num_vectors();
                    let key = vamana::AnnKey::new(&ns, model_name);
                    vamana::insert_ann_if_absent(ann, key, bridge).await;
                    eprintln!("  Vamana ANN built ({n} vectors)");
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to build Vamana ANN index");
                    eprintln!("  Vamana ANN build failed: {e}");
                    ann_failed = true;
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
        }))
    }
}
