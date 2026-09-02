//! Search, suggest, and compose handlers.
//!
//! TF-IDF scoring primitives live in `super::scoring`; this module owns the
//! FTS/ANN pipeline, reranking, hydration, and handler dispatch.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    hex_prefix_to_uuid_pattern, KhiveRuntime, Namespace, NamespaceToken, RuntimeError,
};
use khive_score::DeterministicScore;
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_storage::EntityFilter;

use super::matching;
use super::schema::{Atom, ComposeParams, Domain, SearchParams, SuggestParams};
use super::scoring::{
    compute_idf, exact_name_bonus, expand_terms, load_candidates_from_atoms, score_candidate,
    Candidate, Weights,
};
use super::util::{
    atom_embed_text, atom_from_row, compose_item_char_cost, deser, domain_from_row,
    estimate_compose_item_tokens, explicitly_requested_status, is_stop, row_bool, row_i64, row_str,
    sql_err, status_multiplier, status_sql_clause, status_values, CANDIDATE_POOL, CHARS_PER_TOKEN,
    D_SUGGEST_RERANK_ALPHA, MIN_TERM_LEN,
};
use super::vamana;
use super::KnowledgeHandlers;

// ─── scored hit (internal) ────────────────────────────────────────────────────

#[derive(Clone)]
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

enum AnnAvailability {
    Ready,
    WarmingTimedOut { corpus_non_empty: bool },
    Absent,
}

struct AnnSearchState {
    hits: Vec<(Uuid, f32)>,
    availability: AnnAvailability,
    /// Whether the ANN source itself returned fewer than `k` entries.
    /// Fresh-tail deletes may shrink `hits` afterward without proving that
    /// deeper ANN candidates do not exist.
    source_exhausted: bool,
}

struct FreshTailSearchState {
    hits: Vec<(Uuid, f32)>,
    source_exhausted: bool,
}

async fn merge_fresh_tail_for_search(
    runtime: &KhiveRuntime,
    ann: &vamana::SharedAnn,
    key: &vamana::AnnKey,
    query_embedding: &[f32],
    k: usize,
    loaded: Option<(Vec<(Uuid, f32)>, u64)>,
) -> FreshTailSearchState {
    let (candidates, watermark, source_exhausted) = match loaded {
        Some((candidates, watermark)) => {
            let source_exhausted = candidates.len() < k;
            (candidates, Some(watermark), source_exhausted)
        }
        None => (Vec::new(), None, true),
    };
    match vamana::fresh_tail_leg(runtime, ann, key, query_embedding, k, watermark).await {
        vamana::FreshTailOutcome::Ops(ops) => FreshTailSearchState {
            hits: vamana::merge_fresh_tail(candidates, query_embedding, ops),
            source_exhausted,
        },
        vamana::FreshTailOutcome::Replace {
            candidates,
            source_exhausted,
        } => FreshTailSearchState {
            hits: candidates,
            source_exhausted,
        },
        vamana::FreshTailOutcome::Skipped => FreshTailSearchState {
            hits: candidates,
            source_exhausted,
        },
    }
}

/// Search the loaded ANN slot, waiting a bounded time when its warm is in flight.
async fn search_ann_with_warm_wait(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &vamana::SharedAnn,
    key: &vamana::AnnKey,
    query_embedding: &[f32],
    k: usize,
) -> AnnSearchState {
    if let Some(loaded) = vamana::search_loaded_with_seq(ann, key, query_embedding, k).await {
        let tail =
            merge_fresh_tail_for_search(runtime, ann, key, query_embedding, k, Some(loaded)).await;
        return AnnSearchState {
            hits: tail.hits,
            availability: AnnAvailability::Ready,
            source_exhausted: tail.source_exhausted,
        };
    }
    if !vamana::is_warming_not_loaded(ann, key) {
        let tail = merge_fresh_tail_for_search(runtime, ann, key, query_embedding, k, None).await;
        return AnnSearchState {
            hits: tail.hits,
            availability: AnnAvailability::Absent,
            source_exhausted: tail.source_exhausted,
        };
    }
    if vamana::wait_ready(
        ann,
        key,
        vamana::warm_wait_timeout_ms(),
        vamana::ANN_WARM_WAIT_POLL_MS,
    )
    .await
    {
        let loaded = vamana::search_loaded_with_seq(ann, key, query_embedding, k).await;
        let availability = if loaded.is_some() {
            AnnAvailability::Ready
        } else {
            AnnAvailability::Absent
        };
        let tail = merge_fresh_tail_for_search(runtime, ann, key, query_embedding, k, loaded).await;
        return AnnSearchState {
            hits: tail.hits,
            availability,
            source_exhausted: tail.source_exhausted,
        };
    }

    let corpus_non_empty =
        vamana::compute_fingerprint(runtime, token, runtime.default_embedder_name())
            .await
            .map(|fingerprint| fingerprint.vector_count > 0)
            .unwrap_or(false);
    let tail = merge_fresh_tail_for_search(runtime, ann, key, query_embedding, k, None).await;
    AnnSearchState {
        hits: tail.hits,
        availability: AnnAvailability::WarmingTimedOut { corpus_non_empty },
        source_exhausted: tail.source_exhausted,
    }
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

fn fuse_ann_hits(fts_hits: &mut Vec<ScoredHit>, ann_hits: &[ScoredHit], min_score: f32) {
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
        .map(|hit| (hit.id.clone(), DeterministicScore::from_f32(hit.score)))
        .collect();
    for hit in ann_hits {
        by_id.entry(hit.id.clone()).or_insert_with(|| hit.clone());
    }

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
        }
    }
}

// ─── status filtering (post-hydration) ───────────────────────────────────────

/// Remove hits whose `status` is in `exclude_statuses` after hydration.
fn filter_by_excluded_statuses(hits: &mut Vec<ScoredHit>, exclude_statuses: &[&str]) {
    if exclude_statuses.is_empty() {
        return;
    }
    hits.retain(|hit| {
        let status = hit.status.as_deref().unwrap_or("");
        !exclude_statuses.contains(&status)
    });
}

/// Apply the complete public status contract to hydrated hits.
///
/// An explicit `status=` is an allowlist, not merely a request to disable the
/// default exclusions. This distinction is load-bearing for ANN candidates,
/// which do not pass through the FTS SQL predicate.
fn filter_hits_by_status(
    hits: &mut Vec<ScoredHit>,
    statuses: &[String],
    exclude_statuses: &[&str],
) {
    if statuses.is_empty() {
        filter_by_excluded_statuses(hits, exclude_statuses);
        return;
    }

    hits.retain(|hit| {
        hit.status
            .as_deref()
            .is_some_and(|status| statuses.iter().any(|allowed| allowed == status))
    });
}

fn deprecated_allowed_by_status_policy(statuses: &[String], exclude_statuses: &[&str]) -> bool {
    if statuses.is_empty() {
        !exclude_statuses.contains(&"deprecated")
    } else {
        explicitly_requested_status(statuses, "deprecated")
    }
}

// ─── type filtering (post-hydration) ─────────────────────────────────────────

