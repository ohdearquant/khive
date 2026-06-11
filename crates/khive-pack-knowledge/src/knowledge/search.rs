//! Search, suggest, and compose handlers.
//!
//! TF-IDF scoring primitives live in `super::scoring`; this module owns the
//! FTS/ANN pipeline, reranking, hydration, and handler dispatch.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_score::DeterministicScore;
use khive_storage::types::{SqlStatement, SqlValue};

use super::matching;
use super::schema::{Atom, ComposeParams, Domain, SearchParams, SuggestParams};
use super::scoring::{
    compute_idf, exact_name_bonus, expand_terms, load_candidates_from_atoms, score_candidate,
    Candidate, Weights,
};
use super::util::{
    atom_embed_text, atom_from_row, deser, domain_from_row, explicitly_requested_status, is_stop,
    row_bool, row_str, sql_err, status_multiplier, status_sql_clause, status_values,
    CANDIDATE_POOL, MIN_TERM_LEN,
};
use super::vamana;
use super::KnowledgeHandlers;

// ─── scored hit (internal) ────────────────────────────────────────────────────

struct ScoredHit {
    id: String,
    slug: String,
    name: String,
    content: Option<String>,
    tags: Option<String>,
    finalized: bool,
    is_domain: bool,
    status: Option<String>,
    score: f32,
}

// ─── ANN fusion (symmetric RRF) ─────────────────────────────────────────────

const RRF_K: usize = 60;

fn normalize_rrf_score(raw: f32, source_count: usize, k: usize) -> f32 {
    if source_count == 0 {
        return 0.0;
    }
    let theoretical_max = source_count as f32 / (k as f32 + 1.0);
    (raw / theoretical_max).clamp(0.0, 1.0)
}

fn fuse_ann_hits(fts_hits: &mut Vec<ScoredHit>, ann_hits: &[(Uuid, f32)], min_score: f32) {
    let drained: Vec<ScoredHit> = std::mem::take(fts_hits);

    let fts_source: Vec<(String, DeterministicScore)> = drained
        .iter()
        .map(|hit| (hit.id.clone(), DeterministicScore::from_f32(hit.score)))
        .collect();
    let mut by_id: HashMap<String, ScoredHit> = drained
        .into_iter()
        .map(|hit| (hit.id.clone(), hit))
        .collect();
    let ann_source: Vec<(String, DeterministicScore)> = ann_hits
        .iter()
        .map(|(uuid, score)| (uuid.to_string(), DeterministicScore::from_f32(*score)))
        .collect();

    let source_count = usize::from(!fts_source.is_empty()) + usize::from(!ann_source.is_empty());
    let fused = khive_fusion::reciprocal_rank_fusion(vec![fts_source, ann_source], RRF_K);

    for (id, fused_score) in fused {
        let raw_score = fused_score.to_f64() as f32;
        let score = normalize_rrf_score(raw_score, source_count, RRF_K);
        if score < min_score {
            continue;
        }

        if let Some(mut hit) = by_id.remove(&id) {
            hit.score = score;
            fts_hits.push(hit);
        } else {
            fts_hits.push(ScoredHit {
                id,
                slug: String::new(),
                name: String::new(),
                content: None,
                tags: None,
                finalized: false,
                is_domain: false,
                status: None,
                score,
            });
        }
    }
}

// ─── status filtering (post-hydration) ───────────────────────────────────────

/// Remove hits whose `status` is in `exclude_statuses` after hydration.
///
/// This is the shared gate for both the SQL path (where exclusion is enforced
/// in the query) and the ANN-only path (where hydration happens after fusion
/// and the SQL predicate was never applied to the ANN-sourced IDs).
fn filter_by_excluded_statuses(hits: &mut Vec<ScoredHit>, exclude_statuses: &[&str]) {
    if exclude_statuses.is_empty() {
        return;
    }
    hits.retain(|hit| {
        let status = hit.status.as_deref().unwrap_or("");
        !exclude_statuses.contains(&status)
    });
}

// ─── status scoring ───────────────────────────────────────────────────────────

fn apply_status_multipliers(hits: &mut Vec<ScoredHit>, include_deprecated: bool) {
    hits.retain_mut(|hit| {
        let multiplier = status_multiplier(hit.status.as_deref());
        // Squash raw score to (0,1) via monotonic s/(s+1) before applying the status
        // multiplier so that TF-IDF scores > 1 don't saturate ranking. RRF-normalized
        // scores (already ≤ 1) are squashed at most to 0.5, preserving relative order.
        hit.score = (hit.score / (hit.score + 1.0) * multiplier).clamp(0.0, 1.0);
        include_deprecated || multiplier > 0.0
    });
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.slug.cmp(&b.slug))
    });
}

// ─── FTS5 phrase quoting ─────────────────────────────────────────────────────

