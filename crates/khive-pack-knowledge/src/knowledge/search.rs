//! Search, suggest, and compose handlers plus TF-IDF scoring infrastructure.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_score::DeterministicScore;
use khive_storage::types::{SqlStatement, SqlValue};

use super::matching;
use super::schema::{Atom, ComposeParams, Domain, SearchParams, SuggestParams};
use super::util::{
    atom_embed_text, atom_from_row, deser, domain_from_row, explicitly_requested_status, is_stop,
    row_bool, row_str, sql_err, status_multiplier, status_sql_clause, status_values,
    CANDIDATE_POOL, D_COVERAGE_ALPHA, D_EXPAND_DISCOUNT, D_W_BIGRAM, D_W_CONTENT, D_W_DESCRIPTION,
    D_W_EXACT_NAME, D_W_NAME, D_W_TAGS, MIN_TERM_LEN,
};
use super::vamana;
use super::KnowledgeHandlers;

// ─── TF-IDF weight container ─────────────────────────────────────────────────

struct Weights {
    w_exact_name: f32,
    w_name: f32,
    w_description: f32,
    w_tags: f32,
    w_content: f32,
    expand_discount: f32,
    coverage_alpha: f32,
    w_bigram: f32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            w_exact_name: D_W_EXACT_NAME,
            w_name: D_W_NAME,
            w_description: D_W_DESCRIPTION,
            w_tags: D_W_TAGS,
            w_content: D_W_CONTENT,
            expand_discount: D_EXPAND_DISCOUNT,
            coverage_alpha: D_COVERAGE_ALPHA,
            w_bigram: D_W_BIGRAM,
        }
    }
}

impl Weights {
    fn from_opts(opts: &SearchParams) -> Self {
        let w = opts.weights.as_ref();
        Self {
            w_exact_name: w
                .and_then(|w| w.w_exact_name)
                .map_or(D_W_EXACT_NAME, |v| v as f32),
            w_name: w.and_then(|w| w.w_name).map_or(D_W_NAME, |v| v as f32),
            w_description: w
                .and_then(|w| w.w_description)
                .map_or(D_W_DESCRIPTION, |v| v as f32),
            w_tags: w.and_then(|w| w.w_tags).map_or(D_W_TAGS, |v| v as f32),
            w_content: w
                .and_then(|w| w.w_content)
                .map_or(D_W_CONTENT, |v| v as f32),
            expand_discount: w
                .and_then(|w| w.expand_discount)
                .map_or(D_EXPAND_DISCOUNT, |v| v as f32),
            coverage_alpha: w
                .and_then(|w| w.coverage_alpha)
                .map_or(D_COVERAGE_ALPHA, |v| v as f32),
            w_bigram: w.and_then(|w| w.w_bigram).map_or(D_W_BIGRAM, |v| v as f32),
        }
    }
}

// ─── scored hit (internal) ────────────────────────────────────────────────────

struct ScoredHit {
    id: String,
    slug: String,
    name: String,
    description: Option<String>,
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
                description: None,
                tags: None,
                finalized: false,
                is_domain: false,
                status: None,
                score,
            });
        }
    }
}

// ─── candidate (tokenized) ───────────────────────────────────────────────────

struct Candidate {
    id: String,
    slug: String,
    name_raw: String,
    description_raw: Option<String>,
    tags_raw: Option<String>,
    status_raw: Option<String>,
    finalized: bool,
    is_domain: bool,
    name: Vec<String>,
    description: Vec<String>,
    tags: Vec<String>,
    content: Vec<String>,
}

fn load_candidates_from_atoms(atoms: &[Atom], type_filter: Option<&str>) -> Vec<Candidate> {
    let want_domain = type_filter == Some("domain");
    let want_atom = type_filter == Some("atom");

    atoms
        .iter()
        .filter_map(|atom| {
            let tags_str = atom.tags_display();
            let is_domain = {
                let tags_arr: Vec<String> = serde_json::from_str(&atom.tags).unwrap_or_default();
                tags_arr.iter().any(|t| t == "type:domain")
            };
            if (want_domain && !is_domain) || (want_atom && is_domain) {
                return None;
            }
            Some(Candidate {
                id: atom.id.to_string(),
                slug: atom.slug.clone(),
                name_raw: atom.name.clone(),
                description_raw: atom.description.clone(),
                tags_raw: Some(tags_str.clone()),
                status_raw: atom.status.clone(),
                finalized: atom.finalized,
                is_domain,
                name: matching::tokenize_field(&atom.name),
                description: atom
                    .description
                    .as_deref()
                    .map(matching::tokenize_field)
                    .unwrap_or_default(),
                tags: matching::tokenize_field(&tags_str),
                content: matching::tokenize_field(&atom.content),
            })
        })
        .collect()
}

// ─── IDF computation ──────────────────────────────────────────────────────────