/// Remove hits that do not match `type_filter` after hydration.
///
/// Mirrors the FTS/SQL path in `fetch_fts_candidates` (the full-scan fallback):
///
/// - `Some("domain")` keeps only domain hits (`hit.is_domain == true`).
/// - `Some(other)` where other is non-empty keeps only non-domain hits.
/// - `None` or `Some("")` is a no-op.
///
/// Applied to hydrated ANN candidates before fusion/refill and again to the
/// fused pool as a final shared-source guard.
fn filter_hits_by_type(hits: &mut Vec<ScoredHit>, type_filter: Option<&str>) {
    let filt = match type_filter {
        Some(f) if !f.is_empty() => f,
        _ => return,
    };
    let want_domain = filt == "domain";
    hits.retain(|hit| {
        if want_domain {
            hit.is_domain
        } else {
            !hit.is_domain
        }
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

/// Re-applies `min_score` after [`apply_status_multipliers`] so it is a genuine
/// floor on the scores returned to the caller.
///
/// The fusion-stage application in `fuse_ann_hits` stays as an early admission
/// filter, but the multiplier step rewrites every surviving score via
/// `s / (s + 1)` (mapping 1.0 to 0.5), so a hit that cleared fusion can land
/// below the caller's floor. Filtering again here — after the rewrite, before
/// the `limit` truncation — guarantees every returned score is >= `min_score`;
/// returning fewer than `limit` hits when the floor removes some is correct.
fn enforce_min_score_floor(hits: &mut Vec<ScoredHit>, min_score: f32) {
    hits.retain(|hit| hit.score >= min_score);
}

// ─── FTS5 candidate expression ───────────────────────────────────────────────

fn quote_fts5_phrase(raw_query: &str) -> String {
    let escaped = raw_query.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Build the per-term FTS5 match clauses the candidate fetch runs bounded
/// subqueries over — one quoted phrase per de-duplicated, non-stop, expanded
/// term. Queries with no scoreable term fall back to the exact raw phrase
/// (same fallback `fts5_candidate_expression` uses).
///
/// FTS is only the candidate generator; TF-IDF remains the ranker. Requiring
/// the whole raw query as one phrase drops candidates whose matching terms are
/// separated in the document; per-term clauses keep those non-contiguous
/// matches reachable for the scorer to judge.
fn fts5_candidate_terms(raw_query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms: Vec<String> = matching::tokenize_field(raw_query)
        .into_iter()
        .filter(|term| term.len() >= MIN_TERM_LEN && !is_stop(term))
        .filter(|term| seen.insert(term.clone()))
        .collect();

    if terms.is_empty() {
        vec![quote_fts5_phrase(raw_query)]
    } else {
        // Candidate recall observes the same singular/plural expansion as the
        // scorer. The returned set is used by IDF weighting later; expansion's
        // mutation of `terms` is the only result needed here.
        let _ = expand_terms(&mut terms);
        terms.iter().map(|term| quote_fts5_phrase(term)).collect()
    }
}

/// The OR-joined form of [`fts5_candidate_terms`], used only for the cheap
/// unordered existence probe in `fetch_fts_candidates` (no `ORDER BY bm25`,
/// `LIMIT 1`) — never as the ordered candidate query itself. See issue #1930:
/// running that ordered query over the full OR-joined match set is what made
/// `knowledge.suggest`/`knowledge.search` blow the read deadline at scale.
fn fts5_candidate_expression(raw_query: &str) -> String {
    fts5_candidate_terms(raw_query).join(" OR ")
}

/// SQL eligibility predicate for the public atom/domain kind filter.
///
/// Domain mirrors are atoms carrying the exact `type:domain` tag. Applying
/// this predicate in the FTS/full-scan query is load-bearing: filtering after
/// `LIMIT` lets the wrong kind consume every candidate slot.
fn type_eligibility_sql(type_filter: Option<&str>, atom_alias: &str) -> String {
    match type_filter {
        Some("domain") => format!(" AND {atom_alias}.tags LIKE '%\"type:domain\"%'"),
        Some(filter) if !filter.is_empty() => {
            format!(" AND {atom_alias}.tags NOT LIKE '%\"type:domain\"%'")
        }
        _ => String::new(),
    }
}

// ─── FTS5 candidate pool fetch ────────────────────────────────────────────────

/// Per-term bounded candidate cap (issue #1930). A single-term `MATCH` orders
/// a much smaller row set than the OR-joined expression over every expanded
/// term, so bounding each term independently — instead of bounding only the
/// combined result — keeps the read cost proportional to the number of terms,
/// never to the size of the full match set.
const FTS_TERM_LIMIT: usize = 500;

/// Outcome of the bounded lexical candidate fetch.
///
/// `timed_out` is set only for [`khive_storage::StorageError::Timeout`] — the
/// request-scoped read deadline elapsing mid-fetch. Any other storage error
/// (including a genuine FTS5 syntax/parser error) still surfaces as an `Err`;
/// fail-open applies to a timeout only.
struct FtsFetchOutcome {
    atoms: Vec<Atom>,
    timed_out: bool,
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct FtsTestDeadlineAdvance {
    after_completed_terms: usize,
    by: std::time::Duration,
}

#[cfg(test)]
tokio::task_local! {
    static FTS_TEST_DEADLINE_ADVANCE: FtsTestDeadlineAdvance;
}

#[cfg(test)]
async fn advance_fts_test_deadline_after_term(completed_terms: usize) {
    let advance_by = FTS_TEST_DEADLINE_ADVANCE
        .try_with(|control| {
            (completed_terms == control.after_completed_terms).then_some(control.by)
        })
        .ok()
        .flatten();
    if let Some(advance_by) = advance_by {
        tokio::time::advance(advance_by).await;
    }
}

fn is_timeout(e: &khive_storage::StorageError) -> bool {
    matches!(e, khive_storage::StorageError::Timeout { .. })
}

/// Same check, for the `RuntimeError` shape `?`-propagated storage timeouts
/// arrive in at the handler layer.
fn is_read_timeout(e: &RuntimeError) -> bool {
    matches!(
        e,
        RuntimeError::Storage(khive_storage::StorageError::Timeout { .. })
    )
}

/// Fetch a bounded lexical candidate pool.
///
/// Replaces a single `ORDER BY bm25(...)` over one OR-joined match expression
/// (whose cost scales with the size of the entire match set — the #1930 read
/// timeout at ~94K atoms) with one bounded, independently-capped subquery per
/// term, unioned and deduplicated in application code. FTS remains only the
/// candidate generator; TF-IDF in `search_core` remains the ranker, so the
/// per-term merge order does not need to be a globally correct bm25 rank.
async fn fetch_fts_candidates(
    runtime: &KhiveRuntime,
    ns: &str,
    raw_query: &str,
    type_filter: Option<&str>,
    statuses: &[String],
    exclude_statuses: &[&str],
    fetch_limit: usize,
) -> Result<FtsFetchOutcome, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(reader) => reader,
        Err(e) if is_timeout(&e) => {
            return Ok(FtsFetchOutcome {
                atoms: Vec::new(),
                timed_out: true,
            });
        }
        Err(e) => return Err(sql_err("search fts reader", e)),
    };

    let terms = fts5_candidate_terms(raw_query);
    let type_clause = type_eligibility_sql(type_filter, "a");
    let (status_clause, status_params) = status_sql_clause(statuses, exclude_statuses, 4);
    let per_term_limit = if terms.len() == 1 {
        fetch_limit
    } else {
        fetch_limit.clamp(1, FTS_TERM_LIMIT)
    };

    let mut seen_ids: HashSet<Uuid> = HashSet::new();
    let mut combined: Vec<Atom> = Vec::new();

    // Join the canonical atom row before LIMIT so deleted, status-ineligible,
    // and wrong-kind FTS rows cannot consume the bounded candidate window.
    // bm25 orders each term's eligible matches before its own cap; slug is
    // the stable tie break for equal lexical rank.
    let per_term_sql = format!(
        "SELECT a.* FROM fts_knowledge \
         JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid \
         WHERE fts_knowledge MATCH ?1 \
           AND fts_knowledge.namespace = ?2 \
           AND a.namespace = ?2 \
           AND a.deleted_at IS NULL{status_clause}{type_clause} \
         ORDER BY bm25(fts_knowledge), a.slug \
         LIMIT ?3"
    );

    // Query every term rather than stopping once `combined` reaches
    // `fetch_limit` — an early break made pool membership depend on query
    // word order (a fast-filling early term could starve every later term
    // of a query at all). Each term's rows are collected independently and
    // merged round-robin below, so no single term can crowd out the rest.
    let mut per_term_rows: Vec<Vec<Atom>> = Vec::with_capacity(terms.len());
    let mut term_query_timed_out = false;

    for term in &terms {
        let mut params = vec![
            SqlValue::Text(term.clone()),
            SqlValue::Text(ns.to_owned()),
            SqlValue::Integer(per_term_limit as i64),
        ];
        params.extend(status_params.iter().cloned());

        let rows = match reader
            .query_all(SqlStatement {
                sql: per_term_sql.clone(),
                params,
                label: None,
            })
            .await
        {
            Ok(rows) => rows,
            Err(e) if is_timeout(&e) => {
                term_query_timed_out = true;
                break;
            }
            Err(e) => return Err(sql_err("search fts query", e)),
        };

        per_term_rows.push(rows.iter().filter_map(atom_from_row).collect());
        #[cfg(test)]
        advance_fts_test_deadline_after_term(per_term_rows.len()).await;
    }

    let max_term_rows = per_term_rows.iter().map(Vec::len).max().unwrap_or(0);
    'merge: for i in 0..max_term_rows {
        for term_rows in &per_term_rows {
            if combined.len() >= fetch_limit {
                break 'merge;
            }
            if let Some(atom) = term_rows.get(i) {
                if seen_ids.insert(atom.id) {
                    combined.push(atom.clone());
                }
            }
        }
    }

    if term_query_timed_out {
        return Ok(FtsFetchOutcome {
            atoms: combined,
            timed_out: true,
        });
    }

    if !combined.is_empty() {
        return Ok(FtsFetchOutcome {
            atoms: combined,
            timed_out: false,
        });
    }

    // No term produced an eligible row. Preserve the fallback's established
    // meaning: it is for a lexical miss, not for a lexical match whose rows
    // were all ineligible. In the latter case an empty result is correct; a
    // full scan could otherwise admit unrelated zero-score rows when
    // min_score is the default 0. This probe has no ORDER BY and LIMIT 1, so
    // it stays cheap even over the OR-joined expression.
    let match_expr = fts5_candidate_expression(raw_query);
    let raw_fts_match = match reader
        .query_row(SqlStatement {
            sql: "SELECT 1 AS present FROM fts_knowledge \
                  WHERE fts_knowledge MATCH ?1 AND namespace = ?2 LIMIT 1"
                .to_string(),
            params: vec![SqlValue::Text(match_expr), SqlValue::Text(ns.to_owned())],
            label: None,
        })
        .await
    {
        Ok(row) => row,
        Err(e) if is_timeout(&e) => {
            return Ok(FtsFetchOutcome {
                atoms: Vec::new(),
                timed_out: true,
            });
        }
        Err(e) => return Err(sql_err("search fts eligibility probe", e)),
    };
    if raw_fts_match.is_some() {
        return Ok(FtsFetchOutcome {
            atoms: Vec::new(),
            timed_out: false,
        });
    }

    // FTS returned nothing — fall back to a bounded full scan. Eligibility
    // remains inside SQL so the cap is filled from rows the caller can
    // actually receive.
    let (status_clause, status_params) = status_sql_clause(statuses, exclude_statuses, 3);
    let sql_str = format!(
        "SELECT a.* FROM knowledge_atoms AS a \
         WHERE a.namespace = ?1 AND a.deleted_at IS NULL{status_clause}{type_clause} \
         ORDER BY a.created_at DESC, a.slug LIMIT ?2"
    );
    let mut params = vec![
        SqlValue::Text(ns.to_owned()),
        SqlValue::Integer(fetch_limit as i64),
    ];
    params.extend(status_params);

    let rows = match reader
        .query_all(SqlStatement {
            sql: sql_str,
            params,
            label: None,
        })
        .await
    {
        Ok(rows) => rows,
        Err(e) if is_timeout(&e) => {
            return Ok(FtsFetchOutcome {
                atoms: Vec::new(),
                timed_out: true,
            });
        }
        Err(e) => return Err(sql_err("search full scan", e)),
    };

    Ok(FtsFetchOutcome {
        atoms: rows.iter().filter_map(atom_from_row).collect(),
        timed_out: false,
    })
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

/// `search_core`'s result plus whether the lexical/FTS candidate fetch hit the
/// request read deadline (issue #1930's fail-open signal). A caller sees
/// `hits` possibly empty/partial and `lexical_timed_out = true` instead of an
/// `Err` — never a verb-level error for a genuine timeout.
struct SearchCoreOutcome {
    hits: Vec<ScoredHit>,
    lexical_timed_out: bool,
}

async fn search_core(ctx: &SearchCtx<'_>, query: &str) -> Result<SearchCoreOutcome, RuntimeError> {
    let runtime = ctx.runtime;
    let ns = ctx.ns;
    let role = ctx.role;
    let type_filter = ctx.type_filter;
    let min_score = ctx.min_score;
    let w = ctx.w;
    let fetch_limit = ctx.fetch_limit;
    let raw_query = query.trim().to_string();
    if raw_query.is_empty() {
        return Ok(SearchCoreOutcome {
            hits: Vec::new(),
            lexical_timed_out: false,
        });
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

    let FtsFetchOutcome { atoms, timed_out } = fetch_fts_candidates(
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
        return Ok(SearchCoreOutcome {
            hits: Vec::new(),
            lexical_timed_out: timed_out,
        });
    }

    let candidates = load_candidates_from_atoms(&atoms, type_filter);
    if candidates.is_empty() {
        return Ok(SearchCoreOutcome {
            hits: Vec::new(),
            lexical_timed_out: timed_out,
        });
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

    Ok(SearchCoreOutcome {
        hits: scored
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
            .collect(),
        lexical_timed_out: timed_out,
    })
}

// ─── decomposed search ───────────────────────────────────────────────────────

async fn search_decomposed(
    ctx: &SearchCtx<'_>,
    query: &str,
    intersection_bonus: f32,
) -> Result<SearchCoreOutcome, RuntimeError> {
    let non_stop: Vec<&str> = query
        .split_whitespace()
        .filter(|w| w.len() >= MIN_TERM_LEN && !is_stop(&w.to_lowercase()))
        .collect();

    let mid = non_stop.len() / 2;
    let sub_q1: String = non_stop[..mid].join(" ");
    let sub_q2: String = non_stop[mid..].join(" ");
    let sub_limit = ctx.fetch_limit.min(50);

    let SearchCoreOutcome {
        hits: full,
        lexical_timed_out: full_timed_out,
    } = search_core(ctx, query).await?;
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
    let SearchCoreOutcome {
        hits: s1,
        lexical_timed_out: s1_timed_out,
    } = search_core(&sub_ctx1, &sub_q1).await?;
    let SearchCoreOutcome {
        hits: s2,
        lexical_timed_out: s2_timed_out,
    } = search_core(&sub_ctx1, &sub_q2).await?;
    let lexical_timed_out = full_timed_out || s1_timed_out || s2_timed_out;

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
    Ok(SearchCoreOutcome {
        hits: ranked,
        lexical_timed_out,
    })
}

// ─── embedding rerank ────────────────────────────────────────────────────────

async fn embed_cosine_scores(
    runtime: &KhiveRuntime,
    query: &str,
    candidate_texts: &[String],
) -> Result<Option<Vec<f32>>, RuntimeError> {
    if runtime.default_embedder_name().is_empty() || candidate_texts.is_empty() {
        return Ok(None);
    }
    let mut texts = Vec::with_capacity(candidate_texts.len() + 1);
    texts.push(query.to_string());
    texts.extend_from_slice(candidate_texts);
    let embeddings = match khive_storage::await_request_read_phase(
        "knowledge.embedding_rerank",
        runtime.embed_batch(&texts),
    )
    .await?
    {
        Ok(embeddings) => embeddings,
        Err(_) => return Ok(None),
    };
    if embeddings.len() != texts.len() {
        return Ok(None);
    }
    let query_emb = &embeddings[0];
    Ok(Some(
        embeddings[1..]
            .iter()
            .map(|emb| cosine_similarity(query_emb, emb))
            .collect(),
    ))
}

async fn rerank_with_embeddings(
    runtime: &KhiveRuntime,
    query: &str,
    hits: &mut [ScoredHit],
    alpha: f32,
) -> Result<bool, RuntimeError> {
    if hits.is_empty() {
        return Ok(false);
    }
    let texts: Vec<String> = hits
        .iter()
        .map(|h| format!("{} {}", h.name, h.content.as_deref().unwrap_or("")))
        .collect();
    if let Some(cosines) = embed_cosine_scores(runtime, query, &texts).await? {
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
        return Ok(true);
    }
    Ok(false)
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

// Keep one namespace bind plus the ID binds comfortably below SQLite's
// portable 999-variable ceiling.
const HYDRATION_ID_CHUNK: usize = 900;

/// Hydrate ANN-only hit shells from the canonical corpus tables.
///
/// Returns the number of candidate rows that could not be hydrated. Missing
/// rows (for example a stale ANN id) and storage-read failures both degrade the
/// candidate pool instead of failing an otherwise-useful lexical response, but
/// unresolved shells are always removed and the count is surfaced to callers.
async fn hydrate_empty_hits(runtime: &KhiveRuntime, ns: &str, hits: &mut Vec<ScoredHit>) -> usize {
    let ids: Vec<String> = hits
        .iter()
        .filter(|hit| hit.slug.is_empty())
        .map(|hit| hit.id.clone())
        .collect();
    if ids.is_empty() {
        return 0;
    }

    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(r) => r,
        Err(error) => {
            tracing::warn!(
                namespace = ns,
                requested = ids.len(),
                error = %error,
                "knowledge candidate hydration could not acquire a reader"
            );
            hits.retain(|hit| !hit.slug.is_empty());
            return ids.len();
        }
    };

    let mut atom_rows = Vec::new();
    for chunk in ids.chunks(HYDRATION_ID_CHUNK) {
        let placeholders = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let mut params = vec![SqlValue::Text(ns.to_owned())];
        params.extend(chunk.iter().cloned().map(SqlValue::Text));

        match reader
            .query_all(SqlStatement {
                sql: format!(
                    "SELECT id, slug, name, content, tags, finalized, status FROM knowledge_atoms WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL"
                ),
                params,
                label: None,
            })
            .await
        {
            Ok(rows) => atom_rows.extend(rows),
            Err(error) => {
                tracing::warn!(
                    namespace = ns,
                    requested = chunk.len(),
                    error = %error,
                    "knowledge atom candidate hydration chunk degraded"
                );
            }
        }
    }

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
        return 0;
    }

    let mut domain_rows = Vec::new();
    for chunk in missing_ids.chunks(HYDRATION_ID_CHUNK) {
        let placeholders = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let mut params = vec![SqlValue::Text(ns.to_owned())];
        params.extend(chunk.iter().cloned().map(SqlValue::Text));

        match reader
            .query_all(SqlStatement {
                sql: format!(
                    "SELECT id, slug, name, description, tags, status FROM knowledge_domains WHERE namespace = ?1 AND id IN ({placeholders}) AND deleted_at IS NULL"
                ),
                params,
                label: None,
            })
            .await
        {
            Ok(rows) => domain_rows.extend(rows),
            Err(error) => {
                tracing::warn!(
                    namespace = ns,
                    requested = chunk.len(),
                    error = %error,
                    "knowledge domain candidate hydration chunk degraded"
                );
            }
        }
    }

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
            hit.status = row_str(row, "status");
        }
    }

    let failed = hits.iter().filter(|hit| hit.slug.is_empty()).count();
    hits.retain(|hit| !hit.slug.is_empty());
    if failed > 0 {
        tracing::warn!(
            namespace = ns,
            requested = ids.len(),
            failed,
            "knowledge candidate hydration returned a degraded pool"
        );
    }
    failed
}

/// Add hydration degradation to a response without disturbing another
/// degradation diagnostic (for example suggest's ANN-unavailable object).
fn attach_hydration_degradation(out: &mut Value, hydration_failures: usize) {
    if hydration_failures == 0 {
        return;
    }

    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }
    out["degraded"]["hydration_failures"] = json!(hydration_failures);
}

/// Flag that the lexical/FTS candidate fetch hit the request read deadline
/// (issue #1930). Set alongside whatever ANN-backed results (if any) still
/// made it into the response — a timed-out lexical stage degrades the
/// response, it never fails the verb outright.
fn attach_lexical_timeout_degradation(out: &mut Value) {
    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }
    out["degraded"]["lexical_timeout"] = json!(true);
}

