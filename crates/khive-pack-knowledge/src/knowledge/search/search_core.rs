use std::collections::{HashMap, HashSet};

use khive_runtime::{KhiveRuntime, RuntimeError};

use super::super::lexical_timeout::{LexicalPass, LexicalStage, LexicalTimeout};
use super::super::matching;
use super::super::scoring::{
    compute_idf, exact_name_bonus, expand_terms, load_candidates_from_atoms, score_candidate,
    Candidate, Weights,
};
use super::super::util::{is_stop, CANDIDATE_POOL, MIN_TERM_LEN};
use super::{
    fetch_fts_candidates, lexical_stage_budget, lexical_stage_budget_for_terms, FtsFetchOutcome,
    FtsTermBudget, LexicalCandidateState, ScoreProvenance, ScoredHit,
};

// ─── search context ───────────────────────────────────────────────────────────

pub(super) struct SearchCtx<'a> {
    pub(super) runtime: &'a KhiveRuntime,
    pub(super) ns: &'a str,
    pub(super) role: Option<&'a str>,
    pub(super) type_filter: Option<&'a str>,
    pub(super) min_score: f32,
    pub(super) w: &'a Weights,
    pub(super) fetch_limit: usize,
    pub(super) statuses: &'a [String],
    pub(super) exclude_statuses: &'a [&'a str],
    pub(super) term_budget: &'a FtsTermBudget,
}

// ─── core single-pass search ──────────────────────────────────────────────────

/// `search_core`'s result plus any lexical/FTS read timeout diagnostics.
/// A caller sees `hits` possibly empty/partial and timeout details instead of an
/// `Err` — never a verb-level error for a genuine timeout.
pub(super) struct SearchCoreOutcome {
    pub(super) hits: Vec<ScoredHit>,
    pub(super) lexical_timeouts: Vec<LexicalTimeout>,
    pub(super) lexical_state: LexicalCandidateState,
}

pub(super) async fn search_core(
    ctx: &SearchCtx<'_>,
    query: &str,
    pass: LexicalPass,
) -> Result<SearchCoreOutcome, RuntimeError> {
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
            lexical_timeouts: Vec::new(),
            lexical_state: LexicalCandidateState::NoMatch,
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

    // The lexical stage owns an independent, narrower read-deadline budget
    // (issue #1930 Amendment 2): `scope_request_read_deadline` keeps
    // whichever deadline is earlier, so this only ever tightens the fetch's
    // effective deadline; once the fetch returns, the wider request
    // deadline governs everything that runs after it (rerank, body-line
    // counts, member sizing) — a lexical-stage timeout no longer spends the
    // whole request.
    let configured_budget = lexical_stage_budget_for_terms(
        lexical_stage_budget(),
        ctx.term_budget.available_for(&raw_query),
    );
    let stage_started = tokio::time::Instant::now();
    let FtsFetchOutcome {
        atoms,
        timeout,
        state: lexical_state,
    } = khive_storage::scope_request_read_deadline(configured_budget, async {
        let stage = LexicalStage::new(pass, stage_started, configured_budget);
        fetch_fts_candidates(
            runtime,
            ns,
            &raw_query,
            type_filter,
            ctx.statuses,
            ctx.exclude_statuses,
            CANDIDATE_POOL,
            ctx.term_budget,
            stage,
        )
        .await
    })
    .await?;
    let lexical_timeouts: Vec<_> = timeout.into_iter().collect();
    if atoms.is_empty() {
        return Ok(SearchCoreOutcome {
            hits: Vec::new(),
            lexical_timeouts,
            lexical_state,
        });
    }

    let candidates = load_candidates_from_atoms(&atoms, type_filter);
    if candidates.is_empty() {
        return Ok(SearchCoreOutcome {
            hits: Vec::new(),
            lexical_timeouts,
            lexical_state: if lexical_state == LexicalCandidateState::PartialTimeout {
                lexical_state
            } else {
                LexicalCandidateState::Filtered
            },
        });
    }

    let idf = compute_idf(&candidates, &terms, &expanded, w.expand_discount);
    let mut scored: Vec<(f32, &Candidate)> = candidates
        .iter()
        .filter_map(|cand| {
            let base = if lexical_state == LexicalCandidateState::ExactName {
                w.w_exact_name
            } else if terms_only_exact {
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
                atom_embed_text: cand.atom_embed_text.clone(),
                status: cand.status_raw.clone(),
                finalized: cand.finalized,
                is_domain: cand.is_domain,
                score,
                provenance: ScoreProvenance::lexical(),
            })
            .collect(),
        lexical_timeouts,
        lexical_state,
    })
}

// ─── decomposed search ───────────────────────────────────────────────────────

pub(super) async fn search_decomposed(
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
        mut lexical_timeouts,
        lexical_state: full_state,
    } = search_core(ctx, query, LexicalPass::Full).await?;
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
        term_budget: ctx.term_budget,
    };
    let SearchCoreOutcome {
        hits: s1,
        lexical_timeouts: s1_timeouts,
        lexical_state: s1_state,
    } = search_core(&sub_ctx1, &sub_q1, LexicalPass::Subquery1).await?;
    let SearchCoreOutcome {
        hits: s2,
        lexical_timeouts: s2_timeouts,
        lexical_state: s2_state,
    } = search_core(&sub_ctx1, &sub_q2, LexicalPass::Subquery2).await?;
    lexical_timeouts.extend(s1_timeouts);
    lexical_timeouts.extend(s2_timeouts);

    Ok(SearchCoreOutcome {
        hits: merge_decomposed_hits(full, [s1, s2], intersection_bonus, ctx.fetch_limit),
        lexical_timeouts,
        lexical_state: LexicalCandidateState::merge(&[full_state, s1_state, s2_state]),
    })
}

pub(super) fn merge_decomposed_hits(
    full: Vec<ScoredHit>,
    subqueries: [Vec<ScoredHit>; 2],
    intersection_bonus: f32,
    fetch_limit: usize,
) -> Vec<ScoredHit> {
    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut data: HashMap<String, ScoredHit> = HashMap::new();

    for mut hit in full {
        scores.insert(hit.id.clone(), hit.score);
        if let Some(existing) = data.get(&hit.id) {
            hit.provenance.merge_sources(existing.provenance);
        }
        data.insert(hit.id.clone(), hit);
    }

    let mut sub_counts: HashMap<String, u32> = HashMap::new();
    for hits in subqueries {
        let mut seen: HashSet<String> = HashSet::new();
        for hit in hits {
            if let Some(existing) = data.get_mut(&hit.id) {
                existing.provenance.merge_sources(hit.provenance);
            }
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
    ranked.truncate(fetch_limit);
    ranked
}