fn compute_idf(
    candidates: &[Candidate],
    terms: &[String],
    expanded: &HashSet<String>,
    discount: f32,
) -> HashMap<String, f32> {
    let n = candidates.len() as f32;
    let mut df: HashMap<String, usize> = terms.iter().map(|t| (t.clone(), 0)).collect();
    for cand in candidates {
        for term in terms {
            if matching::has_in_tokens(&cand.content, term)
                || matching::has_in_tokens(&cand.name, term)
                || matching::has_in_tokens(&cand.description, term)
                || matching::has_in_tokens(&cand.tags, term)
            {
                if let Some(d) = df.get_mut(term) {
                    *d += 1;
                }
            }
        }
    }
    df.into_iter()
        .map(|(term, d)| {
            let raw = (n / (d as f32 + 1.0)).ln().max(0.1);
            let idf = if expanded.contains(&term) {
                raw * discount
            } else {
                raw
            };
            (term, idf)
        })
        .collect()
}

fn score_field(tokens: &[String], terms: &[String], idf: &HashMap<String, f32>) -> f32 {
    let mut score = 0.0;
    for term in terms {
        let count = matching::count_in_tokens(tokens, term);
        if count > 0 {
            let tf = 1.0 + (count as f32).ln();
            score += tf * idf.get(term).copied().unwrap_or(1.0);
        }
    }
    score
}

fn bigram_bonus_field(tokens: &[String], query_order: &[String]) -> f32 {
    if query_order.len() < 2 {
        return 0.0;
    }
    let filtered: Vec<&str> = tokens
        .iter()
        .filter(|t| !is_stop(t))
        .map(|t| t.as_str())
        .collect();
    let mut bonus = 0.0f32;
    for window in query_order.windows(2) {
        let (a, b) = (window[0].as_str(), window[1].as_str());
        for w in filtered.windows(2) {
            if w[0] == a && w[1] == b {
                bonus += 1.0;
                break;
            }
        }
    }
    bonus
}

fn exact_name_bonus(name: &str, raw_query: &str, bonus: f32) -> f32 {
    let q = raw_query.trim().to_lowercase();
    if !q.is_empty() && name.to_lowercase().contains(&q) {
        bonus
    } else {
        0.0
    }
}

fn score_candidate(
    cand: &Candidate,
    terms: &[String],
    original_terms: &[String],
    query_order: &[String],
    idf: &HashMap<String, f32>,
    raw_query: &str,
    w: &Weights,
) -> f32 {
    let bigrams = bigram_bonus_field(&cand.name, query_order)
        + bigram_bonus_field(&cand.description, query_order)
        + bigram_bonus_field(&cand.tags, query_order)
        + bigram_bonus_field(&cand.content, query_order);

    let base = exact_name_bonus(&cand.name_raw, raw_query, w.w_exact_name)
        + w.w_name * score_field(&cand.name, terms, idf)
        + w.w_description * score_field(&cand.description, terms, idf)
        + w.w_tags * score_field(&cand.tags, terms, idf)
        + w.w_content * score_field(&cand.content, terms, idf)
        + w.w_bigram * bigrams;

    if w.coverage_alpha > 0.0 && !original_terms.is_empty() {
        // For each original query term, check whether it OR any of its expanded
        // variants matches the candidate. This ensures that "agents" → "agent"
        // expansion still earns coverage credit.
        let matched = original_terms
            .iter()
            .filter(|orig| {
                let has_exact = matching::has_in_tokens(&cand.name, orig)
                    || matching::has_in_tokens(&cand.description, orig)
                    || matching::has_in_tokens(&cand.tags, orig)
                    || matching::has_in_tokens(&cand.content, orig);
                if has_exact {
                    return true;
                }
                terms.iter().filter(|t| *t != *orig).any(|exp| {
                    matching::has_in_tokens(&cand.name, exp)
                        || matching::has_in_tokens(&cand.description, exp)
                        || matching::has_in_tokens(&cand.tags, exp)
                        || matching::has_in_tokens(&cand.content, exp)
                })
            })
            .count();
        let coverage = matched as f32 / original_terms.len() as f32;
        base * coverage.powf(w.coverage_alpha)
    } else {
        base
    }
}

