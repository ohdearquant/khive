//! Section embedding pass (ADR-051).
//!
//! Embeds `knowledge_sections` into the inline `embedding` column with the
//! default embedder. Vectors are unit-normalised little-endian `f32` so a stored
//! dot product equals cosine similarity. Embed text is breadcrumb-enriched:
//! `atom_name \n heading \n\n content`.

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::util::{now_us, row_str, sql_err, EMBED_BATCH, MAX_EMBED_BYTES};

fn unit_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn f32_to_le_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn truncate_bytes(t: &str) -> String {
    if t.len() <= MAX_EMBED_BYTES {
        return t.to_string();
    }
    let mut end = MAX_EMBED_BYTES;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    t[..end].to_string()
}

/// Embed sections in `token`'s namespace into `knowledge_sections.embedding`.
///
/// With `drop_existing`, every section is re-embedded; otherwise only sections
/// whose `embedding` is currently NULL are filled. Returns `(indexed, skipped)`.
pub(crate) async fn embed_sections(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    drop_existing: bool,
    batch_size: usize,
) -> Result<(usize, usize), RuntimeError> {
    if runtime.default_embedder_name().is_empty() {
        return Ok((0, 0));
    }
    let ns = token.namespace().as_str().to_owned();
    let sql = runtime.sql();
    let page = batch_size.clamp(1, 1000) as i64;

    let mut indexed = 0usize;
    let mut skipped = 0usize;
    let mut offset = 0i64;

    loop {
        // When keeping existing vectors we filter on `embedding IS NULL`; embedded
        // rows leave the set, so the page offset stays 0. A full re-embed has a
        // stable result set, so we paginate with a moving offset.
        let (filter, query_offset) = if drop_existing {
            ("", offset)
        } else {
            (" AND s.embedding IS NULL", 0)
        };
        let query = format!(
            "SELECT s.id AS id, s.heading AS heading, s.content AS content, \
                    a.name AS atom_name \
             FROM knowledge_sections s \
             JOIN knowledge_atoms a ON a.id = s.atom_id \
             WHERE s.namespace = ?1{filter} \
             ORDER BY s.id LIMIT ?2 OFFSET ?3"
        );
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("section index reader", e))?;
        let rows = reader
            .query_all(SqlStatement {
                sql: query,
                params: vec![
                    SqlValue::Text(ns.clone()),
                    SqlValue::Integer(page),
                    SqlValue::Integer(query_offset),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("section index page", e))?;
        let n = rows.len();
        if n == 0 {
            break;
        }

        let mut staged: Vec<(String, String)> = Vec::with_capacity(n);
        for r in &rows {
            let Some(id) = row_str(r, "id") else {
                continue;
            };
            let heading = row_str(r, "heading").unwrap_or_default();
            let content = row_str(r, "content").unwrap_or_default();
            let atom_name = row_str(r, "atom_name").unwrap_or_default();
            let text = format!("{atom_name}\n{heading}\n\n{content}");
            if text.trim().is_empty() {
                skipped += 1;
                continue;
            }
            staged.push((id, text));
        }

        for chunk in staged.chunks(EMBED_BATCH) {
            let texts: Vec<String> = chunk.iter().map(|(_, t)| truncate_bytes(t)).collect();
            let embeddings = match runtime.embed_batch(&texts).await {
                Ok(e) if e.len() == chunk.len() => e,
                _ => {
                    skipped += chunk.len();
                    continue;
                }
            };
            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("section index writer", e))?;
            let now = now_us();
            for ((id, _), mut emb) in chunk.iter().zip(embeddings.into_iter()) {
                unit_normalize(&mut emb);
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE knowledge_sections SET embedding = ?1, updated_at = ?2 \
                              WHERE id = ?3"
                            .into(),
                        params: vec![
                            SqlValue::Blob(f32_to_le_bytes(&emb)),
                            SqlValue::Integer(now),
                            SqlValue::Text(id.clone()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("section embedding update", e))?;
                indexed += 1;
            }
        }

        if n < page as usize {
            break;
        }
        offset += n as i64;
    }

    Ok((indexed, skipped))
}