fn quote_fts5_phrase(raw_query: &str) -> String {
    let escaped = raw_query.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

// ─── FTS5 candidate pool fetch ────────────────────────────────────────────────

async fn fetch_fts_candidates(
    runtime: &KhiveRuntime,
    ns: &str,
    raw_query: &str,
    type_filter: Option<&str>,
    statuses: &[String],
    exclude_statuses: &[&str],
    fetch_limit: usize,
) -> Result<Vec<Atom>, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("search fts reader", e))?;

    let match_expr = quote_fts5_phrase(raw_query);
    let fts_rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id FROM fts_knowledge WHERE fts_knowledge MATCH ?1 AND namespace = ?2 LIMIT ?3".into(),
            params: vec![
                SqlValue::Text(match_expr),
                SqlValue::Text(ns.to_owned()),
                SqlValue::Integer(fetch_limit as i64),
            ],
            label: None,
        })
        .await
        .map_err(|e| sql_err("search fts query", e))?;

    if fts_rows.is_empty() {
        // FTS returned nothing — fall back to full scan (small corpora) capped at CANDIDATE_POOL.
        let (status_clause, status_params) = status_sql_clause(statuses, exclude_statuses, 3);
        let sql_str = format!(
            "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL{} ORDER BY created_at DESC LIMIT ?2",
            status_clause
        );
        let mut params = vec![
            SqlValue::Text(ns.to_owned()),
            SqlValue::Integer(CANDIDATE_POOL as i64),
        ];
        params.extend(status_params);

        let rows = reader
            .query_all(SqlStatement {
                sql: sql_str,
                params,
                label: None,
            })
            .await
            .map_err(|e| sql_err("search full scan", e))?;

        let mut atoms: Vec<Atom> = rows.iter().filter_map(atom_from_row).collect();
        if let Some(filt) = type_filter {
            let want_domain = filt == "domain";
            atoms.retain(|a| {
                let tags_arr: Vec<String> = serde_json::from_str(&a.tags).unwrap_or_default();
                let is_domain = tags_arr.iter().any(|t| t == "type:domain");
                if want_domain {
                    is_domain
                } else {
                    !is_domain
                }
            });
        }
        return Ok(atoms);
    }

    let ids: Vec<String> = fts_rows.iter().filter_map(|r| row_str(r, "id")).collect();
    let placeholders: String = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");

    let (status_clause, status_params) =
        status_sql_clause(statuses, exclude_statuses, ids.len() + 2);
    let mut params: Vec<SqlValue> = vec![SqlValue::Text(ns.to_owned())];
    params.extend(ids.iter().map(|id| SqlValue::Text(id.clone())));
    params.extend(status_params);

    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT * FROM knowledge_atoms WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL{status_clause}"
            ),
            params,
            label: None,
        })
        .await
        .map_err(|e| sql_err("search load atoms", e))?;

    Ok(rows.iter().filter_map(atom_from_row).collect())
}

// ─── search context ───────────────────────────────────────────────────────────

struct SearchCtx<'a> {
    runtime: &'a KhiveRuntime,
    ns: &'a str,
    role: Option<&'a str>,
    type_filter: Option<&'a str>,
    min_score: f32,
    w: &'a Weights,
    fetch_limit: usize,
    statuses: &'a [String],
    exclude_statuses: &'a [&'a str],
}

// ─── core single-pass search ──────────────────────────────────────────────────

async fn search_core(ctx: &SearchCtx<'_>, query: &str) -> Result<Vec<ScoredHit>, RuntimeError> {
    let runtime = ctx.runtime;
    let ns = ctx.ns;
    let role = ctx.role;
    let type_filter = ctx.type_filter;
    let min_score = ctx.min_score;
    let w = ctx.w;
    let fetch_limit = ctx.fetch_limit;
    let raw_query = query.trim().to_string();
    if raw_query.is_empty() {
        return Ok(Vec::new());
    }

    let scored_query = match role {
        Some(r) if !r.trim().is_empty() => format!("{} {}", r.trim(), raw_query),
        _ => raw_query.clone(),
    };

    let (terms, original_terms, query_order, expanded) = {
        let raw_tokens: Vec<String> = matching::tokenize_field(&scored_query)
            .into_iter()
            .filter(|w| w.len() >= MIN_TERM_LEN && !is_stop(w))
            .collect();
        let mut seen = HashSet::new();
        let qo: Vec<String> = raw_tokens
            .iter()
            .filter(|w| seen.insert(w.as_str()))
            .cloned()
            .collect();
        let mut t = raw_tokens;
        t.sort();
        t.dedup();
        let originals = t.clone();
        let exp = expand_terms(&mut t);
        (t, originals, qo, exp)
    };
    // When all query tokens are shorter than MIN_TERM_LEN (e.g. "RAG", "GQA", "LoRA"),
    // fall through to exact-name-bonus-only scoring rather than returning early.
    let terms_only_exact = terms.is_empty();

    let atoms = fetch_fts_candidates(
        runtime,
        ns,
        &raw_query,
        type_filter,
        ctx.statuses,
        ctx.exclude_statuses,
        CANDIDATE_POOL,
    )
    .await?;
    if atoms.is_empty() {
        return Ok(Vec::new());
    }

    let candidates = load_candidates_from_atoms(&atoms, type_filter);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let idf = compute_idf(&candidates, &terms, &expanded, w.expand_discount);
    let mut scored: Vec<(f32, &Candidate)> = candidates
        .iter()
        .filter_map(|cand| {
            let base = if terms_only_exact {
                exact_name_bonus(&cand.name_raw, &raw_query, w.w_exact_name)
            } else {
                score_candidate(
                    cand,
                    &terms,
                    &original_terms,
                    &query_order,
                    &idf,
                    &raw_query,
                    w,
                )
            };
            if base >= min_score {
                Some((base, cand))
            } else {
                None
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.slug.cmp(&b.1.slug))
    });
    scored.truncate(fetch_limit);

    Ok(scored
        .into_iter()
        .map(|(score, cand)| ScoredHit {
            id: cand.id.clone(),
            slug: cand.slug.clone(),
            name: cand.name_raw.clone(),
            content: cand.content_raw.clone(),
            tags: cand.tags_raw.clone(),
            status: cand.status_raw.clone(),
            finalized: cand.finalized,
            is_domain: cand.is_domain,
            score,
        })
        .collect())
}

// ─── decomposed search ───────────────────────────────────────────────────────