/// Flag that the best-effort body-line aggregate hit the request read
/// deadline after the search itself completed. The ranked hits are kept and
/// their atom rows report `body_lines: null`; the timeout degrades metadata,
/// it never fails the verb outright.
fn attach_body_lines_timeout_degradation(out: &mut Value) {
    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }
    out["degraded"]["body_lines_timeout"] = json!(true);
}

struct EligibleAnnSearchState {
    hits: Vec<ScoredHit>,
    availability: AnnAvailability,
    hydration_failures: usize,
}

/// Retrieve an ANN pool whose bounded, rank-preserving truncation happens only
/// after canonical hydration and caller eligibility.
///
/// Vamana has no metadata predicate, so a selective status/kind filter may
/// consume the first raw top-k. Widen exponentially and re-evaluate the full
/// deterministic prefix until the eligible target is filled or the vector
/// corpus is exhausted. The common case performs one ANN search and one
/// hydration pass; only filtered/invalid prefixes pay for widening.
async fn search_eligible_ann_with_refill(
    ctx: &SearchCtx<'_>,
    token: &NamespaceToken,
    ann: &vamana::SharedAnn,
    key: &vamana::AnnKey,
    query_embedding: &[f32],
    target_eligible: usize,
    initial_k: usize,
) -> Result<EligibleAnnSearchState, RuntimeError> {
    let runtime = ctx.runtime;
    let target_eligible = target_eligible.max(1);
    let mut request_k = initial_k.max(target_eligible).max(1);

    loop {
        khive_storage::ensure_request_read_active("knowledge.search")?;
        let AnnSearchState {
            hits: raw_hits,
            availability,
            source_exhausted,
        } = search_ann_with_warm_wait(runtime, token, ann, key, query_embedding, request_k).await;

        let mut seen = HashSet::with_capacity(raw_hits.len());
        let mut hits: Vec<ScoredHit> = raw_hits
            .into_iter()
            .filter(|(id, _)| seen.insert(*id))
            .map(|(id, score)| ScoredHit {
                id: id.to_string(),
                slug: String::new(),
                name: String::new(),
                content: None,
                tags: None,
                finalized: false,
                is_domain: false,
                status: None,
                score,
            })
            .collect();

        let hydration_failures = hydrate_empty_hits(runtime, ctx.ns, &mut hits).await;
        khive_storage::ensure_request_read_active("knowledge.search")?;
        filter_hits_by_status(&mut hits, ctx.statuses, ctx.exclude_statuses);
        filter_hits_by_type(&mut hits, ctx.type_filter);

        if hits.len() >= target_eligible || source_exhausted {
            hits.truncate(target_eligible);
            return Ok(EligibleAnnSearchState {
                hits,
                availability,
                hydration_failures,
            });
        }

        // The live vector-store count is not a sound upper bound for a serving
        // bridge: a fresh delete removes the canonical vector before its tail
        // tombstone is merged into that older bridge. Widen until the ANN
        // source itself proves exhaustion.
        let next_k = request_k.saturating_mul(2);
        if next_k == request_k {
            hits.truncate(target_eligible);
            return Ok(EligibleAnnSearchState {
                hits,
                availability,
                hydration_failures,
            });
        }
        request_k = next_k;
    }
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
                sql: "SELECT d.*, (SELECT a.properties FROM knowledge_atoms a WHERE a.id=d.id) AS properties FROM knowledge_domains d WHERE d.id = ?1 AND d.namespace = ?2 AND d.deleted_at IS NULL LIMIT 1".into(),
                params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("compose domain by id", e))?
    } else {
        let by_slug = reader
            .query_row(SqlStatement {
                sql: "SELECT d.*, (SELECT a.properties FROM knowledge_atoms a WHERE a.id=d.id) AS properties FROM knowledge_domains d WHERE d.slug = ?1 AND d.namespace = ?2 AND d.deleted_at IS NULL LIMIT 1".into(),
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
                let pattern = format!("{}%", hex_prefix_to_uuid_pattern(&id));
                let rows = reader
                    .query_all(SqlStatement {
                        sql: "SELECT d.*, (SELECT a.properties FROM knowledge_atoms a WHERE a.id=d.id) AS properties FROM knowledge_domains d WHERE d.id LIKE ?1 AND d.namespace = ?2 AND d.deleted_at IS NULL LIMIT 2".into(),
                        params: vec![
                            SqlValue::Text(pattern),
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
                let pattern = format!("{}%", hex_prefix_to_uuid_pattern(&id));
                let rows = reader
                    .query_all(SqlStatement {
                        sql: "SELECT * FROM knowledge_atoms WHERE id LIKE ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 2".into(),
                        params: vec![
                            SqlValue::Text(pattern),
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

async fn load_domain_member_token_sizes(
    runtime: &KhiveRuntime,
    ns: &str,
    domain_ids: &[String],
) -> Result<HashMap<String, usize>, RuntimeError> {
    let mut sizes: HashMap<String, usize> = domain_ids.iter().map(|id| (id.clone(), 0)).collect();
    if domain_ids.is_empty() {
        return Ok(sizes);
    }

    let placeholders = domain_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(domain_ids.iter().cloned().map(SqlValue::Text));

    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("suggest member size reader", e))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT d.id AS domain_id, a.name, a.content \
                 FROM knowledge_domains AS d \
                 JOIN json_each(d.members) AS member ON 1 = 1 \
                 JOIN knowledge_atoms AS a \
                   ON a.namespace = d.namespace \
                  AND a.slug = member.value \
                  AND a.deleted_at IS NULL \
                 WHERE d.namespace = ?1 \
                   AND d.id IN ({placeholders}) \
                   AND d.deleted_at IS NULL"
            ),
            params,
            label: None,
        })
        .await
        .map_err(|e| sql_err("suggest member size query", e))?;

    for row in rows {
        let Some(domain_id) = row_str(&row, "domain_id") else {
            continue;
        };
        let Some(content) = row_str(&row, "content") else {
            continue;
        };
        let name = row_str(&row, "name").unwrap_or_default();
        let size = sizes.entry(domain_id).or_default();
        *size = size.saturating_add(estimate_compose_item_tokens(&name, &content));
    }

    Ok(sizes)
}

/// Body-line metadata is best-effort: a request read-deadline timeout on
/// either the reader checkout or the aggregate query returns `Ok(None)` —
/// the already-ranked hits report `body_lines: null` with a degradation
/// flag instead of the whole search failing. Non-timeout storage errors
/// still propagate.
///
/// The line count follows `str::lines()` semantics: a terminal newline does
/// not add a line, blank interior lines count, and empty content is 0.
async fn load_atom_body_line_counts(
    runtime: &KhiveRuntime,
    ns: &str,
    atom_ids: &[String],
) -> Result<Option<HashMap<String, usize>>, RuntimeError> {
    let mut counts: HashMap<String, usize> = atom_ids.iter().map(|id| (id.clone(), 0)).collect();
    if atom_ids.is_empty() {
        return Ok(Some(counts));
    }

    let placeholders = atom_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(atom_ids.iter().cloned().map(SqlValue::Text));

    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(reader) => reader,
        Err(e) if is_timeout(&e) => return Ok(None),
        Err(e) => return Err(sql_err("search body line count reader", e)),
    };
    let rows = match reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT atom_id, \
                        SUM(CASE WHEN content = '' THEN 0 \
                                 ELSE length(content) \
                                      - length(replace(content, char(10), '')) \
                                      + (CASE WHEN substr(content, -1) = char(10) \
                                              THEN 0 ELSE 1 END) \
                            END) AS body_lines \
                 FROM knowledge_sections \
                 WHERE namespace = ?1 AND atom_id IN ({placeholders}) \
                 GROUP BY atom_id"
            ),
            params,
            label: None,
        })
        .await
    {
        Ok(rows) => rows,
        Err(e) if is_timeout(&e) => return Ok(None),
        Err(e) => return Err(sql_err("search body line count query", e)),
    };

    for row in rows {
        let Some(atom_id) = row_str(&row, "atom_id") else {
            continue;
        };
        let Some(body_lines) = row_i64(&row, "body_lines") else {
            continue;
        };
        if let Ok(body_lines) = usize::try_from(body_lines) {
            counts.insert(atom_id, body_lines);
        }
    }

    Ok(Some(counts))
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
    if let Some(cosines) = embed_cosine_scores(runtime, query, &texts).await? {
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

// ─── KG entity blending (ADR-051 Amendment 1) ─────────────────────────────────

/// The KG entity kinds `knowledge.compose` blends into a briefing. Concepts
/// and documents are the kinds that carry the measured, expert-curated
/// content (algorithms, papers, ADRs) that outranks generic lore atoms on
/// sharply technical queries — see ADR-051 Amendment 1.
const KG_BLEND_ENTITY_KINDS: [&str; 2] = ["concept", "document"];

/// Cap on blended KG entities per briefing, so entities stay a supplementary
/// "Knowledge graph" section and atoms remain the body of the briefing.
const KG_BLEND_CAP: usize = 5;

/// A `concept`/`document` KG entity blended into a compose briefing.
struct KgEntityHit {
    id: String,
    kind: String,
    name: String,
    description: String,
    score: f32,
}

/// Finds `concept`/`document` KG entities relevant to `query`, reranked with
/// the same embedding-cosine signal `rerank_text_items` uses for atom bodies
/// (embed `name + description`, cosine against the query embedding). Because
/// both pools are scored with the identical metric against the identical
/// query embedding, the resulting scores land on the same 0..1 scale as
/// atom/section scores — direct comparison, not a separate rank-fusion step,
/// is what makes them a valid blended candidate pool.
///
/// Candidate discovery itself reuses `KhiveRuntime::hybrid_search` — the same
/// FTS+ANN RRF-fused retrieval path `kg.search(kind="entity")` dispatches to
/// (`khive-pack-kg`'s `handle_search` calls the identical method) — so this
/// does not stand up a parallel retrieval stack; only the final relevance
/// score is recomputed, to land on the atom-comparable scale.
///
/// `min_score` is the self-calibrating inclusion floor (ADR-051 Amendment
/// 1): only hits scoring at or above it survive, applied after rerank and
/// before the `cap` truncation. Callers derive it from the minimum rerank
/// score among the atoms that made the final compose body.
async fn search_kg_entities(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ns: &str,
    query: &str,
    cap: usize,
    min_score: f32,
) -> Result<Vec<KgEntityHit>, RuntimeError> {
    let candidate_k = ((cap * 4) as u32).max(20);
    let mut candidate_ids: Vec<Uuid> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    for kind in KG_BLEND_ENTITY_KINDS {
        let hits = runtime
            .hybrid_search(token, query, None, candidate_k, Some(kind), None, &[], None)
            .await?;
        for hit in hits {
            if seen.insert(hit.entity_id) {
                candidate_ids.push(hit.entity_id);
            }
        }
    }
    if candidate_ids.is_empty() {
        return Ok(Vec::new());
    }

    let visible_ns: Vec<String> = token
        .visible_namespace_strs()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let entities_page = runtime
        .entities(token)?
        .query_entities(
            ns,
            EntityFilter {
                ids: candidate_ids.clone(),
                kinds: KG_BLEND_ENTITY_KINDS
                    .iter()
                    .map(|k| k.to_string())
                    .collect(),
                namespaces: visible_ns,
                ..EntityFilter::default()
            },
            PageRequest {
                offset: 0,
                limit: candidate_ids.len() as u32,
            },
        )
        .await?;

    if entities_page.items.is_empty() {
        return Ok(Vec::new());
    }

    let texts: Vec<String> = entities_page
        .items
        .iter()
        .map(|e| format!("{} {}", e.name, e.description.as_deref().unwrap_or("")))
        .collect();
    let cosines = match embed_cosine_scores(runtime, query, &texts).await? {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };

    let mut hits: Vec<KgEntityHit> = entities_page
        .items
        .iter()
        .zip(cosines.iter())
        .map(|(e, &score)| KgEntityHit {
            id: e.id.to_string(),
            kind: e.kind.clone(),
            name: e.name.clone(),
            description: e.description.clone().unwrap_or_default(),
            score: score.max(0.0),
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    // Self-calibrating inclusion floor (ADR-051 Amendment 1): an entity only
    // blends in if it outranks the weakest atom that made the final body —
    // no fixed score constant. Applied after rerank, before the cap, so a
    // query with many strong entities doesn't starve out a marginal one that
    // still clears the floor.
    hits.retain(|h| h.score >= min_score);
    hits.truncate(cap);
    Ok(hits)
}

/// Trims already-sorted, best-first `hits` to fit `remaining_budget`
/// characters, using the same per-item cost accounting the atom/section trim
/// loops use (`name + description` length plus a fixed per-entry overhead).
/// Entities are trimmed against whatever budget the atom/section trim left
/// over, so a tight `max_tokens` never evicts an atom to make room for a
/// blended entity.
fn trim_kg_entities_to_budget(hits: Vec<KgEntityHit>, remaining_budget: usize) -> Vec<KgEntityHit> {
    let mut used = 0usize;
    hits.into_iter()
        .take_while(|h| {
            let cost = compose_item_char_cost(&h.name, &h.description);
            if used + cost > remaining_budget {
                return false;
            }
            used += cost;
            true
        })
        .collect()
}

fn format_kg_entities_markdown(entities: &[KgEntityHit]) -> String {
    let mut out = String::from("\n---\n\n## Knowledge graph\n\n");
    for e in entities {
        out.push_str(&format!("- **{}** ({})", e.name, e.kind));
        if !e.description.is_empty() {
            out.push_str(&format!(" — {}", e.description));
        }
        out.push('\n');
    }
    out
}

fn format_section_compose_markdown(
    query: &str,
    domains: &[Domain],
    atoms: &[Atom],
    sections: &[super::compose::ComposeSectionResult],
    explain: bool,
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
                if explain {
                    out.push_str(&format!("\n### {} (score: {:.4})\n\n", s.heading, s.score));
                } else {
                    out.push_str(&format!("\n### {}\n\n", s.heading));
                }
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

fn format_compose_markdown(
    query: &str,
    domains: &[Domain],
    atoms: &[(&Atom, f32)],
    explain: bool,
) -> String {
    let mut out = String::from("# Knowledge Briefing\n\n");
    out.push_str(&format!("Query: {query}\n"));
    for (atom, score) in atoms {
        out.push_str(&format!("\n## {}\n\n", atom.name));
        out.push_str(&format!("Source: {}\n", atom.slug));
        if explain {
            out.push_str(&format!("Score: {:.4}\n", score));
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
        khive_storage::ensure_request_read_active("knowledge.search")?;
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
        if let Some(kind) = p.kind.as_deref() {
            if !matches!(kind, "atom" | "domain") {
                return Err(RuntimeError::InvalidInput(format!(
                    "kind must be one of: atom, domain; got {kind:?}"
                )));
            }
        }
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

        // Normalize exclude_status once: trim whitespace, treat blank as absent.
        // This single normalized value feeds both the SQL predicate (via SearchCtx)
        // and the ANN post-hydration filter, ensuring both result sources see the
        // identical exclusion set regardless of how the caller formatted the value.
        let exclude_status_normalized: Option<&str> = p
            .exclude_status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(status) = exclude_status_normalized {
            if !matches!(status, "draft" | "reviewed" | "deprecated") {
                return Err(RuntimeError::InvalidInput(format!(
                    "exclude_status must be one of: draft, reviewed, deprecated; got {status:?}"
                )));
            }
        }

        // Precedence (highest to lowest, matches ADR-047 §Status filtering):
        //   1. explicit status=  → no exclusion; SQL and hydrated ANN use the allowlist
        //   2. no status=, explicit exclude_status= (non-blank) → use that exclusion
        //   3. no status=, include_drafts=true → exclude only deprecated
        //   4. default (no status params / blank exclude_status) → exclude draft and deprecated
        let effective_exclude_statuses: Vec<&str> = if !requested_statuses.is_empty() {
            // Caller specified exact status; the shared allowlist wins.
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
        // The zero multiplier for deprecated is also a final eligibility gate.
        // Resolve its override from the same precedence policy used before FTS
        // caps and ANN refill; otherwise explicit exclude_status can admit a
        // row early only for the multiplier stage to remove it later.
        let allow_deprecated =
            deprecated_allowed_by_status_policy(&requested_statuses, &effective_exclude_statuses);

        let ctx = SearchCtx {
            runtime,
            ns: &ns,
            role: p.role.as_deref(),
            type_filter,
            min_score,
            w: &w,
            fetch_limit,
            statuses: &requested_statuses,
            exclude_statuses: &effective_exclude_statuses,
        };

        // Trigger background warm — never block search on the ANN rebuild.
        vamana::ensure_ann_background(runtime, token, ann);

        // Fetch ANN candidates BEFORE the lexical stage. The lexical fetch is
        // now bounded (issue #1930) but still non-zero cost; running ANN
        // first means a lexical read-deadline timeout afterward cannot
        // discard ANN results that were already safely computed — the
        // degraded arm below can then report ANN-backed results instead of
        // erroring the whole verb. A read timeout in this ANN stage itself
        // is likewise fail-open, never propagated as a verb-level error.
        let mut ann_hits: Vec<ScoredHit> = Vec::new();
        let mut ann_availability: Option<AnnAvailability> = None;
        let mut hydration_failures = 0usize;
        let ann_k = fetch_limit.max(20);
        match khive_storage::await_request_read_phase(
            "knowledge.search",
            runtime.embed_query(&raw_query),
        )
        .await
        {
            Ok(Ok(query_emb)) => {
                let model = runtime.default_embedder_name();
                let key = vamana::AnnKey::new(&ns, model);
                match search_eligible_ann_with_refill(
                    &ctx, token, ann, &key, &query_emb, ann_k, ann_k,
                )
                .await
                {
                    Ok(EligibleAnnSearchState {
                        hits,
                        availability,
                        hydration_failures: ann_hydration_failures,
                    }) => {
                        hydration_failures += ann_hydration_failures;
                        ann_hits = hits;
                        ann_availability = Some(availability);
                    }
                    Err(e) if is_read_timeout(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(Err(_)) => {}
            Err(e) if is_timeout(&e) => {}
            Err(e) => return Err(e.into()),
        }

        let SearchCoreOutcome {
            mut hits,
            lexical_timed_out,
        } = if do_decompose && non_stop_count >= decompose_threshold {
            search_decomposed(&ctx, &raw_query, intersection_bonus).await?
        } else {
            search_core(&ctx, &raw_query).await?
        };

        let mut ann_unavailable = false;
        if !ann_hits.is_empty() {
            fuse_ann_hits(&mut hits, &ann_hits, min_score);
        }
        // FTS hits remain valid partial results. Preserve the existing
        // advisory only for a non-empty corpus with no lexical fallback.
        if matches!(
            ann_availability,
            Some(AnnAvailability::WarmingTimedOut {
                corpus_non_empty: true
            })
        ) && hits.is_empty()
        {
            ann_unavailable = true;
        }
        // Apply shared eligibility unconditionally so every source observes the
        // same final status and kind contract even when ANN did not run.
        filter_hits_by_status(&mut hits, &requested_statuses, &effective_exclude_statuses);
        filter_hits_by_type(&mut hits, type_filter);

        // See `suggest`'s matching guard: a lexical-stage timeout means the
        // request read deadline is already spent, so skip the further
        // embedding read rather than let it convert a degraded-but-ok
        // response into a verb-level error.
        if do_rerank && !hits.is_empty() && !lexical_timed_out {
            rerank_with_embeddings(runtime, &raw_query, &mut hits, rerank_alpha).await?;
        }

        apply_status_multipliers(&mut hits, allow_deprecated);
        enforce_min_score_floor(&mut hits, min_score);
        hits.truncate(limit);

        let atom_ids: Vec<String> = hits
            .iter()
            .filter(|hit| !hit.is_domain)
            .map(|hit| hit.id.clone())
            .collect();
        let mut body_lines_timed_out = false;
        let body_line_counts = if lexical_timed_out {
            None
        } else {
            match load_atom_body_line_counts(runtime, &ns, &atom_ids).await? {
                Some(counts) => Some(counts),
                None => {
                    body_lines_timed_out = true;
                    None
                }
            }
        };

        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                let body_lines = if h.is_domain {
                    None
                } else {
                    body_line_counts
                        .as_ref()
                        .and_then(|counts| counts.get(&h.id).copied())
                };
                json!({
                    "id": h.id,
                    "slug": h.slug,
                    "name": h.name,
                    "content": h.content,
                    "body_lines": body_lines,
                    "tags": h.tags,
                    "status": h.status,
                    "finalized": h.finalized,
                    "kind": if h.is_domain { "domain" } else { "atom" },
                    "score": h.score,
                })
            })
            .collect();
        let count = results.len();

        let mut out = json!({ "results": results, "total": count });
        if ann_unavailable {
            out["ann_unavailable"] = json!(true);
        }
        if lexical_timed_out {
            attach_lexical_timeout_degradation(&mut out);
        }
        if body_lines_timed_out {
            attach_body_lines_timeout_degradation(&mut out);
        }
        attach_hydration_degradation(&mut out, hydration_failures);
        // A lexical-stage or body-line-stage timeout already committed this
        // call to a degraded response (never a verb-level error, issue #1930)
        // — re-checking the same expired deadline here would discard it.
        if !lexical_timed_out && !body_lines_timed_out {
            khive_storage::ensure_request_read_active("knowledge.search")?;
        }
        Ok(out)
    }

    pub(crate) async fn suggest(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        khive_storage::ensure_request_read_active("knowledge.suggest")?;
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

        // Fetch ANN candidates BEFORE the lexical stage — same rationale as
        // `search`: the lexical fetch is bounded (issue #1930) but still
        // non-zero cost, so computing ANN first means a lexical read-deadline
        // timeout afterward cannot discard ANN results already in hand.
        vamana::ensure_ann_background(runtime, token, ann);
        let mut ann_hits: Vec<ScoredHit> = Vec::new();
        let mut ann_availability: Option<AnnAvailability> = None;
        let mut hydration_failures = 0usize;
        // Over-fetch aggressively: the corpus is ~27% domains / ~73% atoms, so
        // limit*3 would return mostly atoms that all get dropped after type filtering.
        // 50× over-fetch (floor 200) gives domains a fair chance to appear in the
        // top ANN neighbors before the type gate discards atom hits.
        let ann_k = (limit * 50).max(200);
        match khive_storage::await_request_read_phase(
            "knowledge.suggest",
            runtime.embed_query(&raw_query),
        )
        .await
        {
            Ok(Ok(query_emb)) => {
                let model = runtime.default_embedder_name();
                let key = vamana::AnnKey::new(&ns, model);
                match search_eligible_ann_with_refill(
                    &ctx,
                    token,
                    ann,
                    &key,
                    &query_emb,
                    ctx.fetch_limit,
                    ann_k,
                )
                .await
                {
                    Ok(EligibleAnnSearchState {
                        hits,
                        availability,
                        hydration_failures: ann_hydration_failures,
                    }) => {
                        hydration_failures += ann_hydration_failures;
                        ann_hits = hits;
                        ann_availability = Some(availability);
                    }
                    Err(e) if is_read_timeout(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(Err(_)) => {}
            Err(e) if is_timeout(&e) => {}
            Err(e) => return Err(e.into()),
        }

        let SearchCoreOutcome {
            mut hits,
            lexical_timed_out,
        } = search_core(&ctx, &raw_query).await?;

        let mut ann_unavailable = false;
        if !ann_hits.is_empty() {
            fuse_ann_hits(&mut hits, &ann_hits, 0.0);
        }
        // Suggest always reports degraded candidate recall for a
        // non-empty corpus, even when lexical candidates survived.
        if let Some(AnnAvailability::WarmingTimedOut { corpus_non_empty }) = ann_availability {
            ann_unavailable = corpus_non_empty;
        }

        filter_hits_by_status(&mut hits, &[], SUGGEST_EXCLUDE);
        filter_hits_by_type(&mut hits, Some("domain"));

        // A lexical-stage timeout already committed this call to a degraded,
        // ANN-backed response — the request read deadline is spent, so
        // skip further reads/embedding calls rather than let them convert
        // this into a verb-level error.
        let fresh_rerank_applied = if lexical_timed_out {
            false
        } else {
            rerank_with_embeddings(runtime, &raw_query, &mut hits, D_SUGGEST_RERANK_ALPHA).await?
        };

        // Safety net: retain only domain hits in case any non-domain survived above.
        hits.retain(|h| h.is_domain);
        hits.truncate(limit);

        let domain_ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
        let member_token_sizes = if lexical_timed_out {
            HashMap::new()
        } else {
            load_domain_member_token_sizes(runtime, &ns, &domain_ids).await?
        };
        if !lexical_timed_out {
            khive_storage::ensure_request_read_active("knowledge.suggest")?;
        }

        // Price the member atom bodies that compose expands, not the much smaller
        // domain mirror description used for retrieval. The batched join keeps the
        // suggest -> fold budget in compose's estimated-token unit without an N+1
        // hydration pass.
        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "id": h.id,
                    "name": h.name,
                    "score": h.score,
                    "size": member_token_sizes.get(&h.id).copied().unwrap_or_default(),
                })
            })
            .collect();
        let count = results.len();

        let mut out = json!({ "results": results, "total": count });
        if ann_unavailable {
            // issue #91: escalate degradation to a top-level, self-explaining
            // signal instead of a bare total:0 or an unflagged partial list.
            // `ann_unavailable` is kept unchanged for existing callers; `degraded`
            // states the consequence so a caller does not have to infer it.
            out["ann_unavailable"] = json!(true);
            let (mode, note): (&str, &str) = match (count, fresh_rerank_applied) {
                (0, _) => (
                    "no_match",
                    "ANN index unavailable and lexical/FTS matching also found no \
                     domain for this query. This does NOT confirm the corpus has \
                     nothing relevant — only that this call could not find one. \
                     Do not cache as an absence; retry once the index is healthy.",
                ),
                (_, true) => (
                    "ann_candidates_degraded",
                    "ANN index unavailable: candidate retrieval used lexical/FTS \
                     matching, but fresh embedding cosine reranking was applied to \
                     those candidates. Final ranking includes a dense signal, while \
                     topically relevant domains outside the lexical candidate set may \
                     still be missing. Do not cache; retry once the index is healthy.",
                ),
                (_, false) => (
                    "lexical_only",
                    "ANN index unavailable and fresh embedding reranking did not run: \
                     these results were ranked by lexical/FTS matching only. Ranking \
                     may be less precise than a healthy call, and topically relevant \
                     domains outside the lexical match may be missing. Do not cache; \
                     retry once the index is healthy.",
                ),
            };
            out["degraded"] = json!({
                "reason": "ann_unavailable",
                "mode": mode,
                "cache_safe": false,
                "note": note,
            });
        }
        if lexical_timed_out {
            attach_lexical_timeout_degradation(&mut out);
        }
        attach_hydration_degradation(&mut out, hydration_failures);
        if !lexical_timed_out {
            khive_storage::ensure_request_read_active("knowledge.suggest")?;
        }
        Ok(out)
    }

    pub(crate) async fn compose(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
        type_weights: HashMap<String, f32>,
    ) -> Result<Value, RuntimeError> {
        let p: ComposeParams = deser(params)?;

        // Registry dispatch already mints an exact token for an explicit
        // namespace. Direct handler callers must provide that same authorized
        // token; never turn an untrusted business parameter into a stronger
        // namespace capability here.
        let effective_token = match p.namespace.as_deref() {
            Some(ns_str) => {
                let ns = Namespace::parse(ns_str).map_err(|e| {
                    RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
                })?;
                if &ns != token.namespace() {
                    return Err(RuntimeError::InvalidInput(
                        "knowledge.compose namespace does not match authorized token namespace"
                            .to_string(),
                    ));
                }
                // Equality above makes this a safe exact-scope narrowing of
                // any broader direct-call token.
                token.with_namespace(ns)
            }
            None => token.clone(),
        };
        let token = &effective_token;

        let raw_query = p.query.trim().to_string();
        if raw_query.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }
        let explain = p.explain.unwrap_or(false);

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
        // `atom_ids`-only calls (caller pinned exact atoms, no domain_ids) never
        // blend KG entities — the caller opted into exactly those atoms. Auto and
        // explicit-domain_ids calls both blend (ADR-051 Amendment 1).
        let atom_ids_only = domain_ids.is_empty() && !atom_ids.is_empty();
        let blend_kg = p.blend_kg.unwrap_or(true) && !atom_ids_only;
        let mut suggest_ann_unavailable = false;
        let mut suggest_hydration_failures = 0usize;
        if is_auto {
            let word_count = raw_query.split_whitespace().count();
            if word_count < 10 {
                return Err(RuntimeError::InvalidInput(format!(
                    "auto-compose query must be at least 10 words for effective domain selection \
                     (got {word_count}). Provide explicit domain_ids/atom_ids for shorter queries."
                )));
            }
        }

        // #887: unconditional per-stage timing, WARN-on-slow and
        // WARN-on-abandoned. See `super::compose::ComposeTiming` for the
        // full rationale and the completion-contract every early return
        // below must honor (`finish()` before returning, or route the error
        // through `try_or_finish!`). Each `begin(Phase::X)` fires *before*
        // the phase's (possibly fallible, possibly long-running) work — not
        // after — so an in-flight phase is never lost from the breakdown if
        // the request errors, is cancelled, or is abandoned mid-phase.
        use super::compose::Phase;
        let mut timing = super::compose::ComposeTiming::start(&raw_query, is_auto);
        macro_rules! try_or_finish {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => {
                        timing.finish(0);
                        return Err(e.into());
                    }
                }
            };
        }
        try_or_finish!(timing.begin(Phase::Suggest));

        if is_auto {
            let auto_limit = p.auto_limit.unwrap_or(5).clamp(1, 20);
            let suggest_attempt = Self::suggest(
                runtime,
                token,
                json!({ "query": &raw_query, "limit": auto_limit }),
                ann,
            )
            .await;
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let suggest_result = match suggest_attempt {
                Ok(v) => {
                    suggest_ann_unavailable = v
                        .get("ann_unavailable")
                        .and_then(|f| f.as_bool())
                        .unwrap_or(false);
                    suggest_hydration_failures = v
                        .pointer("/degraded/hydration_failures")
                        .and_then(Value::as_u64)
                        .and_then(|count| usize::try_from(count).ok())
                        .unwrap_or(0);
                    v
                }
                Err(e) => {
                    try_or_finish!(khive_storage::ensure_request_read_active(
                        "knowledge.compose"
                    ));
                    tracing::warn!(error = %e, "auto-compose: internal suggest failed, returning empty");
                    let response = json!({
                        "status": "ok",
                        "data": {
                            "query": raw_query,
                            "markdown": "# Knowledge Briefing\n\nDomain suggestion unavailable.",
                            "domains": [],
                            "atoms": [],
                            "count": 0,
                            "suggest_error": e.to_string(),
                        },
                    });
                    try_or_finish!(khive_storage::ensure_request_read_active(
                        "knowledge.compose"
                    ));
                    timing.finish(0);
                    return Ok(response);
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
                let mut data = json!({
                    "query": raw_query,
                    "markdown": "# Knowledge Briefing\n\nNo matching domains found for auto-suggest.",
                    "domains": [],
                    "atoms": [],
                    "count": 0,
                });
                if suggest_ann_unavailable {
                    data["ann_unavailable"] = json!(true);
                }
                attach_hydration_degradation(&mut data, suggest_hydration_failures);
                let response = json!({ "status": "ok", "data": data });
                try_or_finish!(khive_storage::ensure_request_read_active(
                    "knowledge.compose"
                ));
                timing.finish(0);
                return Ok(response);
            }
        }
        try_or_finish!(timing.begin(Phase::Fetch));

        let ns = token.namespace().as_str().to_owned();

        let mut resolved_domains: Vec<Domain> = Vec::new();
        let mut member_slugs: Vec<String> = Vec::new();

        for id in &domain_ids {
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let domain = try_or_finish!(load_domain_by_id_or_slug(runtime, &ns, id).await);
            let members = try_or_finish!(parse_domain_members(&domain));
            member_slugs.extend(members);
            resolved_domains.push(domain);
        }

        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut ordered_atoms: Vec<Atom> = Vec::new();

        for slug in &member_slugs {
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let atom = try_or_finish!(load_atom_by_id_or_slug(runtime, &ns, slug).await);
            if seen_ids.insert(atom.id.to_string()) {
                ordered_atoms.push(atom);
            }
        }
        for id in &atom_ids {
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let atom = try_or_finish!(load_atom_by_id_or_slug(runtime, &ns, id).await);
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
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let mut data = json!({
                "query": raw_query,
                "markdown": "# Knowledge Briefing\n\nNo atoms found.",
                "domains": [],
                "atoms": [],
                "count": 0,
            });
            if suggest_ann_unavailable {
                data["ann_unavailable"] = json!(true);
            }
            attach_hydration_degradation(&mut data, suggest_hydration_failures);
            let response = json!({ "status": "ok", "data": data });
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            timing.finish(0);
            return Ok(response);
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

        try_or_finish!(timing.begin(Phase::Rerank));
        try_or_finish!(rerank_text_items(runtime, &raw_query, &mut items).await);

        let atom_ids: Vec<String> = ordered_atoms.iter().map(|a| a.id.to_string()).collect();
        let atom_cosine_scores: HashMap<String, f32> = items
            .iter()
            .map(|item| (item.id.clone(), item.score))
            .collect();

        try_or_finish!(timing.begin(Phase::Fetch));
        let section_map =
            try_or_finish!(super::compose::load_sections(runtime, &ns, &atom_ids).await);

        let has_sections = !section_map.is_empty();
        try_or_finish!(timing.begin(Phase::Rerank));

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

            let q_emb = try_or_finish!(
                khive_storage::await_request_read_phase(
                    "knowledge.compose",
                    runtime.embed_query(&raw_query),
                )
                .await
            );
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let q_emb = q_emb.ok();

            if let Some(qe) = q_emb {
                try_or_finish!(super::compose::score_sections(
                    &raw_query,
                    &qe,
                    &atom_cosine_scores,
                    &section_map,
                    &domain_scores,
                    &type_weights,
                    &super::compose::ComposeScoreWeights::default(),
                ))
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        try_or_finish!(timing.begin(Phase::Trim));

        let max_tokens = p.max_tokens.unwrap_or(8000).clamp(500, 100_000);
        let char_budget = max_tokens * CHARS_PER_TOKEN;

        // Tracks characters consumed by the atom/section body so blended KG
        // entities (below) trim against whatever budget is left over, never
        // evicting an atom or section to make room for an entity.
        let mut body_used = 0usize;

        if !section_results.is_empty() {
            section_results.retain(|s| {
                let cost = compose_item_char_cost(&s.heading, &s.content);
                if body_used + cost > char_budget {
                    return false;
                }
                body_used += cost;
                true
            });
        }

        let (markdown, section_json, included_atom_ids) = if !section_results.is_empty() {
            let included_atom_ids: HashSet<String> =
                section_results.iter().map(|s| s.atom_id.clone()).collect();
            let md = format_section_compose_markdown(
                &raw_query,
                &resolved_domains,
                &ordered_atoms,
                &section_results,
                explain,
            );
            let sj: Vec<Value> = if explain {
                section_results
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
                    .collect()
            } else {
                Vec::new()
            };
            (md, sj, included_atom_ids)
        } else {
            let sorted_atoms: Vec<(&Atom, f32)> = items
                .iter()
                .filter_map(|item| {
                    ordered_atoms
                        .iter()
                        .find(|a| a.id.to_string() == item.id)
                        .map(|a| (a, item.score))
                })
                .take_while(|(a, _)| {
                    let cost = compose_item_char_cost(&a.name, &a.content);
                    if body_used + cost > char_budget {
                        return false;
                    }
                    body_used += cost;
                    true
                })
                .collect();
            let included_atom_ids: HashSet<String> =
                sorted_atoms.iter().map(|(a, _)| a.id.to_string()).collect();
            (
                format_compose_markdown(&raw_query, &resolved_domains, &sorted_atoms, explain),
                Vec::new(),
                included_atom_ids,
            )
        };

        // KG entity blend (ADR-051 Amendment 1): additive "Knowledge graph"
        // section, trimmed against whatever budget the atom/section body left
        // over. Runs after the body is finalized so entities never displace an
        // atom or section — see `trim_kg_entities_to_budget`.
        //
        // Self-calibrating inclusion floor: an entity only blends in if its
        // rerank score clears the minimum rerank score among the atoms that
        // actually made the final body. A compose whose final body has zero
        // atoms (everything trimmed by `max_tokens`) has no floor to
        // calibrate against, so it blends no entities at all (ADR-051
        // Amendment 1, zero-atom edge case).
        let entity_score_floor: Option<f32> = included_atom_ids
            .iter()
            .filter_map(|id| atom_cosine_scores.get(id).copied())
            .fold(None, |acc, s| Some(acc.map_or(s, |a: f32| a.min(s))));
        let mut markdown = markdown;
        let mut kg_entities_json: Vec<Value> = Vec::new();
        if blend_kg {
            if let Some(floor) = entity_score_floor {
                // Discovery/hydration failures degrade to an atom-only
                // response instead of aborting the whole compose — the
                // finalized atom/section body above is still a valid,
                // useful briefing even without the supplementary KG section.
                // KG entities live on the core (main) backend; on a
                // secondary-assigned pack runtime this search would silently
                // blend against an empty graph (ADR-073).
                match search_kg_entities(
                    &runtime.core(),
                    token,
                    &ns,
                    &raw_query,
                    KG_BLEND_CAP,
                    floor,
                )
                .await
                {
                    Ok(kg_hits) => {
                        let remaining_budget = char_budget.saturating_sub(body_used);
                        let kg_hits = trim_kg_entities_to_budget(kg_hits, remaining_budget);
                        if !kg_hits.is_empty() {
                            markdown.push_str(&format_kg_entities_markdown(&kg_hits));
                            kg_entities_json = kg_hits
                                .iter()
                                .map(|e| {
                                    json!({
                                        "id": e.id,
                                        "kind": e.kind,
                                        "name": e.name,
                                        "score": (e.score * 10000.0).round() / 10000.0,
                                    })
                                })
                                .collect();
                        }
                    }
                    Err(e) => {
                        try_or_finish!(khive_storage::ensure_request_read_active(
                            "knowledge.compose"
                        ));
                        tracing::warn!(
                            error = %e,
                            "knowledge.compose: KG entity blend failed, continuing with atom-only response"
                        );
                    }
                }
            }
        }

        try_or_finish!(khive_storage::ensure_request_read_active(
            "knowledge.compose"
        ));

        let atom_json: Vec<Value> = items
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "slug": item.slug,
                    "name": item.name,
                    "score": (item.score * 10000.0).round() / 10000.0,
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
        if explain && !section_json.is_empty() {
            data["sections"] = json!(section_json);
            data["section_count"] = json!(section_json.len());
        }
        if !kg_entities_json.is_empty() {
            data["entities"] = json!(kg_entities_json);
        }
        if suggest_ann_unavailable {
            data["ann_unavailable"] = json!(true);
        }
        attach_hydration_degradation(&mut data, suggest_hydration_failures);

        let response = json!({
            "status": "ok",
            "data": data,
        });
        try_or_finish!(khive_storage::ensure_request_read_active(
            "knowledge.compose"
        ));
        timing.finish(count);
        Ok(response)
    }
}

/// Seeds `n` atoms whose content each carries exactly one term from a
/// `vocab_size`-word vocabulary (`term0`..`term{vocab_size-1}`), so an
/// OR-joined query over `k` of those terms matches roughly `k/vocab_size`
/// of the corpus while any single term matches roughly `1/vocab_size` — the
/// same low-overlap shape that makes the OR-joined bm25 sort in the
/// pre-#1930 query cost far more than any one term's bounded subquery.
/// `pub(crate)` (not scoped to `mod tests` below) so the handler-level
/// degrade tests in `ann_degrade_tests.rs` can reuse the same corpus shape.
#[cfg(test)]
pub(crate) async fn seed_low_overlap_corpus(runtime: &KhiveRuntime, n: u32, vocab_size: u32) {
    let y_stride: u32 = 100;
    assert_eq!(
        n % y_stride,
        0,
        "seed_low_overlap_corpus requires n a multiple of 100"
    );
    let x_max = n / y_stride - 1;
    let y_max = y_stride - 1;

    let access = runtime.sql();
    let mut writer = access.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: format!(
                "WITH RECURSIVE x(n) AS ( \
                     VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < {x_max} \
                 ), y(n) AS ( \
                     VALUES(0) UNION ALL SELECT n + 1 FROM y WHERE n < {y_max} \
                 ) \
                 INSERT INTO knowledge_atoms ( \
                     id, namespace, slug, name, content, tags, properties, finalized, \
                     status, source_uri, source_type, created_at, updated_at, deleted_at \
                 ) \
                 SELECT \
                     printf('80000000-0000-0000-0000-%012d', x.n * {y_stride} + y.n), \
                     'local', printf('lowoverlap-%06d', x.n * {y_stride} + y.n), \
                     printf('Low Overlap %06d', x.n * {y_stride} + y.n), \
                     'synthetic corpus content entry ' || (x.n * {y_stride} + y.n) || \
                     ' discusses topic term' || ((x.n * {y_stride} + y.n) % {vocab_size}) || \
                     ' with padding context sentence for realistic length and additional filler', \
                     '[]', NULL, 1, 'reviewed', NULL, NULL, \
                     x.n * {y_stride} + y.n, x.n * {y_stride} + y.n, NULL \
                 FROM x CROSS JOIN y WHERE x.n * {y_stride} + y.n < {n}"
            ),
            params: Vec::new(),
            label: None,
        })
        .await
        .expect("seed low-overlap corpus");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compose_direct_handler_rejects_namespace_token_mismatch() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("local token");
        let ann = vamana::new_shared();

        let err = KnowledgeHandlers::compose(
            &runtime,
            &token,
            json!({
                "namespace": "bench-arm-a",
                "query": "must reject before reading",
            }),
            &ann,
            HashMap::new(),
        )
        .await
        .expect_err("a local token must not elevate into a measurement arm");

        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("does not match authorized token namespace")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn fts_candidate_expression_recalls_non_contiguous_terms() {
        assert_eq!(
            fts5_candidate_expression("alpha beta alpha and"),
            "\"alpha\" OR \"alphas\" OR \"beta\" OR \"betas\""
        );
        assert_eq!(fts5_candidate_expression("RAG"), "\"rag\" OR \"rags\"");
        assert_eq!(
            fts5_candidate_expression("the and"),
            "\"the and\"",
            "stop-only queries retain the exact-phrase fallback"
        );
    }

    /// Issue #1930: the old OR-joined query returned an all-or-nothing error
    /// when it crossed the request deadline. The per-term fetch must instead
    /// keep candidates from a completed term and report `timed_out`. Paused
    /// Tokio time and the test-only per-term deadline control place expiry
    /// exactly between two queries, so the assertion never depends on corpus
    /// work taking longer than a machine-specific wall-clock budget.
    ///
    /// Scope: this covers the per-term BOUNDARY only. Advancing only the
    /// async clock means the post-boundary term is refused by the
    /// pre-registration deadline check, never by an in-flight SQLite
    /// interrupt — that path has its own wall-clock test below. The old
    /// wall-clock old-query oracle (running the pre-fix OR-joined SQL over
    /// this corpus against a tuned budget) is deliberately retired with the
    /// machine-timing flake it depended on; the all-or-nothing behavior of a
    /// single statement crossing its deadline is what the in-flight test
    /// below pins.
    #[tokio::test(start_paused = true)]
    async fn per_term_fetch_degrades_at_controlled_deadline_boundary() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        const N: u32 = 1_000;
        const VOCAB: u32 = 20;
        seed_low_overlap_corpus(&runtime, N, VOCAB).await;

        let query = "term0 term1";
        let deadline = std::time::Duration::from_millis(650);

        let new_result = khive_storage::scope_request_read_deadline(
            deadline,
            FTS_TEST_DEADLINE_ADVANCE.scope(
                FtsTestDeadlineAdvance {
                    after_completed_terms: 1,
                    by: deadline,
                },
                fetch_fts_candidates(&runtime, "local", query, None, &[], &[], CANDIDATE_POOL),
            ),
        )
        .await;
        let outcome = new_result.expect(
            "the per-term fetch must return partial degradation when the controlled deadline \
             expires between term queries",
        );
        assert!(
            outcome.timed_out,
            "the controlled deadline must be observed"
        );
        assert!(
            !outcome.atoms.is_empty(),
            "candidates from the completed term must survive degradation"
        );
        assert!(
            outcome.atoms.len() <= CANDIDATE_POOL,
            "partial pool must respect the fetch cap; got {}",
            outcome.atoms.len()
        );
    }

    /// Companion to the boundary test above: prove that a wall-clock
    /// deadline expiring during this pack's read path surfaces the typed
    /// `StorageError::Timeout` — never an untyped error — end to end through
    /// the runtime's reader surface. The statement is structurally slow (a
    /// 1000^3 cross join — billions of row operations on any machine), so
    /// the deadline expires long before it could complete.
    ///
    /// Scope: which arm of the deadline machinery fires is scheduling-
    /// dependent — the deadline can latch at reader checkout, at read
    /// registration, or mid-statement via the progress handler — and every
    /// arm must yield the same typed timeout, which is exactly this test's
    /// assertion. The mid-statement arm specifically (progress-handler
    /// interrupt of an executing statement, proven by a probe that counts
    /// progress callbacks) is deterministically covered where the mechanism
    /// lives: `request_deadline_interrupts_statement_without_outer_timeout`
    /// in `crates/khive-db/src/sql_bridge.rs`, whose probe assertion fails
    /// if SQLite work never started. This test does not re-prove the
    /// in-flight arm; it pins the pack-visible contract over all arms.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wall_clock_deadline_on_pack_read_path_surfaces_typed_timeout() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let deadline = std::time::Duration::from_millis(50);
        let result = khive_storage::scope_request_read_deadline(deadline, async {
            let sql = runtime.sql();
            let mut reader = sql.reader().await.expect("reader");
            reader
                .query_all(SqlStatement {
                    sql: "WITH RECURSIVE numbers(value) AS (\
                          SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000\
                          ) SELECT SUM(a.value * b.value * c.value) \
                          FROM numbers AS a CROSS JOIN numbers AS b CROSS JOIN numbers AS c"
                        .into(),
                    params: vec![],
                    label: Some("knowledge-deadline-probe".into()),
                })
                .await
        })
        .await;
        assert!(
            matches!(result, Err(khive_storage::StorageError::Timeout { .. })),
            "a read crossing the wall-clock deadline must surface the typed \
             timeout whichever deadline arm fires; got {result:?}"
        );
    }

    /// Issue #1930 rework: the per-term loop used to `break` as soon as the
    /// merged pool reached `fetch_limit`, checked *before* querying the next
    /// term. A term that sorts first and alone has more matches than
    /// `fetch_limit` then fills the pool on its own turn, so every later
    /// term never gets queried at all — pool membership depended on query
    /// word order. This seeds "alpha" with more rows than `fetch_limit` and
    /// a disjoint, small "beta" set, then asserts beta's rows survive into
    /// the returned pool. Against the pre-fix early-break loop this must
    /// FAIL: alpha's own per-term query (capped at `fetch_limit`) already
    /// fills `combined` to `fetch_limit` before the loop reaches "beta", so
    /// "beta" is never queried and none of its rows can appear.
    #[tokio::test]
    async fn round_robin_merge_keeps_later_term_candidates_from_starving() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        {
            let access = runtime.sql();
            let mut writer = access.writer().await.expect("writer");
            writer
                .execute(SqlStatement {
                    sql: "WITH RECURSIVE x(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 9 \
                          ) \
                          INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, properties, finalized, \
                              status, source_uri, source_type, created_at, updated_at, deleted_at \
                          ) \
                          SELECT \
                              printf('90000000-0000-0000-0000-%012d', x.n), \
                              'local', printf('alpha-%02d', x.n), printf('Alpha %02d', x.n), \
                              'synthetic content about alpha only', '[]', NULL, 1, \
                              'reviewed', NULL, NULL, x.n, x.n, NULL \
                          FROM x"
                        .to_string(),
                    params: Vec::new(),
                    label: None,
                })
                .await
                .expect("seed alpha rows");
            writer
                .execute(SqlStatement {
                    sql: "WITH RECURSIVE x(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 2 \
                          ) \
                          INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, properties, finalized, \
                              status, source_uri, source_type, created_at, updated_at, deleted_at \
                          ) \
                          SELECT \
                              printf('91000000-0000-0000-0000-%012d', x.n), \
                              'local', printf('beta-%02d', x.n), printf('Beta %02d', x.n), \
                              'synthetic content about beta only', '[]', NULL, 1, \
                              'reviewed', NULL, NULL, x.n, x.n, NULL \
                          FROM x"
                        .to_string(),
                    params: Vec::new(),
                    label: None,
                })
                .await
                .expect("seed beta rows");
        }

        let fetch_limit = 5;
        let outcome =
            fetch_fts_candidates(&runtime, "local", "alpha beta", None, &[], &[], fetch_limit)
                .await
                .expect("fetch must not error");
        assert!(!outcome.timed_out);
        assert_eq!(outcome.atoms.len(), fetch_limit);

        let beta_present = outcome
            .atoms
            .iter()
            .any(|atom| atom.slug.starts_with("beta-"));
        assert!(
            beta_present,
            "round-robin merge must keep the second term's candidates in the pool \
             even though the first term alone has more matches than fetch_limit; \
             got {:?}",
            outcome
                .atoms
                .iter()
                .map(|a| a.slug.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn missing_ann_hydration_is_dropped_and_reported() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let mut hits = vec![ScoredHit {
            id: "00000000-0000-0000-0000-000000001763".to_string(),
            slug: String::new(),
            name: String::new(),
            content: None,
            tags: None,
            finalized: false,
            is_domain: false,
            status: None,
            score: 0.8,
        }];

        let failures = hydrate_empty_hits(&runtime, "local", &mut hits).await;
        assert_eq!(failures, 1);
        assert!(hits.is_empty(), "unhydrated shells must never be returned");

        let mut response = json!({"results": [], "total": 0});
        attach_hydration_degradation(&mut response, failures);
        assert_eq!(response["degraded"]["hydration_failures"], 1);
    }

    #[test]
    fn zero_hydration_failures_do_not_change_the_response() {
        let mut response = json!({"results": [], "total": 0});
        attach_hydration_degradation(&mut response, 0);
        assert!(response.get("degraded").is_none());
    }

    #[test]
    fn hydration_degradation_preserves_existing_diagnostics() {
        let mut response = json!({
            "results": [],
            "total": 0,
            "degraded": {
                "reason": "ann_unavailable",
                "cache_safe": false,
            }
        });
        attach_hydration_degradation(&mut response, 7);
        assert_eq!(response["degraded"]["reason"], "ann_unavailable");
        assert_eq!(response["degraded"]["cache_safe"], false);
        assert_eq!(response["degraded"]["hydration_failures"], 7);
    }

    #[tokio::test]
    async fn hydration_chunks_candidate_sets_above_sqlite_bind_ceiling() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        {
            let access = runtime.sql();
            let mut writer = access.writer().await.expect("writer");
            writer
                .execute(SqlStatement {
                    sql: "WITH RECURSIVE x(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < 20 \
                          ), y(n) AS ( \
                              VALUES(0) UNION ALL SELECT n + 1 FROM y WHERE n < 49 \
                          ) \
                          INSERT INTO knowledge_atoms ( \
                              id, namespace, slug, name, content, tags, properties, finalized, \
                              status, source_uri, source_type, created_at, updated_at, deleted_at \
                          ) \
                          SELECT \
                              printf('70000000-0000-0000-0000-%012d', x.n * 50 + y.n), \
                              'local', printf('hydrate-%04d', x.n * 50 + y.n), \
                              printf('Hydrate %04d', x.n * 50 + y.n), 'hydration content', \
                              '[]', NULL, 1, 'reviewed', NULL, NULL, \
                              x.n * 50 + y.n, x.n * 50 + y.n, NULL \
                          FROM x CROSS JOIN y WHERE x.n * 50 + y.n < 1005"
                        .to_string(),
                    params: Vec::new(),
                    label: None,
                })
                .await
                .expect("seed hydration rows");
        }

        let mut hits: Vec<ScoredHit> = (0..1005)
            .map(|i| ScoredHit {
                id: format!("70000000-0000-0000-0000-{i:012}"),
                slug: String::new(),
                name: String::new(),
                content: None,
                tags: None,
                finalized: false,
                is_domain: false,
                status: None,
                score: 1.0,
            })
            .collect();

        let failures = hydrate_empty_hits(&runtime, "local", &mut hits).await;
        assert_eq!(failures, 0);
        assert_eq!(hits.len(), 1005);
        assert!(hits.iter().all(|hit| hit.slug.starts_with("hydrate-")));
    }

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
    fn explicit_status_is_an_exact_allowlist_for_hydrated_hits() {
        let mut hits = vec![
            make_hit("reviewed", Some("reviewed"), 0.9),
            make_hit("draft", Some("draft"), 0.8),
            make_hit("deprecated", Some("deprecated"), 0.7),
            make_hit("missing-status", None, 0.6),
        ];
        filter_hits_by_status(&mut hits, &["draft".to_string()], &[]);
        let ids: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, ["draft"]);
    }

    #[test]
    fn deprecated_multiplier_gate_uses_resolved_status_policy() {
        assert!(!deprecated_allowed_by_status_policy(
            &[],
            &["draft", "deprecated"]
        ));
        assert!(!deprecated_allowed_by_status_policy(&[], &["deprecated"]));
        assert!(deprecated_allowed_by_status_policy(&[], &["reviewed"]));
        assert!(!deprecated_allowed_by_status_policy(
            &["reviewed".to_string()],
            &[]
        ));
        assert!(deprecated_allowed_by_status_policy(
            &["deprecated".to_string()],
            &[]
        ));
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

    /// Fusion-admitted hit whose score the status multiplier squashes below
    /// `min_score` must not survive the late floor. Reproduction arithmetic:
    /// a single-source RRF top hit normalizes to 1.0, then `s/(s+1)` with
    /// multiplier 1.0 squashes it to 0.5 — below a 0.7 floor.
    #[test]
    fn min_score_floor_drops_hit_squashed_below_threshold_by_status_multiplier() {
        let mut hits = vec![make_hit("atom-1", Some("reviewed"), 0.0)];
        fuse_ann_hits(&mut hits, &[], 0.7);
        assert_eq!(hits.len(), 1, "fusion stage must admit the 1.0 RRF hit");
        assert_eq!(hits[0].score, 1.0);

        apply_status_multipliers(&mut hits, false);
        assert!((hits[0].score - 0.5).abs() < 1e-6, "1.0 squashes to 0.5");

        enforce_min_score_floor(&mut hits, 0.7);
        assert!(
            hits.is_empty(),
            "0.5 post-multiplier score must not clear a 0.7 floor"
        );
    }

    #[test]
    fn min_score_floor_keeps_hit_at_or_above_threshold_after_multiplier() {
        let mut hits = vec![make_hit("atom-1", Some("reviewed"), 0.0)];
        fuse_ann_hits(&mut hits, &[], 0.4);
        assert_eq!(hits.len(), 1, "fusion stage must admit the 1.0 RRF hit");

        apply_status_multipliers(&mut hits, false);
        let squashed = hits[0].score;

        enforce_min_score_floor(&mut hits, 0.4);
        assert_eq!(
            hits.len(),
            1,
            "0.5 post-multiplier score clears a 0.4 floor"
        );
        assert_eq!(hits[0].id, "atom-1");
        assert!(hits[0].score >= 0.4);
        assert_eq!(hits[0].score, squashed, "floor must not rewrite scores");
    }

    /// `min_score = 0.0` (the absent default) must return the identical set —
    /// the floor cannot alter the no-threshold path.
    #[test]
    fn min_score_floor_zero_is_noop_on_multiplier_survivors() {
        let build = || {
            let mut hits = vec![
                make_hit("atom-1", Some("reviewed"), 0.9),
                make_hit("atom-2", Some("draft"), 0.6),
                make_hit("atom-3", Some("deprecated"), 0.8),
            ];
            apply_status_multipliers(&mut hits, false);
            hits
        };

        let before = build();
        let mut after = build();
        enforce_min_score_floor(&mut after, 0.0);

        let ids_before: Vec<&str> = before.iter().map(|h| h.id.as_str()).collect();
        let ids_after: Vec<&str> = after.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids_after, ids_before);
        for (a, b) in after.iter().zip(before.iter()) {
            assert_eq!(a.score, b.score);
        }
    }

    /// Body-line metadata is best-effort: a read-deadline timeout during the
    /// aggregate lookup degrades to `Ok(None)` (rendered as `body_lines:
    /// null` plus a degradation flag by the handler) instead of failing an
    /// already-ranked search with an `Internal` error.
    #[tokio::test]
    async fn body_line_counts_degrade_to_none_under_expired_read_deadline() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let atom_ids = vec!["10000000-0000-0000-0000-000000000001".to_owned()];

        let degraded = khive_storage::scope_request_read_deadline(
            std::time::Duration::ZERO,
            load_atom_body_line_counts(&runtime, "local", &atom_ids),
        )
        .await;
        assert!(
            matches!(degraded, Ok(None)),
            "an expired read deadline must degrade body-line metadata to \
             None, never error the search; got {degraded:?}"
        );

        let healthy = load_atom_body_line_counts(&runtime, "local", &atom_ids)
            .await
            .expect("undeadlined lookup must succeed");
        assert_eq!(
            healthy.and_then(|counts| counts.get(&atom_ids[0]).copied()),
            Some(0),
            "control: without a deadline the lookup returns real counts"
        );
    }
}