fn expand_terms(terms: &mut Vec<String>) -> HashSet<String> {
    let originals: HashSet<String> = terms.iter().cloned().collect();
    let snapshot: Vec<String> = terms.clone();
    for t in &snapshot {
        if !t.ends_with('s') && t.len() >= 3 {
            terms.push(format!("{t}s"));
        }
        if t.ends_with("ies") && t.len() > 4 {
            let s = format!("{}y", &t[..t.len() - 3]);
            if s.len() >= 3 {
                terms.push(s);
            }
        } else if t.ends_with('s') && !t.ends_with("ss") && t.len() > 3 {
            let s = t[..t.len() - 1].to_string();
            if s.len() >= 3 {
                terms.push(s);
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms
        .iter()
        .filter(|t| !originals.contains(*t))
        .cloned()
        .collect()
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
    exclude_status: Option<&str>,
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
        let (status_clause, status_params) = status_sql_clause(statuses, exclude_status, 3);
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

    let (status_clause, status_params) = status_sql_clause(statuses, exclude_status, ids.len() + 2);
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
    exclude_status: Option<&'a str>,
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
        ctx.exclude_status,
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
            description: cand.description_raw.clone(),
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
        exclude_status: ctx.exclude_status,
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
        .map(|h| format!("{} {}", h.name, h.description.as_deref().unwrap_or("")))
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
                "SELECT id, slug, name, description, tags, finalized, status FROM knowledge_atoms WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL"
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
            hit.description = row_str(row, "description");
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
            hit.description = row_str(row, "description");
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
        reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_domains WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose domain by slug", e))?
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
        reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM knowledge_atoms WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose atom by slug", e))?
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

fn format_compose_markdown(query: &str, domains: &[Domain], atoms: &[(&Atom, f32)]) -> String {
    let mut out = String::from("# Knowledge Briefing\n\n");
    out.push_str(&format!("Query: {query}\n"));
    for (atom, score) in atoms {
        out.push_str(&format!("\n## {}\n\n", atom.name));
        out.push_str(&format!("Source: {}\n", atom.slug));
        out.push_str(&format!("Score: {:.4}\n", score));
        if let Some(ref desc) = atom.description {
            if !desc.is_empty() {
                out.push('\n');
                out.push_str(desc);
                out.push('\n');
            }
        }
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
                ("w_description", w.w_description),
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

        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter,
            min_score,
            w: &w,
            fetch_limit,
            statuses: &requested_statuses,
            exclude_status: p.exclude_status.as_deref(),
        };

        let mut hits = if do_decompose && non_stop_count >= decompose_threshold {
            search_decomposed(&ctx, &raw_query, intersection_bonus).await?
        } else {
            search_core(&ctx, &raw_query).await?
        };

        // Trigger background warm — never block search on the ANN rebuild.
        vamana::ensure_ann_background(runtime, token, ann);

        if let Ok(query_emb) = runtime.embed(&raw_query).await {
            let ann_k = fetch_limit.max(20);
            let key = vamana::AnnKey::new(&ns, runtime.default_embedder_name());
            if let Some(ann_hits) = vamana::search_loaded(ann, &key, &query_emb, ann_k).await {
                if !ann_hits.is_empty() {
                    fuse_ann_hits(&mut hits, &ann_hits, min_score);
                    hydrate_empty_hits(runtime, &ns, &mut hits).await;
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
                    "description": h.description,
                    "tags": h.tags,
                    "status": h.status,
                    "finalized": h.finalized,
                    "kind": if h.is_domain { "domain" } else { "atom" },
                    "score": h.score,
                })
            })
            .collect();
        let count = results.len();

        Ok(json!({
            "status": "ok",
            "data": { "results": results, "count": count },
        }))
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
        let limit = p.limit.unwrap_or(8).clamp(1, 100);
        let ns = token.namespace().as_str().to_owned();

        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter: Some("domain"),
            min_score: 0.0,
            w: &Weights::default(),
            fetch_limit: limit * 3,
            statuses: &[],
            exclude_status: None,
        };

        let mut hits = search_core(&ctx, &raw_query).await?;

        vamana::ensure_ann_background(runtime, token, ann);
        if let Ok(query_emb) = runtime.embed(&raw_query).await {
            let ann_k = (limit * 3).max(20);
            let key = vamana::AnnKey::new(&ns, runtime.default_embedder_name());
            if let Some(ann_hits) = vamana::search_loaded(ann, &key, &query_emb, ann_k).await {
                if !ann_hits.is_empty() {
                    fuse_ann_hits(&mut hits, &ann_hits, 0.0);
                    hydrate_empty_hits(runtime, &ns, &mut hits).await;
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

        Ok(json!({
            "status": "ok",
            "data": { "results": results, "count": count },
        }))
    }

    pub(crate) async fn compose(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ComposeParams = deser(params)?;
        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }

        let domain_ids: Vec<String> = p
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

        if domain_ids.is_empty() && atom_ids.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "domain_ids or atom_ids must be provided".into(),
            ));
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

        let sorted_atoms: Vec<(&Atom, f32)> = items
            .iter()
            .filter_map(|item| {
                ordered_atoms
                    .iter()
                    .find(|a| a.id.to_string() == item.id)
                    .map(|a| (a, item.score))
            })
            .collect();

        let markdown = format_compose_markdown(&raw_query, &resolved_domains, &sorted_atoms);

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

        Ok(json!({
            "status": "ok",
            "data": {
                "query": raw_query,
                "markdown": markdown,
                "domains": domain_json,
                "atoms": atom_json,
                "count": count,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