async fn search_decomposed(
    ctx: &SearchCtx<'_>,
    query: &str,
    intersection_bonus: f32,
) -> Result<Vec<ScoredHit>, RuntimeError> {
    let non_stop: Vec<&str> = query
        .split_whitespace()
        .filter(|w| w.len() >= MIN_TERM_LEN && !is_stop(&w.to_lowercase()))
        .collect();

    let mid = non_stop.len() / 2;
    let sub_q1: String = non_stop[..mid].join(" ");
    let sub_q2: String = non_stop[mid..].join(" ");
    let sub_limit = ctx.fetch_limit.min(50);

    let full = search_core(ctx, query).await?;
    let sub_ctx1 = SearchCtx {
        runtime: ctx.runtime,
        ns: ctx.ns,
        role: None,
        type_filter: ctx.type_filter,
        min_score: 0.0,
        w: ctx.w,
        fetch_limit: sub_limit,
        statuses: ctx.statuses,
        exclude_statuses: ctx.exclude_statuses,
    };
    let s1 = search_core(&sub_ctx1, &sub_q1).await?;
    let s2 = search_core(&sub_ctx1, &sub_q2).await?;

    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut data: HashMap<String, ScoredHit> = HashMap::new();

    for hit in full {
        scores.insert(hit.id.clone(), hit.score);
        data.insert(hit.id.clone(), hit);
    }

    let mut sub_counts: HashMap<String, u32> = HashMap::new();
    for hits in [s1, s2] {
        let mut seen: HashSet<String> = HashSet::new();
        for hit in hits {
            if !seen.insert(hit.id.clone()) {
                continue;
            }
            *sub_counts.entry(hit.id.clone()).or_default() += 1;
            if !data.contains_key(&hit.id) {
                scores.insert(hit.id.clone(), hit.score * 0.3);
                data.insert(hit.id.clone(), hit);
            }
        }
    }

    for (id, count) in &sub_counts {
        if *count >= 2 {
            if let Some(s) = scores.get_mut(id) {
                *s *= 1.0 + intersection_bonus * (*count as f32 - 1.0);
            }
        }
    }

    let mut ranked: Vec<ScoredHit> = data
        .into_values()
        .map(|mut h| {
            if let Some(&s) = scores.get(&h.id) {
                h.score = s;
            }
            h
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    ranked.truncate(ctx.fetch_limit);
    Ok(ranked)
}

// ─── embedding rerank ────────────────────────────────────────────────────────

async fn embed_cosine_scores(
    runtime: &KhiveRuntime,
    query: &str,
    candidate_texts: &[String],
) -> Option<Vec<f32>> {
    if runtime.default_embedder_name().is_empty() || candidate_texts.is_empty() {
        return None;
    }
    let mut texts = Vec::with_capacity(candidate_texts.len() + 1);
    texts.push(query.to_string());
    texts.extend_from_slice(candidate_texts);
    let embeddings = runtime.embed_batch(&texts).await.ok()?;
    if embeddings.len() != texts.len() {
        return None;
    }
    let query_emb = &embeddings[0];
    Some(
        embeddings[1..]
            .iter()
            .map(|emb| cosine_similarity(query_emb, emb))
            .collect(),
    )
}

async fn rerank_with_embeddings(
    runtime: &KhiveRuntime,
    query: &str,
    hits: &mut [ScoredHit],
    alpha: f32,
) -> Result<(), RuntimeError> {
    if hits.is_empty() {
        return Ok(());
    }
    let texts: Vec<String> = hits
        .iter()
        .map(|h| format!("{} {}", h.name, h.content.as_deref().unwrap_or("")))
        .collect();
    if let Some(cosines) = embed_cosine_scores(runtime, query, &texts).await {
        let max_tfidf = hits
            .iter()
            .map(|h| h.score)
            .fold(0.0f32, f32::max)
            .max(1e-6);
        for (hit, cos) in hits.iter_mut().zip(cosines.iter()) {
            let norm_tfidf = hit.score / max_tfidf;
            hit.score = alpha * norm_tfidf + (1.0 - alpha) * cos.max(0.0);
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.slug.cmp(&b.slug))
        });
    }
    Ok(())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-8 {
        0.0
    } else {
        dot / denom
    }
}

// ─── hit hydration ────────────────────────────────────────────────────────────

async fn hydrate_empty_hits(runtime: &KhiveRuntime, ns: &str, hits: &mut Vec<ScoredHit>) {
    let ids: Vec<String> = hits
        .iter()
        .filter(|hit| hit.slug.is_empty())
        .map(|hit| hit.id.clone())
        .collect();
    if ids.is_empty() {
        return;
    }

    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(r) => r,
        Err(_) => return,
    };

    let placeholders = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(ids.iter().cloned().map(SqlValue::Text));

    let atom_rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT id, slug, name, content, tags, finalized, status FROM knowledge_atoms WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL"
            ),
            params,
            label: None,
        })
        .await
        .unwrap_or_default();

    let mut atom_rows_by_id: HashMap<String, khive_storage::types::SqlRow> = HashMap::new();
    for row in atom_rows {
        if let Some(id) = row_str(&row, "id") {
            atom_rows_by_id.insert(id, row);
        }
    }

    for hit in hits.iter_mut().filter(|hit| hit.slug.is_empty()) {
        if let Some(row) = atom_rows_by_id.get(&hit.id) {
            hit.slug = row_str(row, "slug").unwrap_or_default();
            hit.name = row_str(row, "name").unwrap_or_default();
            hit.content = row_str(row, "content");
            hit.tags = row_str(row, "tags");
            hit.finalized = row_bool(row, "finalized");
            hit.status = row_str(row, "status");
            let tags_arr: Vec<String> = hit
                .tags
                .as_deref()
                .and_then(|tags| serde_json::from_str(tags).ok())
                .unwrap_or_default();
            hit.is_domain = tags_arr.iter().any(|t| t == "type:domain");
        }
    }

    let missing_ids: Vec<String> = hits
        .iter()
        .filter(|hit| hit.slug.is_empty())
        .map(|hit| hit.id.clone())
        .collect();
    if missing_ids.is_empty() {
        return;
    }

    let placeholders = missing_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(missing_ids.iter().cloned().map(SqlValue::Text));

    let domain_rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT id, slug, name, description, tags FROM knowledge_domains WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL"
            ),
            params,
            label: None,
        })
        .await
        .unwrap_or_default();

    let mut domain_rows_by_id: HashMap<String, khive_storage::types::SqlRow> = HashMap::new();
    for row in domain_rows {
        if let Some(id) = row_str(&row, "id") {
            domain_rows_by_id.insert(id, row);
        }
    }

    for hit in hits.iter_mut().filter(|hit| hit.slug.is_empty()) {
        if let Some(row) = domain_rows_by_id.get(&hit.id) {
            hit.slug = row_str(row, "slug").unwrap_or_default();
            hit.name = row_str(row, "name").unwrap_or_default();
            hit.content = row_str(row, "description");
            hit.tags = row_str(row, "tags");
            hit.finalized = false;
            hit.is_domain = true;
        }
    }

    hits.retain(|hit| !hit.slug.is_empty());
}

