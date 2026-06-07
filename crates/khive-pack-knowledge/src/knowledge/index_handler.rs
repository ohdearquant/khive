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
    ) -> Result<Value, RuntimeError> {
        let p: IndexParams = deser(params)?;
        let rebuild_ann = p.rebuild_ann.unwrap_or(false);
        let ns = token.namespace().as_str().to_owned();

        if runtime.default_embedder_name().is_empty() {
            return Ok(
                json!({ "indexed": 0, "skipped": 0, "total": 0, "reason": "no embedding model configured" }),
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

            let embeddings = match runtime.embed_batch(&texts).await {
                Ok(e) => e,
                Err(_) => {
                    skipped += staged.len();
                    continue;
                }
            };
            if embeddings.len() != staged.len() {
                skipped += staged.len();
                continue;
            }

            if let Ok(vectors) = runtime.vectors(token) {
                let ns_str = token.namespace().as_str();
                if !insert_only {
                    for (id, _) in &staged {
                        let _ = vectors.delete(*id).await;
                    }
                }
                for ((id, _), emb) in staged.iter().zip(embeddings.iter()) {
                    let _ = vectors
                        .insert(
                            *id,
                            SubstrateKind::Entity,
                            ns_str,
                            "knowledge.atom",
                            vec![emb.clone()],
                        )
                        .await;
                }
            }

            if rebuild_ann {
                for ((id, _), emb) in staged.iter().zip(embeddings.iter()) {
                    if ann_dim == 0 {
                        ann_dim = emb.len();
                    }
                    if emb.len() == ann_dim {
                        ann_ids.push(*id);
                        ann_vectors.extend_from_slice(emb);
                    }
                }
            }

            indexed += staged.len();
        }

        // Any vector write invalidates the existing snapshot — the corpus has changed.
        if indexed > 0 {
            vamana::invalidate_snapshot(runtime, &ns).await;
            vamana::clear_namespace(ann, &ns).await;
        }

        let mut ann_count: Option<usize> = None;
        let is_full_corpus = p.ids.is_none();
        if rebuild_ann && is_full_corpus && !ann_vectors.is_empty() && ann_dim > 0 {
            match vamana::AnnBridge::build(ann_vectors, ann_dim, ann_ids) {
                Ok(bridge) => {
                    ann_count = Some(bridge.num_vectors());
                    let model_name = runtime.default_embedder_name();
                    if let Some(fp) = vamana::compute_fingerprint(runtime, token, model_name).await
                    {
                        if let Err(e) =
                            vamana::persist_snapshot(runtime, &ns, model_name, &bridge, fp).await
                        {
                            tracing::error!(error = %e, "failed to persist Vamana snapshot");
                        }
                    }
                    let key = vamana::AnnKey::new(&ns, model_name);
                    vamana::insert_ann_if_absent(ann, key, bridge).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to build Vamana ANN index");
                }
            }
        }

        Ok(json!({
            "indexed": indexed,
            "skipped": skipped,
            "total": total,
            "ann_vectors": ann_count,
        }))
    }
}
