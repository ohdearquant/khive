//! Section embedding pass (ADR-051).
//!
//! Embeds `knowledge_sections` into the inline `embedding` column with the
//! default embedder. Vectors are unit-normalised little-endian `f32` so a stored
//! dot product equals cosine similarity. Embed text is breadcrumb-enriched:
//! `atom_name \n heading \n\n content`.

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::util::{now_us, row_str, sql_err, MAX_EMBED_BYTES};

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
/// whose `embedding` is currently NULL are filled. When `atom_id` is `Some`, only
/// sections belonging to that atom are processed (used by `knowledge.edit` for
/// inline re-embed after a write). Returns `(indexed, skipped, failed)`. Genuine
/// skips (blank section text) go to `skipped`; embed errors and vector-count
/// mismatches go to `failed` (fail-closed contract).
pub(crate) async fn embed_sections(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    drop_existing: bool,
    batch_size: usize,
    on_progress: Option<&(dyn Fn(u64, u64) + Send + Sync)>,
    atom_id: Option<&str>,
) -> Result<(usize, usize, usize), RuntimeError> {
    if runtime.default_embedder_name().is_empty() {
        return Ok((0, 0, 0));
    }
    let ns = token.namespace().as_str().to_owned();
    let sql = runtime.sql();
    let page = batch_size.clamp(1, 1000) as i64;

    // Build the atom-scope fragment and its bind parameter once.
    let atom_filter = if atom_id.is_some() {
        " AND atom_id = ?2"
    } else {
        ""
    };

    let total: u64 = {
        let null_filter = if drop_existing {
            ""
        } else {
            " AND embedding IS NULL"
        };
        let mut params = vec![SqlValue::Text(ns.clone())];
        if let Some(id) = atom_id {
            params.push(SqlValue::Text(id.to_owned()));
        }
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("section count reader", e))?;
        let row = reader
            .query_row(SqlStatement {
                sql: format!(
                    "SELECT count(*) AS cnt FROM knowledge_sections \
                     WHERE namespace = ?1{atom_filter}{null_filter}"
                ),
                params,
                label: None,
            })
            .await
            .map_err(|e| sql_err("section count", e))?;
        match row {
            Some(r) => match r.get("cnt") {
                Some(SqlValue::Integer(n)) => *n as u64,
                _ => 0,
            },
            None => 0,
        }
    };

    if let Some(cb) = on_progress {
        cb(0, total);
    }

    let mut indexed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut last_id: Option<String> = None;

    loop {
        // Keyset pagination on the `id` PRIMARY KEY: each page selects rows with
        // `id > last_id` in id order, then advances `last_id` to the page's max id.
        // Cost is O(N) total (one forward walk of the id B-tree) instead of the
        // O(N^2) deep-OFFSET re-scan. Works for both modes: a full re-embed walks
        // every row once; keep-existing walks once and the `embedding IS NULL`
        // filter skips already-embedded rows inline. Advancing past EVERY page row
        // (embedded, skipped, or failed) guarantees each row is attempted at most
        // once and the loop terminates.
        let null_filter = if drop_existing {
            ""
        } else {
            " AND s.embedding IS NULL"
        };
        let mut page_params = vec![SqlValue::Text(ns.clone())];
        let atom_clause = if let Some(id) = atom_id {
            page_params.push(SqlValue::Text(id.to_owned()));
            format!(" AND s.atom_id = ?{}", page_params.len())
        } else {
            String::new()
        };
        let keyset_clause = if let Some(ref last) = last_id {
            page_params.push(SqlValue::Text(last.clone()));
            format!(" AND s.id > ?{}", page_params.len())
        } else {
            String::new()
        };
        page_params.push(SqlValue::Integer(page));
        let limit_pos = page_params.len();
        let query = format!(
            "SELECT s.id AS id, s.heading AS heading, s.content AS content, \
                    a.name AS atom_name \
             FROM knowledge_sections s \
             JOIN knowledge_atoms a ON a.id = s.atom_id \
             WHERE s.namespace = ?1{atom_clause}{null_filter}{keyset_clause} \
             ORDER BY s.id LIMIT ?{limit_pos}"
        );
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("section index reader", e))?;
        let rows = reader
            .query_all(SqlStatement {
                sql: query,
                params: page_params,
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

        // One embed call per page: the page (LIMIT = batch_size) IS the embed
        // batch, so there is no inner re-chunk. `staged` holds the non-empty rows
        // of this page (≤ batch_size).
        if !staged.is_empty() {
            let texts: Vec<String> = staged.iter().map(|(_, t)| truncate_bytes(t)).collect();
            match runtime.embed_document_batch(&texts).await {
                Ok(embeddings) if embeddings.len() == staged.len() => {
                    let mut writer = sql
                        .writer()
                        .await
                        .map_err(|e| sql_err("section index writer", e))?;
                    let now = now_us();
                    for ((id, _), mut emb) in staged.iter().zip(embeddings) {
                        unit_normalize(&mut emb);
                        if let Err(e) = writer
                            .execute(SqlStatement {
                                sql:
                                    "UPDATE knowledge_sections SET embedding = ?1, updated_at = ?2 \
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
                        {
                            tracing::warn!(id = %id, error = %e, "section embedding UPDATE failed; counting as failed");
                            failed += 1;
                        } else {
                            indexed += 1;
                        }
                    }
                }
                Ok(_) => {
                    tracing::warn!(
                        batch = staged.len(),
                        "section embed_batch returned wrong vector count; counting as failed"
                    );
                    failed += staged.len();
                }
                Err(e) => {
                    tracing::warn!(error = %e, batch = staged.len(), "section embed_batch failed; counting as failed");
                    failed += staged.len();
                }
            }
        }

        if let Some(cb) = on_progress {
            cb(indexed as u64, total);
        }

        if n < page as usize {
            break;
        }
        // Advance the cursor past the whole page. Rows are id-ordered, so the last
        // row holds the page's max id; this steps over embedded, skipped, and
        // failed rows alike so none is re-selected and the loop terminates.
        match rows.last().and_then(|r| row_str(r, "id")) {
            Some(id) => last_id = Some(id),
            None => break,
        }
    }

    Ok((indexed, skipped, failed))
}