// ─── compose helpers ──────────────────────────────────────────────────────────

struct ScoredTextItem {
    id: String,
    slug: String,
    name: String,
    text: String,
    score: f32,
}

async fn load_domain_by_id_or_slug(
    runtime: &KhiveRuntime,
    ns: &str,
    id_or_slug: &str,
) -> Result<Domain, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("compose domain reader", e))?;
    let id = id_or_slug.trim().to_string();
    let row = if id.parse::<Uuid>().is_ok() {
        reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_domains WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose domain by id", e))?
    } else {
        let by_slug = reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_domains WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose domain by slug", e))?;
        if by_slug.is_some() {
            by_slug
        } else {
            let is_hex = id.len() >= 8
                && id.len() <= 36
                && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            if is_hex {
                let rows = reader
                    .query_all(SqlStatement {
                        sql: "SELECT * FROM knowledge_domains WHERE id LIKE ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 2".into(),
                        params: vec![
                            SqlValue::Text(format!("{id}%")),
                            SqlValue::Text(ns.to_owned()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("compose domain by prefix", e))?;
                if rows.len() > 1 {
                    return Err(RuntimeError::InvalidInput(format!(
                        "ambiguous domain prefix {id:?} matches multiple domains"
                    )));
                }
                rows.into_iter().next()
            } else {
                None
            }
        }
    };
    row.and_then(|r| domain_from_row(&r))
        .ok_or_else(|| RuntimeError::NotFound(format!("domain not found: {id:?}")))
}

async fn load_atom_by_id_or_slug(
    runtime: &KhiveRuntime,
    ns: &str,
    id_or_slug: &str,
) -> Result<Atom, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("compose atom reader", e))?;
    let id = id_or_slug.trim().to_string();
    let row = if id.parse::<Uuid>().is_ok() {
        reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose atom by id", e))?
    } else {
        let by_slug = reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_atoms WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose atom by slug", e))?;
        if by_slug.is_some() {
            by_slug
        } else {
            let is_hex = id.len() >= 8
                && id.len() <= 36
                && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            if is_hex {
                let rows = reader
                    .query_all(SqlStatement {
                        sql: "SELECT * FROM knowledge_atoms WHERE id LIKE ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 2".into(),
                        params: vec![
                            SqlValue::Text(format!("{id}%")),
                            SqlValue::Text(ns.to_owned()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("compose atom by prefix", e))?;
                if rows.len() > 1 {
                    return Err(RuntimeError::InvalidInput(format!(
                        "ambiguous atom prefix {id:?} matches multiple atoms"
                    )));
                }
                rows.into_iter().next()
            } else {
                None
            }
        }
    };
    row.and_then(|r| atom_from_row(&r))
        .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {id:?}")))
}

fn parse_domain_members(domain: &Domain) -> Result<Vec<String>, RuntimeError> {
    if domain.members.is_empty() || domain.members == "[]" {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<String>>(&domain.members).map_err(|e| {
        RuntimeError::Internal(format!(
            "domain {:?} has invalid members JSON: {e}",
            domain.slug
        ))
    })
}

async fn rerank_text_items(
    runtime: &KhiveRuntime,
    query: &str,
    items: &mut [ScoredTextItem],
) -> Result<(), RuntimeError> {
    if items.is_empty() {
        return Ok(());
    }
    let texts: Vec<String> = items.iter().map(|item| item.text.clone()).collect();
    if let Some(cosines) = embed_cosine_scores(runtime, query, &texts).await {
        for (item, cos) in items.iter_mut().zip(cosines.iter()) {
            item.score = cos.max(0.0);
        }
        items.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.slug.cmp(&b.slug))
        });
    }
    Ok(())
}

fn format_section_compose_markdown(
    query: &str,
    domains: &[Domain],
    atoms: &[Atom],
    sections: &[super::compose::ComposeSectionResult],
) -> String {
    let mut out = String::from("# Knowledge Briefing\n\n");
    out.push_str(&format!("Query: {query}\n"));

    let mut by_atom: HashMap<&str, Vec<&super::compose::ComposeSectionResult>> = HashMap::new();
    for s in sections {
        by_atom.entry(s.atom_id.as_str()).or_default().push(s);
    }

    for atom in atoms {
        let atom_id = atom.id.to_string();
        if let Some(secs) = by_atom.get(atom_id.as_str()) {
            out.push_str(&format!("\n## {}\n\n", atom.name));
            out.push_str(&format!("Source: {}\n", atom.slug));
            for s in secs {
                out.push_str(&format!("\n### {} (score: {:.4})\n\n", s.heading, s.score));
                if !s.content.is_empty() {
                    out.push_str(&s.content);
                    out.push('\n');
                }
            }
        }
    }
    if !domains.is_empty() {
        out.push_str("\n---\n\nDomains: ");
        let names: Vec<&str> = domains.iter().map(|d| d.name.as_str()).collect();
        out.push_str(&names.join(", "));
        out.push('\n');
    }
    out
}

fn format_compose_markdown(query: &str, domains: &[Domain], atoms: &[(&Atom, f32)]) -> String {
    let mut out = String::from("# Knowledge Briefing\n\n");
    out.push_str(&format!("Query: {query}\n"));
    for (atom, score) in atoms {
        out.push_str(&format!("\n## {}\n\n", atom.name));
        out.push_str(&format!("Source: {}\n", atom.slug));
        out.push_str(&format!("Score: {:.4}\n", score));
        if !atom.content.is_empty() {
            out.push('\n');
            out.push_str(&atom.content);
            out.push('\n');
        }
    }
    if !domains.is_empty() {
        out.push_str("\n---\n\nDomains: ");
        let names: Vec<&str> = domains.iter().map(|d| d.name.as_str()).collect();
        out.push_str(&names.join(", "));
        out.push('\n');
    }
    out
}

// ─── handler impls ────────────────────────────────────────────────────────────

impl KnowledgeHandlers {
    pub(crate) async fn search(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: SearchParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }

        if let Some(ms) = p.min_score {
            if !ms.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "min_score must be a finite number".into(),
                ));
            }
        }
        if let Some(ib) = p.intersection_bonus {
            if !ib.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "intersection_bonus must be a finite number".into(),
                ));
            }
        }
        if let Some(ra) = p.rerank_alpha {
            if !ra.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "rerank_alpha must be a finite number".into(),
                ));
            }
        }
        if let Some(ref w) = p.weights {
            let pairs: &[(&str, Option<f64>)] = &[
                ("w_exact_name", w.w_exact_name),
                ("w_name", w.w_name),
                ("w_tags", w.w_tags),
                ("w_content", w.w_content),
                ("expand_discount", w.expand_discount),
                ("coverage_alpha", w.coverage_alpha),
                ("w_bigram", w.w_bigram),
            ];
            for (name, val) in pairs {
                if let Some(v) = val {
                    if !v.is_finite() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "weights.{name} must be a finite number"
                        )));
                    }
                }
            }
        }

        let limit = p.limit.unwrap_or(10).clamp(1, 100);
        let min_score = p.min_score.unwrap_or(0.0) as f32;
        let w = Weights::from_opts(&p);
        let type_filter = p.kind.as_deref();
        let do_decompose = p.decompose.unwrap_or(false);
        let decompose_threshold = p.decompose_threshold.unwrap_or(4);
        let intersection_bonus = p.intersection_bonus.unwrap_or(0.25) as f32;
        let requested_rerank = p.rerank.unwrap_or(true);
        let do_rerank = requested_rerank && !runtime.default_embedder_name().is_empty();
        let rerank_alpha = p.rerank_alpha.unwrap_or(0.7) as f32;
        let fetch_limit = if do_rerank { limit * 3 } else { limit }.min(100);

        let non_stop_count = raw_query
            .split_whitespace()
            .filter(|w| w.len() >= MIN_TERM_LEN && !is_stop(&w.to_lowercase()))
            .count();

        let ns = token.namespace().as_str().to_owned();
        let requested_statuses = status_values(p.status.as_ref());
        let include_deprecated = explicitly_requested_status(&requested_statuses, "deprecated");

        // Normalize exclude_status once: trim whitespace, treat blank as absent.
        // This single normalized value feeds both the SQL predicate (via SearchCtx)
        // and the ANN post-hydration filter, ensuring both result sources see the
        // identical exclusion set regardless of how the caller formatted the value.
        let exclude_status_normalized: Option<&str> = p
            .exclude_status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Precedence (highest to lowest, matches ADR-047 §Status filtering):
        //   1. explicit status=  → no exclusion; SQL handles the allowlist
        //   2. no status=, explicit exclude_status= (non-blank) → use that exclusion
        //   3. no status=, include_drafts=true → exclude only deprecated
        //   4. default (no status params / blank exclude_status) → exclude draft and deprecated
        let exclude_statuses_buf: Vec<&str> = if !requested_statuses.is_empty() {
            // Caller specified exact status; no exclusion needed — SQL allowlist wins.
            vec![]
        } else if let Some(ex) = exclude_status_normalized {
            vec![ex]
        } else {
            let include_drafts = p.include_drafts.unwrap_or(false);
            if include_drafts {
                vec!["deprecated"]
            } else {
                vec!["draft", "deprecated"]
            }
        };

        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter,
            min_score,
            w: &w,
            fetch_limit,
            statuses: &requested_statuses,
            exclude_statuses: &exclude_statuses_buf,
        };

        let mut hits = if do_decompose && non_stop_count >= decompose_threshold {
            search_decomposed(&ctx, &raw_query, intersection_bonus).await?
        } else {
            search_core(&ctx, &raw_query).await?
        };

        // Trigger background warm — never block search on the ANN rebuild.
        vamana::ensure_ann_background(runtime, token, ann);

        if let Ok(query_emb) = runtime.embed_query(&raw_query).await {
            let ann_k = fetch_limit.max(20);
            let key = vamana::AnnKey::new(&ns, runtime.default_embedder_name());
            if let Some(ann_hits) = vamana::search_loaded(ann, &key, &query_emb, ann_k).await {
                if !ann_hits.is_empty() {
                    fuse_ann_hits(&mut hits, &ann_hits, min_score);
                    hydrate_empty_hits(runtime, &ns, &mut hits).await;
                    // ANN-sourced hits bypass the SQL status predicate; apply the
                    // same exclusion policy here so all result sources are consistent.
                    filter_by_excluded_statuses(&mut hits, &exclude_statuses_buf);
                }
            }
        }

        if do_rerank && !hits.is_empty() {
            rerank_with_embeddings(runtime, &raw_query, &mut hits, rerank_alpha).await?;
        }

        apply_status_multipliers(&mut hits, include_deprecated);
        hits.truncate(limit);

        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "id": h.id,
                    "slug": h.slug,
                    "name": h.name,
                    "content": h.content,
                    "tags": h.tags,
                    "status": h.status,
                    "finalized": h.finalized,
                    "kind": if h.is_domain { "domain" } else { "atom" },
                    "score": h.score,
                })
            })
            .collect();
        let count = results.len();

        Ok(json!({ "results": results, "total": count }))
    }

    pub(crate) async fn suggest(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: SuggestParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }
        let word_count = raw_query.split_whitespace().count();
        if word_count < 5 {
            return Err(RuntimeError::InvalidInput(format!(
                "suggest query must be at least 5 words for meaningful domain matching \
                 (got {word_count}). Use knowledge.search for short keyword queries."
            )));
        }
        let limit = p.limit.unwrap_or(8).clamp(1, 100);
        let ns = token.namespace().as_str().to_owned();

        // Exclude draft and deprecated domain atoms by default — same quality
        // default as knowledge.search.  Draft domain atoms are incomplete and
        // should not drive auto-compose or agent orientation.
        const SUGGEST_EXCLUDE: &[&str] = &["draft", "deprecated"];

        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter: Some("domain"),
            min_score: 0.0,
            w: &Weights::default(),
            fetch_limit: limit * 3,
            statuses: &[],
            exclude_statuses: SUGGEST_EXCLUDE,
        };

        let mut hits = search_core(&ctx, &raw_query).await?;

        vamana::ensure_ann_background(runtime, token, ann);
        if let Ok(query_emb) = runtime.embed_query(&raw_query).await {
            let ann_k = (limit * 3).max(20);
            let key = vamana::AnnKey::new(&ns, runtime.default_embedder_name());
            if let Some(ann_hits) = vamana::search_loaded(ann, &key, &query_emb, ann_k).await {
                if !ann_hits.is_empty() {
                    fuse_ann_hits(&mut hits, &ann_hits, 0.0);
                    hydrate_empty_hits(runtime, &ns, &mut hits).await;
                    // Apply the same status exclusion to ANN-sourced domain hits.
                    filter_by_excluded_statuses(&mut hits, SUGGEST_EXCLUDE);
                }
            }
        }

        rerank_with_embeddings(runtime, &raw_query, &mut hits, 0.7).await?;

        hits.retain(|h| h.is_domain);
        hits.truncate(limit);

        let results: Vec<Value> = hits
            .iter()
            .map(|h| json!({ "id": h.id, "name": h.name, "score": h.score }))
            .collect();
        let count = results.len();

        Ok(json!({ "results": results, "total": count }))
    }

    pub(crate) async fn compose(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: ComposeParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }

        let mut domain_ids: Vec<String> = p
            .domain_ids
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let atom_ids: Vec<String> = p
            .atom_ids
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();

        let is_auto = domain_ids.is_empty() && atom_ids.is_empty();
        if is_auto {
            let word_count = raw_query.split_whitespace().count();
            if word_count < 10 {
                return Err(RuntimeError::InvalidInput(format!(
                    "auto-compose query must be at least 10 words for effective domain selection \
                     (got {word_count}). Provide explicit domain_ids/atom_ids for shorter queries."
                )));
            }
        }

        if is_auto {
            let auto_limit = p.auto_limit.unwrap_or(5).clamp(1, 20);
            let suggest_result = match Self::suggest(
                runtime,
                token,
                json!({ "query": &raw_query, "limit": auto_limit }),
                ann,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "auto-compose: internal suggest failed, returning empty");
                    return Ok(json!({
                        "status": "ok",
                        "data": {
                            "query": raw_query,
                            "markdown": "# Knowledge Briefing\n\nDomain suggestion unavailable.",
                            "domains": [],
                            "atoms": [],
                            "count": 0,
                            "suggest_error": e.to_string(),
                        },
                    }));
                }
            };
            if let Some(results) = suggest_result.get("results").and_then(|v| v.as_array()) {
                for r in results {
                    if let Some(id) = r.get("id").and_then(|v| v.as_str()) {
                        domain_ids.push(id.to_string());
                    }
                }
            }
            if domain_ids.is_empty() {
                return Ok(json!({
                    "status": "ok",
                    "data": {
                        "query": raw_query,
                        "markdown": "# Knowledge Briefing\n\nNo matching domains found for auto-suggest.",
                        "domains": [],
                        "atoms": [],
                        "count": 0,
                    },
                }));
            }
        }

        let ns = token.namespace().as_str().to_owned();

        let mut resolved_domains: Vec<Domain> = Vec::new();
        let mut member_slugs: Vec<String> = Vec::new();

        for id in &domain_ids {
            let domain = load_domain_by_id_or_slug(runtime, &ns, id).await?;
            let members = parse_domain_members(&domain)?;
            member_slugs.extend(members);
            resolved_domains.push(domain);
        }

        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut ordered_atoms: Vec<Atom> = Vec::new();

        for slug in &member_slugs {
            let atom = load_atom_by_id_or_slug(runtime, &ns, slug).await?;
            if seen_ids.insert(atom.id.to_string()) {
                ordered_atoms.push(atom);
            }
        }
        for id in &atom_ids {
            let atom = load_atom_by_id_or_slug(runtime, &ns, id).await?;
            if seen_ids.insert(atom.id.to_string()) {
                ordered_atoms.push(atom);
            }
        }

        // Auto-compose inherits the same quality default as knowledge.search and
        // knowledge.suggest: draft and deprecated atoms are excluded unless the caller
        // explicitly provided atom_ids (which is an opt-in to whatever those IDs hold).
        if is_auto {
            const COMPOSE_EXCLUDE: &[&str] = &["draft", "deprecated"];
            ordered_atoms.retain(|a| {
                let status = a.status.as_deref().unwrap_or("");
                !COMPOSE_EXCLUDE.contains(&status)
            });
        }

        if ordered_atoms.is_empty() {
            return Ok(json!({
                "status": "ok",
                "data": {
                    "query": raw_query,
                    "markdown": "# Knowledge Briefing\n\nNo atoms found.",
                    "domains": [],
                    "atoms": [],
                    "count": 0,
                },
            }));
        }

        let mut items: Vec<ScoredTextItem> = ordered_atoms
            .iter()
            .map(|a| ScoredTextItem {
                id: a.id.to_string(),
                slug: a.slug.clone(),
                name: a.name.clone(),
                text: atom_embed_text(a),
                score: 1.0,
            })
            .collect();

        rerank_text_items(runtime, &raw_query, &mut items).await?;

        let atom_ids: Vec<String> = ordered_atoms.iter().map(|a| a.id.to_string()).collect();
        let atom_cosine_scores: HashMap<String, f32> = items
            .iter()
            .map(|item| (item.id.clone(), item.score))
            .collect();

        let section_map = super::compose::load_sections(runtime, &ns, &atom_ids).await?;

        let has_sections = !section_map.is_empty();

        let mut section_results = if has_sections {
            let domain_member_ids: HashSet<String> = member_slugs
                .iter()
                .filter_map(|slug| {
                    ordered_atoms
                        .iter()
                        .find(|a| a.slug == *slug)
                        .map(|a| a.id.to_string())
                })
                .collect();

            let domain_scores: HashMap<String, f32> = ordered_atoms
                .iter()
                .map(|a| {
                    let id = a.id.to_string();
                    let score = if domain_member_ids.contains(&id) {
                        1.0
                    } else {
                        0.0
                    };
                    (id, score)
                })
                .collect();

            let section_state = khive_brain_core::SectionPosteriorState::default();
            let type_weights: HashMap<String, f32> = section_state
                .deterministic_weights()
                .into_iter()
                .map(|(st, w)| (st.as_str().to_string(), w as f32))
                .collect();

            let q_emb = runtime.embed_query(&raw_query).await.ok();

            if let Some(qe) = q_emb {
                super::compose::score_sections(
                    &raw_query,
                    &qe,
                    &atom_cosine_scores,
                    &section_map,
                    &domain_scores,
                    &type_weights,
                    &super::compose::ComposeScoreWeights::default(),
                )
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let max_tokens = p.max_tokens.unwrap_or(8000).clamp(500, 100_000);
        const CHARS_PER_TOKEN: usize = 4;
        let char_budget = max_tokens * CHARS_PER_TOKEN;

        if !section_results.is_empty() {
            let mut used = 0usize;
            section_results.retain(|s| {
                let cost = s.heading.len() + s.content.len() + 40;
                if used + cost > char_budget {
                    return false;
                }
                used += cost;
                true
            });
        }

        let (markdown, section_json) = if !section_results.is_empty() {
            let md = format_section_compose_markdown(
                &raw_query,
                &resolved_domains,
                &ordered_atoms,
                &section_results,
            );
            let sj: Vec<Value> = section_results
                .iter()
                .map(|s| {
                    json!({
                        "section_id": s.section_id,
                        "atom_id": s.atom_id,
                        "section_type": s.section_type,
                        "heading": s.heading,
                        "score": (s.score * 10000.0).round() / 10000.0,
                        "breakdown": {
                            "section_cosine": (s.score_breakdown.section_cosine * 10000.0).round() / 10000.0,
                            "section_bm25": (s.score_breakdown.section_bm25 * 10000.0).round() / 10000.0,
                            "atom_cosine": (s.score_breakdown.atom_cosine * 10000.0).round() / 10000.0,
                            "domain_score": (s.score_breakdown.domain_score * 10000.0).round() / 10000.0,
                            "type_weight": (s.score_breakdown.type_weight * 10000.0).round() / 10000.0,
                        },
                    })
                })
                .collect();
            (md, sj)
        } else {
            let mut used = 0usize;
            let sorted_atoms: Vec<(&Atom, f32)> = items
                .iter()
                .filter_map(|item| {
                    ordered_atoms
                        .iter()
                        .find(|a| a.id.to_string() == item.id)
                        .map(|a| (a, item.score))
                })
                .take_while(|(a, _)| {
                    let cost = a.name.len() + a.content.len() + 40;
                    if used + cost > char_budget {
                        return false;
                    }
                    used += cost;
                    true
                })
                .collect();
            (
                format_compose_markdown(&raw_query, &resolved_domains, &sorted_atoms),
                Vec::new(),
            )
        };

        let atom_json: Vec<Value> = items
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "slug": item.slug,
                    "name": item.name,
                    "score": item.score,
                })
            })
            .collect();

        let domain_json: Vec<Value> = resolved_domains
            .iter()
            .map(|d| json!({ "id": d.id.to_string(), "slug": d.slug, "name": d.name }))
            .collect();

        let count = atom_json.len();

        let mut data = json!({
            "query": raw_query,
            "markdown": markdown,
            "domains": domain_json,
            "atoms": atom_json,
            "count": count,
        });
        if !section_json.is_empty() {
            data["sections"] = json!(section_json);
            data["section_count"] = json!(section_json.len());
        }

        Ok(json!({
            "status": "ok",
            "data": data,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── embed-intent regression ───────────────────────────────────────────────
    // Guard that the ANN query paths in `search` and `suggest` use the
    // query-intent embedding call, not the generic `runtime.embed(...)`.
    // Uses include_str! so the assertion runs on the actual source bytes,
    // but splits the needle to avoid matching the needle itself in test source.
    #[test]
    fn knowledge_ann_query_paths_use_query_intent_embed() {
        let src = include_str!("search.rs");
        // Build needle at runtime to avoid self-match in include_str.
        let generic_needle: String = [".embed(", "&raw_query)"].concat();
        let generic_count = src
            .lines()
            // Skip lines that are part of this test body (contain "concat" or "needle").
            .filter(|l| !l.contains("concat") && !l.contains("needle"))
            .filter(|l| l.contains(&generic_needle))
            .count();
        assert_eq!(
            generic_count, 0,
            "ANN query paths must not call generic {generic_needle}; \
             found {generic_count} occurrence(s) — use embed_query instead"
        );
        // Confirm the query-intent call is present for both search and suggest.
        let query_intent_needle: String = [".embed_query(", "&raw_query)"].concat();
        let query_intent_count = src
            .lines()
            .filter(|l| !l.contains("concat"))
            .filter(|l| l.contains(&query_intent_needle))
            .count();
        // 3 sites: knowledge.search ANN path, knowledge.suggest ANN path,
        // and the section-scoring query embed (search.rs:~1291).
        assert_eq!(
            query_intent_count, 3,
            "expected exactly 3 {query_intent_needle} calls \
             (search ANN + suggest ANN + section query), found {query_intent_count}"
        );
    }

    // ── filter_by_excluded_statuses ───────────────────────────────────────────

    fn make_hit(id: &str, status: Option<&str>, score: f32) -> ScoredHit {
        ScoredHit {
            id: id.to_string(),
            slug: id.to_string(),
            name: id.to_string(),
            content: None,
            tags: None,
            finalized: false,
            is_domain: false,
            status: status.map(str::to_string),
            score,
        }
    }

    #[test]
    fn filter_excluded_statuses_removes_draft_hits() {
        let mut hits = vec![
            make_hit("reviewed-1", Some("reviewed"), 0.8),
            make_hit("draft-1", Some("draft"), 0.7),
            make_hit("reviewed-2", Some("reviewed"), 0.6),
            make_hit("draft-2", Some("draft"), 0.5),
        ];
        filter_by_excluded_statuses(&mut hits, &["draft", "deprecated"]);
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(
            ids,
            ["reviewed-1", "reviewed-2"],
            "draft hits must be removed"
        );
    }

    #[test]
    fn filter_excluded_statuses_removes_deprecated_hits() {
        let mut hits = vec![
            make_hit("reviewed-1", Some("reviewed"), 0.9),
            make_hit("deprecated-1", Some("deprecated"), 0.8),
        ];
        filter_by_excluded_statuses(&mut hits, &["draft", "deprecated"]);
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["reviewed-1"]);
    }

    #[test]
    fn filter_excluded_statuses_empty_list_is_noop() {
        let mut hits = vec![
            make_hit("draft-1", Some("draft"), 0.9),
            make_hit("reviewed-1", Some("reviewed"), 0.8),
        ];
        filter_by_excluded_statuses(&mut hits, &[]);
        assert_eq!(hits.len(), 2, "empty exclude list must be a no-op");
    }

    #[test]
    fn filter_excluded_statuses_null_status_treated_as_not_excluded() {
        // Hits with no status (ANN-sourced before hydration completes) must not
        // be removed by the status exclusion — they are not drafts or deprecated.
        let mut hits = vec![
            make_hit("no-status", None, 0.9),
            make_hit("draft-1", Some("draft"), 0.7),
        ];
        filter_by_excluded_statuses(&mut hits, &["draft", "deprecated"]);
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["no-status"], "null-status hit must survive exclusion");
    }

    #[test]
    fn normalize_rrf_score_is_bounded_and_monotonic() {
        let k = RRF_K;
        let max_single = 1.0f32 / (k as f32 + 1.0);
        let scores_single = [
            max_single * 0.25,
            max_single * 0.5,
            max_single,
            max_single * 1.5,
        ];
        let normed_single: Vec<f32> = scores_single
            .iter()
            .map(|&r| normalize_rrf_score(r, 1, k))
            .collect();
        for &s in &normed_single {
            assert!((0.0..=1.0).contains(&s), "score out of range: {s}");
        }
        assert!(normed_single[0] < normed_single[1]);
        assert!(normed_single[1] < normed_single[2]);
        assert_eq!(normed_single[3], 1.0);

        let max_two = 2.0f32 / (k as f32 + 1.0);
        let scores_two = [max_two * 0.25, max_two * 0.75, max_two, max_two * 2.0];
        let normed_two: Vec<f32> = scores_two
            .iter()
            .map(|&r| normalize_rrf_score(r, 2, k))
            .collect();
        for &s in &normed_two {
            assert!((0.0..=1.0).contains(&s), "score out of range: {s}");
        }
        assert!(normed_two[0] < normed_two[1]);
        assert!(normed_two[1] < normed_two[2]);
        assert_eq!(normed_two[3], 1.0);

        let raw = [0.001f32, 0.005, 0.010, 0.015];
        let normed: Vec<f32> = raw.iter().map(|&r| normalize_rrf_score(r, 1, k)).collect();
        let raw_order: Vec<usize> = {
            let mut idx: Vec<usize> = (0..raw.len()).collect();
            idx.sort_by(|&a, &b| raw[b].partial_cmp(&raw[a]).unwrap());
            idx
        };
        let norm_order: Vec<usize> = {
            let mut idx: Vec<usize> = (0..normed.len()).collect();
            idx.sort_by(|&a, &b| normed[b].partial_cmp(&normed[a]).unwrap());
            idx
        };
        assert_eq!(
            raw_order, norm_order,
            "normalization must not invert ranking"
        );
    }

    #[test]
    fn normalize_rrf_score_zero_source_count_returns_zero() {
        assert_eq!(normalize_rrf_score(0.5, 0, RRF_K), 0.0);
    }
}
