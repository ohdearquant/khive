//! Search, suggest, and compose handlers.
//!
//! TF-IDF scoring primitives live in `super::scoring`; this module owns the
//! FTS/ANN pipeline, reranking, hydration, and handler dispatch.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    hex_prefix_to_uuid_pattern, KhiveRuntime, Namespace, NamespaceToken, RuntimeError,
};
use khive_score::DeterministicScore;
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_storage::EntityFilter;

use super::lexical_timeout::{
    LexicalBound, LexicalPass, LexicalPhase, LexicalStage, LexicalTimeout,
};
use super::matching;
use super::schema::{Atom, ComposeParams, Domain, SearchParams, SuggestParams};
use super::scoring::{
    compute_idf, exact_name_bonus, expand_terms, load_candidates_from_atoms, score_candidate,
    Candidate, Weights,
};
use super::sections::to_slug;
use super::util::{
    atom_embed_text, atom_embed_text_fields, atom_from_row, deser, domain_from_row,
    estimate_compose_item_tokens, explicitly_requested_status, is_stop, row_bool, row_i64, row_str,
    sql_err, status_multiplier, status_sql_clause, status_values, CANDIDATE_POOL, CHARS_PER_TOKEN,
    D_SUGGEST_RERANK_ALPHA, MIN_TERM_LEN, SERVABLE_SECTION,
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
    /// The index-time atom renderer, independent of the result's display tags.
    atom_embed_text: Option<String>,
    finalized: bool,
    is_domain: bool,
    status: Option<String>,
    score: f32,
    provenance: ScoreProvenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScoreProvenance {
    lexical: bool,
    ann: bool,
    embedding_rerank: bool,
}

impl ScoreProvenance {
    const fn lexical() -> Self {
        Self {
            lexical: true,
            ann: false,
            embedding_rerank: false,
        }
    }

    const fn ann() -> Self {
        Self {
            lexical: false,
            ann: true,
            embedding_rerank: false,
        }
    }

    fn merge_sources(&mut self, other: Self) {
        self.lexical |= other.lexical;
        self.ann |= other.ann;
        self.embedding_rerank |= other.embedding_rerank;
    }

    fn to_json(self) -> Value {
        let mut sources = Vec::with_capacity(2);
        if self.lexical {
            sources.push("lexical");
        }
        if self.ann {
            sources.push("ann");
        }
        json!({
            "sources": sources,
            "embedding_rerank": self.embedding_rerank,
            "normalization": "s_over_s_plus_1",
            "calibrated": false,
        })
    }
}

fn candidate_fallback(hits: &[ScoredHit]) -> &'static str {
    if hits.iter().any(|hit| hit.provenance.ann) && !hits.iter().any(|hit| hit.provenance.lexical) {
        "ann"
    } else {
        "none"
    }
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
            hits: vamana::merge_fresh_tail_off_thread(candidates, query_embedding, ops).await,
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
        by_id
            .entry(hit.id.clone())
            .and_modify(|existing| existing.provenance.merge_sources(hit.provenance))
            .or_insert_with(|| hit.clone());
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
/// Mirrors eligibility in the FTS/SQL path in `fetch_fts_candidates`:
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
/// term. Queries with no scoreable term fall back to the exact raw phrase.
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

// The #3514 experiment substitutes only the FTS access path. The ordinary
// production build always uses `fts_knowledge`; the feature-gated test scopes
// two temporary index shapes around the same real knowledge.search dispatch.
#[cfg(all(test, feature = "namespace-trigram-proto"))]
#[derive(Clone)]
enum NamespaceTrigramExperiment {
    SlotTable { key: String },
    Prefixed { key: String },
}

#[cfg(all(test, feature = "namespace-trigram-proto"))]
tokio::task_local! {
    static NAMESPACE_TRIGRAM_EXPERIMENT: NamespaceTrigramExperiment;
}

#[cfg(all(test, feature = "namespace-trigram-proto"))]
fn prototype_fts_target(term: &str) -> Option<(&'static str, String)> {
    NAMESPACE_TRIGRAM_EXPERIMENT
        .try_with(Clone::clone)
        .ok()
        .map(|experiment| match experiment {
            NamespaceTrigramExperiment::SlotTable { key } => (
                "fts_knowledge",
                format!(
                    "namespace_key : {} AND {{slug name content}} : {term}",
                    quote_fts5_phrase(&key)
                ),
            ),
            NamespaceTrigramExperiment::Prefixed { key } => {
                // Every input term came from quote_fts5_phrase above. Decode
                // that single phrase before letting the prototype helper
                // quote the trusted namespace envelope and the raw text.
                let raw = term
                    .strip_prefix('"')
                    .and_then(|term| term.strip_suffix('"'))
                    .expect("candidate terms are quoted FTS5 phrases")
                    .replace("\"\"", "\"");
                (
                    "fts_knowledge_namespace_proto",
                    khive_db::namespace_trigram_proto::scoped_match(&key, &raw)
                        .expect("the fixture uses a valid namespace key"),
                )
            }
        })
}

/// SQL eligibility predicate for the public atom/domain kind filter.
///
/// Domain mirrors are atoms carrying the exact `type:domain` tag. Applying
/// this predicate in FTS hydration and recovery is load-bearing: filtering after
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

/// Per-term candidate cap. FTS enumerates in rowid order and stops at the
/// limit; application TF-IDF scoring ranks only the admitted candidates.
const FTS_TERM_LIMIT: usize = 500;

/// Shared by the full query and both decomposed passes. Admission happens
/// before rarity probes; every later stage reuses that admitted term set.
const FTS_TERM_COUNT_LIMIT: usize = 32;

struct FtsTermBudget {
    remaining: AtomicUsize,
    truncated: AtomicBool,
}

impl FtsTermBudget {
    fn new() -> Self {
        Self {
            remaining: AtomicUsize::new(FTS_TERM_COUNT_LIMIT),
            truncated: AtomicBool::new(false),
        }
    }

    fn admit(&self, mut terms: Vec<String>) -> Vec<String> {
        let remaining = self
            .remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                Some(remaining.saturating_sub(terms.len()))
            })
            .expect("term reservation always succeeds");
        if terms.len() > remaining {
            self.truncated.store(true, Ordering::Relaxed);
            terms.truncate(remaining);
        }
        terms
    }

    fn truncated(&self) -> bool {
        self.truncated.load(Ordering::Relaxed)
    }

    /// Count only terms this pass can actually admit. Decomposed passes share
    /// the request allowance, so a pass after exhaustion must not acquire a
    /// larger deadline just because its raw query contains many terms.
    fn available_for(&self, raw_query: &str) -> usize {
        fts5_candidate_terms(raw_query)
            .len()
            .min(self.remaining.load(Ordering::Relaxed))
    }
}

fn phase_a_rowids_statement(term: &str, limit: usize) -> SqlStatement {
    #[cfg(all(test, feature = "namespace-trigram-proto"))]
    if let Some((table, scoped_term)) = prototype_fts_target(term) {
        return SqlStatement {
            sql: format!(
                "SELECT rowid FROM {table} WHERE {table} MATCH ?1 \
                 ORDER BY rowid LIMIT ?2"
            ),
            params: vec![SqlValue::Text(scoped_term), SqlValue::Integer(limit as i64)],
            label: Some("knowledge.fts_rowids".into()),
        };
    }
    SqlStatement {
        sql: "SELECT rowid FROM fts_knowledge WHERE fts_knowledge MATCH ?1 \
              ORDER BY rowid LIMIT ?2"
            .into(),
        params: vec![SqlValue::Text(term.into()), SqlValue::Integer(limit as i64)],
        label: Some("knowledge.fts_rowids".into()),
    }
}

fn term_frequency_statement(term: &str) -> SqlStatement {
    #[cfg(all(test, feature = "namespace-trigram-proto"))]
    if let Some((table, scoped_term)) = prototype_fts_target(term) {
        return SqlStatement {
            sql: format!(
                "SELECT count(*) AS frequency FROM ( \
                     SELECT rowid FROM {table} WHERE {table} MATCH ?1 \
                     ORDER BY rowid LIMIT ?2 \
                 )"
            ),
            params: vec![
                SqlValue::Text(scoped_term),
                SqlValue::Integer((FTS_TERM_LIMIT + 1) as i64),
            ],
            label: Some("knowledge.fts_term_frequency".into()),
        };
    }
    SqlStatement {
        sql: "SELECT count(*) AS frequency FROM ( \
                  SELECT rowid FROM fts_knowledge WHERE fts_knowledge MATCH ?1 \
                  ORDER BY rowid LIMIT ?2 \
              )"
        .into(),
        params: vec![
            SqlValue::Text(term.into()),
            SqlValue::Integer((FTS_TERM_LIMIT + 1) as i64),
        ],
        label: Some("knowledge.fts_term_frequency".into()),
    }
}

async fn rarest_fts_terms_first(
    reader: &mut dyn khive_storage::SqlReader,
    terms: Vec<String>,
    stage: &mut LexicalStage,
) -> Result<Vec<String>, khive_storage::StorageError> {
    let mut frequencies = Vec::with_capacity(terms.len());
    // The index has exactly one reader, the `cfg(test)` seam at the bottom of
    // this loop, so in a non-test build it genuinely has none and clippy says
    // so. The exemption is scoped to that configuration rather than written as
    // a bare allow, and the alternatives were both worse: a hand-rolled counter
    // is an unused assignment in a non-test build and an explicit counter loop
    // in a test one, and dropping the index removes the seam.
    #[cfg_attr(not(test), allow(clippy::unused_enumerate_index))]
    for (_probe_index, term) in terms.into_iter().enumerate() {
        // Count only a bounded index prefix. Rare counts are exact; terms
        // above the cap tie by spelling, without scanning their whole lists.
        // Aggregating inside SQLite avoids materializing up to 501 owned
        // SqlRows just to discard their rowids and count them in Rust.
        let rows = stage
            .read_bounded(
                LexicalPhase::TermFrequency,
                LexicalBound::OrderingProbe,
                // The caller scoped this same budget as the read deadline;
                // both sides call `rarity_probe_budget()` so they cannot
                // disagree, and recording it here is what lets the timeout
                // record name the 500 ms that governed the read instead of
                // the stage's 2000 ms (issue #2879).
                rarity_probe_budget(),
                reader.query_all(term_frequency_statement(&term)),
            )
            .await?;
        let frequency = rows
            .first()
            .and_then(|row| row_i64(row, "frequency"))
            .ok_or_else(|| khive_storage::StorageError::Serialization {
                capability: khive_storage::StorageCapability::Sql,
                message: "term frequency probe returned no integer count".into(),
            })?;
        if frequency > 0 {
            frequencies.push((term, frequency));
        }
        #[cfg(test)]
        advance_fts_test_probe_deadline_after_term(_probe_index + 1).await;
    }
    frequencies
        .sort_unstable_by(|(a, a_count), (b, b_count)| a_count.cmp(b_count).then_with(|| a.cmp(b)));
    Ok(frequencies.into_iter().map(|(term, _)| term).collect())
}

/// Overfetch factor applied to a term's phase-A rowid window when phase B has
/// an eligibility predicate (status or type) that can reject rows phase A had
/// no way to see (issue #1930 Amendment 2 — see [`fetch_fts_candidates`]).
/// Phase A carries no eligibility predicate at all, so without headroom an
/// ineligible-heavy match set could starve phase B down to nothing even
/// though eligible rows exist further down the rowid sequence.
const PHASE_A_OVERFETCH_FACTOR: usize = 4;

/// Ceiling a single term's phase-A probe window is allowed to widen to
/// (issue #1930 Amendment 2). Bounds the worst case — every phase-A row
/// ineligible, and the corpus large enough to keep returning full pages — to
/// a fixed per-term cost instead of an unbounded retry loop.
const PHASE_A_WIDEN_CEILING: usize = 8000;

/// Independent read-deadline budget for the lexical/FTS candidate fetch
/// (issue #1930 Amendment 2). `khive_storage::scope_request_read_deadline`
/// keeps whichever deadline is earlier, so nesting this around just the
/// fetch only ever *tightens* its effective deadline; popping back out of
/// the scope once the fetch returns restores the wider request deadline for
/// every stage that runs after it (rerank, body-line counts, member
/// sizing). A lexical-stage timeout therefore no longer means the request
/// itself is out of time — only that this one stage's own budget is.
pub(crate) const LEXICAL_STAGE_BUDGET_MS: u64 = 2_000;

/// The fixed 2 s stage allowance leaves the same time for one or many
/// sequential bounded term reads. Give each additional admitted term one
/// quarter of the base budget, capped at 4x for a pass. The request's outer
/// read deadline remains authoritative (normally 30 s), and the 32-term
/// request-wide admission bound still limits actual FTS work.
const LEXICAL_STAGE_EXTRA_TERM_QUARTERS: usize = 12;

fn lexical_stage_budget_for_terms(
    base: std::time::Duration,
    admitted_terms: usize,
) -> std::time::Duration {
    let quarters = 4 + admitted_terms
        .saturating_sub(1)
        .min(LEXICAL_STAGE_EXTRA_TERM_QUARTERS);
    base.saturating_mul(quarters as u32) / 4
}

/// Share of the base lexical budget the rarity probe may spend before it is
/// cut off and the terms are used in the order they arrived. Multi-term
/// candidate fetches gain time; the optional ordering probe does not.
///
/// `rarest_fts_terms_first` fetches no candidates. It issues one bounded
/// `count(*)` per term, sequentially, and its entire product is an ORDERING of
/// the terms the real fetch then queries. Before this bound existed it ran
/// against the whole stage budget, and on a production corpus a five-to-seven
/// term query spent all 2000 ms in it — after which the caller returned an
/// empty candidate list for a stage that had not yet asked for a candidate
/// (issue #2766). An optimization must not be able to consume the budget of
/// the work it optimizes, and its expiry must degrade to the unoptimized path
/// rather than to no path.
///
/// A quarter, rather than a half or a tenth, for two reasons stated so a later
/// change has something to argue against. The probe's value is largest on the
/// first few terms (querying the rarest term first is what bounds the rowid
/// window the later terms widen) and falls off across the tail, so it does not
/// need most of the budget to deliver most of its benefit. And the fetch it
/// precedes is the part that can return nothing useful when it is short of
/// time, so the remainder belongs to the fetch.
const RARITY_PROBE_BUDGET_NUMERATOR: u32 = 1;
const RARITY_PROBE_BUDGET_DENOMINATOR: u32 = 4;

/// The rarity probe's own deadline, derived from the base budget in force
/// (including the test override) rather than from a second constant that
/// could drift away from it.
fn rarity_probe_budget() -> std::time::Duration {
    lexical_stage_budget() / RARITY_PROBE_BUDGET_DENOMINATOR * RARITY_PROBE_BUDGET_NUMERATOR
}

// ── Test-only seam: override the lexical-stage budget and the phase-A widen
// ceiling ──────────────────────────────────────────────────────────────────
//
// Both overrides ride the same request-scoped `tokio::task_local!` mechanism
// `khive_storage::scope_request_read_deadline` already uses for the read
// deadline (issue #2396 fix 4). A task-local override is visible only to the
// task it is scoped around, so two tests running concurrently under Cargo's
// parallel runner can never observe each other's override the way the prior
// process-global `AtomicU64` could — there is nothing shared left to reset
// on drop or serialize with a mutex.
tokio::task_local! {
    static LEXICAL_STAGE_BUDGET_OVERRIDE_MS: u64;
    static PHASE_A_WIDEN_CEILING_OVERRIDE: usize;
}

#[cfg(test)]
tokio::task_local! {
    // Expire only sizing after ANN and lexical candidate work has finished.
    static MEMBER_SIZING_READ_TIMEOUT: ();
}

#[cfg(test)]
pub(crate) async fn with_member_sizing_read_timeout<F: std::future::Future>(
    future: F,
) -> F::Output {
    MEMBER_SIZING_READ_TIMEOUT.scope((), future).await
}

fn lexical_stage_budget() -> std::time::Duration {
    let ms = LEXICAL_STAGE_BUDGET_OVERRIDE_MS
        .try_with(|ms| *ms)
        .unwrap_or(LEXICAL_STAGE_BUDGET_MS);
    std::time::Duration::from_millis(ms)
}

fn phase_a_widen_ceiling() -> usize {
    PHASE_A_WIDEN_CEILING_OVERRIDE
        .try_with(|ceiling| *ceiling)
        .unwrap_or(PHASE_A_WIDEN_CEILING)
}

/// Scope `future` to a lexical-stage budget override, so a test can force a
/// controlled deadline instead of depending on a real multi-second wall-clock
/// wait against the production default (mirrors `vamana::warm_wait_timeout_ms`'s
/// override seam). `pub(crate)` (not scoped to `mod tests` below) so the
/// handler-level degrade tests in `ann_degrade_tests.rs` can reuse it.
#[cfg(test)]
pub(crate) async fn with_lexical_stage_budget_override_ms<F>(ms: u64, future: F) -> F::Output
where
    F: std::future::Future,
{
    LEXICAL_STAGE_BUDGET_OVERRIDE_MS.scope(ms, future).await
}

/// Scope `future` to a phase-A widen-ceiling override, so a test can shrink
/// the ceiling far below `PHASE_A_WIDEN_CEILING` and reach the ceiling-
/// exhaustion fallback (fix 2) with a small fixture.
#[cfg(test)]
pub(crate) async fn with_phase_a_widen_ceiling_override<F>(ceiling: usize, future: F) -> F::Output
where
    F: std::future::Future,
{
    PHASE_A_WIDEN_CEILING_OVERRIDE.scope(ceiling, future).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LexicalCandidateState {
    Matched,
    ExactName,
    NoMatch,
    Filtered,
    PartialTimeout,
    TimedOut,
}

impl LexicalCandidateState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::ExactName => "exact_name",
            Self::NoMatch => "no_match",
            Self::Filtered => "filtered",
            Self::PartialTimeout => "partial_timeout",
            Self::TimedOut => "timed_out",
        }
    }

    fn merge(states: &[Self]) -> Self {
        let timed_out = states
            .iter()
            .filter(|&&state| state == Self::TimedOut)
            .count();
        if !states.is_empty() && timed_out == states.len() {
            Self::TimedOut
        } else if timed_out > 0 || states.contains(&Self::PartialTimeout) {
            Self::PartialTimeout
        } else if states.contains(&Self::Matched) {
            Self::Matched
        } else if states.contains(&Self::ExactName) {
            Self::ExactName
        } else if states.contains(&Self::Filtered) {
            Self::Filtered
        } else {
            Self::NoMatch
        }
    }
}

/// Outcome of the bounded lexical candidate fetch.
///
/// `timeout` is set only for [`khive_storage::StorageError::Timeout`]. Any other storage error
/// (including a genuine FTS5 syntax/parser error) still surfaces as an `Err`;
/// fail-open applies to a timeout only.
struct FtsFetchOutcome {
    atoms: Vec<Atom>,
    timeout: Option<LexicalTimeout>,
    state: LexicalCandidateState,
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

#[cfg(test)]
tokio::task_local! {
    static FTS_TEST_PROBE_DEADLINE_ADVANCE: FtsTestDeadlineAdvance;
}

/// Sibling of [`advance_fts_test_deadline_after_term`] for the rarity probe.
/// A SEPARATE task-local, not a shared counter: the probe loop and the
/// per-term fetch loop are different boundaries, and a test that wants the
/// probe to expire must not also move the fetch's clock.
#[cfg(test)]
async fn advance_fts_test_probe_deadline_after_term(completed_terms: usize) {
    let advance_by = FTS_TEST_PROBE_DEADLINE_ADVANCE
        .try_with(|control| {
            (completed_terms == control.after_completed_terms).then_some(control.by)
        })
        .ok()
        .flatten();
    if let Some(advance_by) = advance_by {
        tokio::time::advance(advance_by).await;
    }
}

#[cfg(test)]
pub(crate) async fn with_fts_probe_deadline_advance_after_term<F>(
    after_completed_terms: usize,
    by: std::time::Duration,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    FTS_TEST_PROBE_DEADLINE_ADVANCE
        .scope(
            FtsTestDeadlineAdvance {
                after_completed_terms,
                by,
            },
            future,
        )
        .await
}

#[cfg(test)]
pub(crate) async fn with_fts_deadline_advance_after_term<F>(
    after_completed_terms: usize,
    by: std::time::Duration,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    FTS_TEST_DEADLINE_ADVANCE
        .scope(
            FtsTestDeadlineAdvance {
                after_completed_terms,
                by,
            },
            future,
        )
        .await
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
/// Two-phase per term (issue #1930 Amendment 2). The original shape ordered
/// a full atom row — `content` included — per FTS5 match *before* its own
/// per-term `LIMIT`, so every match paid a scattered read against the whole
/// (potentially multi-gigabyte) atom table even though only the top few
/// hundred survived. Phase A instead enumerates bare rowids straight off
/// the `fts_knowledge` index, stopping before scoring the full match set;
/// phase B hydrates just the surviving
/// rowids from `knowledge_atoms` in chunks, applying namespace, soft-delete,
/// status, and type eligibility there.
///
/// Phase A carries no namespace predicate. `fts_knowledge.namespace` is
/// UNINDEXED on this external-content FTS5 table, so filtering on it (as the
/// original single query did) forces FTS5 to fetch the backing content row
/// per candidate — exactly the cost phase A exists to avoid. The namespace
/// check moves entirely to phase B, where the atom row is already being
/// read for its other eligibility columns. Overfetch and the scoped ceiling
/// fallback recover eligible rows beyond an ineligible prefix.
///
/// Phase A also drops the `a.slug` tie-break the old query used: `rowid` is
/// already stable and needs no join. `ORDER BY rowid` is consumed by FTS5,
/// so LIMIT can stop enumeration without a temporary full-match sort.
///
/// Replaces a single `ORDER BY bm25(...)` over one OR-joined match expression
/// (whose cost scales with the size of the entire match set — the #1930 read
/// timeout at ~94K atoms) with one bounded, independently-capped subquery per
/// term, unioned and deduplicated in application code. FTS remains only the
/// candidate generator; TF-IDF in `search_core` remains the ranker, so the
/// per-term merge order does not need to be a globally correct bm25 rank.
#[allow(clippy::too_many_arguments)]
async fn fetch_fts_candidates(
    runtime: &KhiveRuntime,
    ns: &str,
    raw_query: &str,
    type_filter: Option<&str>,
    statuses: &[String],
    exclude_statuses: &[&str],
    fetch_limit: usize,
    term_budget: &FtsTermBudget,
    mut stage: LexicalStage,
) -> Result<FtsFetchOutcome, RuntimeError> {
    let mut terms = term_budget.admit(fts5_candidate_terms(raw_query));
    if terms.is_empty() {
        return Ok(FtsFetchOutcome {
            atoms: Vec::new(),
            timeout: None,
            state: LexicalCandidateState::NoMatch,
        });
    }
    let sql = runtime.sql();
    let reader = match stage.read(LexicalPhase::ReaderOpen, sql.reader()).await {
        Ok(reader) => reader,
        Err(e) if is_timeout(&e) => {
            return Ok(FtsFetchOutcome {
                atoms: Vec::new(),
                timeout: stage.timeout,
                state: LexicalCandidateState::TimedOut,
            });
        }
        Err(e) => return Err(sql_err("search fts reader", e)),
    };

    #[cfg(test)]
    let reader = tests::record_term_probes(reader);
    let mut reader = reader;
    #[cfg(all(test, feature = "namespace-trigram-proto"))]
    if let Ok(experiment) = NAMESPACE_TRIGRAM_EXPERIMENT.try_with(Clone::clone) {
        // The slot-table baseline resolves its namespace slot once per
        // lexical pass. Both paired arms retain that indexed lookup, so their
        // timings include the same key-read cost.
        let key_row = match stage
            .read(
                LexicalPhase::ReaderOpen,
                reader.query_row(SqlStatement {
                    sql: "SELECT namespace_key FROM knowledge_fts_namespace_tokens \
                          WHERE namespace = ?1"
                        .into(),
                    params: vec![SqlValue::Text(ns.into())],
                    label: Some("knowledge.fts_namespace_key".into()),
                }),
            )
            .await
        {
            Ok(row) => row,
            Err(e) if is_timeout(&e) => {
                return Ok(FtsFetchOutcome {
                    atoms: Vec::new(),
                    timeout: stage.timeout,
                    state: LexicalCandidateState::TimedOut,
                });
            }
            Err(e) => return Err(sql_err("search fts namespace key", e)),
        };
        let found = key_row
            .as_ref()
            .and_then(|row| row_str(row, "namespace_key"))
            .ok_or_else(|| {
                RuntimeError::Internal(format!("missing knowledge FTS namespace key for {ns:?}"))
            })?;
        let expected = match experiment {
            NamespaceTrigramExperiment::SlotTable { key }
            | NamespaceTrigramExperiment::Prefixed { key } => key,
        };
        if found != expected {
            return Err(RuntimeError::Internal(
                "prototype namespace key differs from slot-table baseline".into(),
            ));
        }
    }
    let type_clause = type_eligibility_sql(type_filter, "a");
    let per_term_limit = if terms.len() == 1 {
        fetch_limit
    } else {
        fetch_limit.clamp(1, FTS_TERM_LIMIT)
    };
    // `status_sql_clause`'s no-filter case still excludes `deprecated`, so
    // this is currently always true; kept explicit (not assumed) so a future
    // change that *can* return an empty clause degrades to no-overfetch
    // instead of silently keeping an overfetch with nothing to absorb.
    let has_eligibility_clause = !status_sql_clause(statuses, exclude_statuses, 1)
        .0
        .is_empty()
        || !type_clause.is_empty();
    let widen_ceiling = phase_a_widen_ceiling();
    let base_probe_limit = if has_eligibility_clause {
        per_term_limit
            .saturating_mul(PHASE_A_OVERFETCH_FACTOR)
            .min(widen_ceiling)
    } else {
        per_term_limit
    };

    // Set when the ordering probe below is cut off by its own deadline. The
    // per-term fetch has `term_query_timed_out` for the same purpose; this is a
    // second, independent way for the stage to have degraded, and the classifier
    // at the bottom has to see both or an expired probe reads as a clean miss.
    let mut rarity_probe_timed_out = false;
    if terms.len() > 1 {
        // The probe runs under its OWN, tighter deadline nested inside the
        // stage's. `scope_request_read_deadline` keeps whichever deadline is
        // earlier and restores the wider one on the way out, so this bounds
        // the probe and leaves the rest of the stage budget to the fetch.
        //
        // On expiry the terms are used in the order the candidate list
        // produced them. That order is not the caller's word order:
        // `fts5_candidate_terms` runs `expand_terms`, which sorts, so the
        // fallback is lexicographic and two spellings of the same query fall
        // back identically. It is the unoptimized path, not a failure path:
        // the ordering is a hint about which term to query first, and every
        // term is queried either way. The recorded timeout still rides out on
        // `stage.timeout`, so the expiry is disclosed rather than swallowed --
        // but it is disclosed as `degraded.lexical_ordering_probe_timeout`,
        // NOT as `degraded.lexical_timeout`, which stays the flag for a
        // candidate fetch that was actually cut short. Reporting both under
        // one flag is what issue #2879 measured: on one serving process 86 of
        // 196 timeout records were this graceful fallback, every one of them
        // at 500-532 ms against a 2000 ms stage budget that had not expired,
        // wearing the same flag and the same wording as a real cut.
        let arrival_order = terms.clone();
        let probe_budget = rarity_probe_budget();
        let probed = khive_storage::scope_request_read_deadline(probe_budget, async {
            rarest_fts_terms_first(reader.as_mut(), terms, &mut stage).await
        })
        .await;
        terms = match probed {
            Ok(terms) => terms,
            Err(e) if is_timeout(&e) => {
                rarity_probe_timed_out = true;
                arrival_order
            }
            Err(e) => return Err(sql_err("search fts term frequency probe", e)),
        };
    }

    // Query every matching term rather than stopping once `combined` reaches
    // `fetch_limit` — an early break made pool membership depend on query
    // word order (a fast-filling early term could starve every later term
    // of a query at all). Each term's rows are collected independently and
    // merged round-robin below, so no single term can crowd out the rest.
    // Rarest-first scheduling preserves useful narrow matches if a later
    // common term exhausts the independent stage deadline.
    let mut per_term_rows: Vec<Vec<Atom>> = Vec::with_capacity(terms.len());
    // Retain phase-A rowids to distinguish filtered local matches from a miss
    // without another FTS query or an unscoped existence probe.
    let mut term_probe_rowids: Vec<Vec<i64>> = Vec::with_capacity(terms.len());
    let mut unexhausted_terms = Vec::new();
    let mut term_query_timed_out = false;

    'terms: for term in &terms {
        let mut probe_limit = base_probe_limit;

        let (mut eligible, probed_rowids, exhausted): (Vec<Atom>, Vec<i64>, bool) = loop {
            let phase_a_rows = match stage
                .read(
                    LexicalPhase::PhaseARowids,
                    reader.query_all(phase_a_rowids_statement(term, probe_limit)),
                )
                .await
            {
                Ok(rows) => rows,
                Err(e) if is_timeout(&e) => {
                    term_query_timed_out = true;
                    break 'terms;
                }
                Err(e) => return Err(sql_err("search fts phase-a query", e)),
            };
            let rowids: Vec<i64> = phase_a_rows
                .iter()
                .filter_map(|r| row_i64(r, "rowid"))
                .collect();
            let phase_a_full = rowids.len() >= probe_limit;
            if rowids.is_empty() {
                break (Vec::new(), rowids, true);
            }

            let mut atoms_by_rowid: HashMap<i64, Atom> = HashMap::with_capacity(rowids.len());
            for chunk in rowids.chunks(HYDRATION_ID_CHUNK) {
                let statement = phase_b_hydration_statement(
                    ns,
                    chunk,
                    statuses,
                    exclude_statuses,
                    type_clause.as_str(),
                );
                let rows = match stage
                    .read(LexicalPhase::PhaseBHydration, reader.query_all(statement))
                    .await
                {
                    Ok(rows) => rows,
                    Err(e) if is_timeout(&e) => {
                        term_query_timed_out = true;
                        break 'terms;
                    }
                    Err(e) => return Err(sql_err("search fts phase-b hydration", e)),
                };
                for row in &rows {
                    if let (Some(atom), Some(rowid)) = (atom_from_row(row), row_i64(row, "rowid")) {
                        atoms_by_rowid.insert(rowid, atom);
                    }
                }
            }

            // Reassemble in phase A's rowid order; ineligible rows or intervening deletes drop out.
            // The reads share no snapshot: an intervening edit hydrates current text,
            // which the lexical scorer uses even if it no longer matches the term.
            let eligible_now: Vec<Atom> = rowids
                .iter()
                .filter_map(|rowid| atoms_by_rowid.get(rowid).cloned())
                .collect();

            // Widen only when phase A itself was the bottleneck (it returned
            // a full page, meaning more matches may exist beyond this
            // probe) and only up to the ceiling; a term that is genuinely
            // exhausted (phase A returned less than it asked for) has
            // nothing more to gain from a wider probe.
            if has_eligibility_clause && eligible_now.len() < per_term_limit && phase_a_full {
                if probe_limit < widen_ceiling {
                    probe_limit = probe_limit
                        .saturating_mul(PHASE_A_OVERFETCH_FACTOR)
                        .min(widen_ceiling);
                    continue;
                }

                // Ceiling exhausted and still short: more than `widen_ceiling`
                // ineligible top-ranked rows could still be hiding an
                // eligible one further down the rowid sequence than phase A
                // ever probed. Pay once for the pre-#2396-shape eligibility-
                // scoped join, bounded to this term's own `per_term_limit`,
                // so the ceiling bounds cost without letting an ineligible-
                // heavy match set hide a real candidate (issue #2396 fix 2).
                let (scoped_status_clause, scoped_status_params) =
                    status_sql_clause(statuses, exclude_statuses, 4);
                let scoped_sql = format!(
                    "SELECT a.* FROM fts_knowledge \
                     CROSS JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid \
                     WHERE fts_knowledge MATCH ?1 \
                       AND +a.namespace = ?2 \
                       AND a.deleted_at IS NULL{scoped_status_clause}{type_clause} \
                     ORDER BY fts_knowledge.rowid \
                     LIMIT ?3"
                );
                let scoped_term = term.clone();
                #[cfg(all(test, feature = "namespace-trigram-proto"))]
                let (scoped_sql, scoped_term) =
                    if let Some((table, match_expression)) = prototype_fts_target(term) {
                        (
                            format!(
                                "SELECT a.* FROM {table} \
                                 CROSS JOIN knowledge_atoms AS a ON a.rowid = {table}.rowid \
                                 WHERE {table} MATCH ?1 \
                                   AND +a.namespace = ?2 \
                                   AND a.deleted_at IS NULL{scoped_status_clause}{type_clause} \
                                 ORDER BY {table}.rowid LIMIT ?3"
                            ),
                            match_expression,
                        )
                    } else {
                        (scoped_sql, scoped_term)
                    };
                let mut scoped_params = vec![
                    SqlValue::Text(scoped_term),
                    SqlValue::Text(ns.to_owned()),
                    SqlValue::Integer(per_term_limit as i64),
                ];
                scoped_params.extend(scoped_status_params);
                let scoped_rows = match stage
                    .read(
                        LexicalPhase::EligibilityFallback,
                        reader.query_all(SqlStatement {
                            sql: scoped_sql,
                            params: scoped_params,
                            label: None,
                        }),
                    )
                    .await
                {
                    Ok(rows) => rows,
                    Err(e) if is_timeout(&e) => {
                        term_query_timed_out = true;
                        break 'terms;
                    }
                    Err(e) => {
                        return Err(sql_err("search fts eligibility-scoped ceiling fallback", e));
                    }
                };
                break (
                    scoped_rows.iter().filter_map(atom_from_row).collect(),
                    rowids,
                    false,
                );
            }
            break (eligible_now, rowids, !phase_a_full);
        };

        eligible.truncate(per_term_limit);
        per_term_rows.push(eligible);
        term_probe_rowids.push(probed_rowids);
        if !exhausted {
            unexhausted_terms.push(term.clone());
        }
        #[cfg(test)]
        advance_fts_test_deadline_after_term(per_term_rows.len()).await;
    }

    let mut seen_ids: HashSet<Uuid> = HashSet::new();
    let mut combined: Vec<Atom> = Vec::new();
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

    // Either degradation lands here. A cut-off ordering probe with candidates in
    // hand is `PartialTimeout`, which is what issue #2766 asks for: the lexical
    // arm survives and the degradation is still reported. With no candidates it
    // is `TimedOut` exactly as a per-term expiry is, because the alternative is
    // for the classifier below to call it a clean local miss, and a miss and a
    // stage that ran out of time are the two answers a caller must be able to
    // tell apart.
    if term_query_timed_out || rarity_probe_timed_out {
        return Ok(FtsFetchOutcome {
            state: if combined.is_empty() {
                LexicalCandidateState::TimedOut
            } else {
                LexicalCandidateState::PartialTimeout
            },
            atoms: combined,
            timeout: stage.timeout,
        });
    }

    if !combined.is_empty() {
        return Ok(FtsFetchOutcome {
            atoms: combined,
            timeout: None,
            state: LexicalCandidateState::Matched,
        });
    }

    // Classify empty candidates using only namespace-scoped evidence. A match
    // in another tenant must remain indistinguishable from a true local miss.
    // Reuse the bounded phase-A windows; never browse unrelated recent rows.
    let mut probe_rowids: Vec<i64> = term_probe_rowids.into_iter().flatten().collect();
    probe_rowids.sort_unstable();
    probe_rowids.dedup();

    let mut namespace_has_match = false;
    for chunk in probe_rowids.chunks(HYDRATION_ID_CHUNK) {
        let placeholders = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let mut params = vec![SqlValue::Text(ns.to_owned())];
        params.extend(chunk.iter().map(|rowid| SqlValue::Integer(*rowid)));
        let membership_sql = format!(
            "SELECT 1 AS present FROM knowledge_atoms \
             WHERE rowid IN ({placeholders}) AND namespace = ?1 LIMIT 1"
        );
        let row = match stage
            .read(
                LexicalPhase::NamespaceMembership,
                reader.query_row(SqlStatement {
                    sql: membership_sql,
                    params,
                    label: None,
                }),
            )
            .await
        {
            Ok(row) => row,
            Err(e) if is_timeout(&e) => {
                return Ok(FtsFetchOutcome {
                    atoms: Vec::new(),
                    timeout: stage.timeout,
                    state: LexicalCandidateState::TimedOut,
                });
            }
            Err(e) => return Err(sql_err("search fts namespace membership probe", e)),
        };
        if row.is_some() {
            namespace_has_match = true;
            break;
        }
    }
    // A capped global window cannot prove local absence: a foreign prefix may
    // hide local ineligible rows. Recover namespace-only existence for those
    // terms before exposing no_match, within the same lexical deadline.
    if !namespace_has_match {
        for term in unexhausted_terms {
            let statement = SqlStatement {
                sql: "SELECT 1 AS present FROM fts_knowledge \
                      CROSS JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid \
                      WHERE fts_knowledge MATCH ?1 AND +a.namespace = ?2 LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(term.clone()), SqlValue::Text(ns.to_owned())],
                label: None,
            };
            #[cfg(all(test, feature = "namespace-trigram-proto"))]
            let statement = if let Some((table, match_expression)) = prototype_fts_target(&term) {
                SqlStatement {
                    sql: format!(
                        "SELECT 1 AS present FROM {table} \
                         CROSS JOIN knowledge_atoms AS a ON a.rowid = {table}.rowid \
                         WHERE {table} MATCH ?1 AND +a.namespace = ?2 LIMIT 1"
                    ),
                    params: vec![
                        SqlValue::Text(match_expression),
                        SqlValue::Text(ns.to_owned()),
                    ],
                    label: None,
                }
            } else {
                statement
            };
            let row = match stage
                .read(
                    LexicalPhase::NamespaceExistence,
                    reader.query_row(statement),
                )
                .await
            {
                Ok(row) => row,
                Err(e) if is_timeout(&e) => {
                    return Ok(FtsFetchOutcome {
                        atoms: Vec::new(),
                        timeout: stage.timeout,
                        state: LexicalCandidateState::TimedOut,
                    });
                }
                Err(e) => return Err(sql_err("search fts scoped namespace existence", e)),
            };
            if row.is_some() {
                namespace_has_match = true;
                break;
            }
        }
    }
    if namespace_has_match {
        return Ok(FtsFetchOutcome {
            atoms: Vec::new(),
            timeout: None,
            state: LexicalCandidateState::Filtered,
        });
    }

    let no_scoreable_terms = !matching::tokenize_field(raw_query)
        .iter()
        .any(|term| term.len() >= MIN_TERM_LEN && !is_stop(term));
    if no_scoreable_terms {
        match fetch_exact_name_candidate(
            reader.as_mut(),
            ns,
            raw_query,
            type_filter,
            statuses,
            exclude_statuses,
            &mut stage,
        )
        .await?
        {
            ExactNameProbe::Hit(atom) => {
                return Ok(FtsFetchOutcome {
                    atoms: vec![*atom],
                    timeout: None,
                    state: LexicalCandidateState::ExactName,
                })
            }
            ExactNameProbe::Filtered => {
                return Ok(FtsFetchOutcome {
                    atoms: Vec::new(),
                    timeout: None,
                    state: LexicalCandidateState::Filtered,
                })
            }
            ExactNameProbe::TimedOut => {
                return Ok(FtsFetchOutcome {
                    atoms: Vec::new(),
                    timeout: stage.timeout,
                    state: LexicalCandidateState::TimedOut,
                })
            }
            ExactNameProbe::Miss => {}
        }
    }

    Ok(FtsFetchOutcome {
        atoms: Vec::new(),
        timeout: None,
        state: LexicalCandidateState::NoMatch,
    })
}

fn exact_name_statement(
    ns: &str,
    slug: &str,
    type_filter: Option<&str>,
    statuses: &[String],
    exclude_statuses: &[&str],
) -> SqlStatement {
    let (status_clause, status_params) = status_sql_clause(statuses, exclude_statuses, 3);
    let type_clause = type_eligibility_sql(type_filter, "knowledge_atoms");
    let mut params = vec![
        SqlValue::Text(ns.to_owned()),
        SqlValue::Text(slug.to_owned()),
    ];
    params.extend(status_params);
    SqlStatement {
        sql: format!(
            "SELECT *, CASE WHEN 1{status_clause}{type_clause} THEN 1 ELSE 0 END AS exact_name_eligible \
             FROM knowledge_atoms \
             WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1"
        ),
        params,
        label: Some("knowledge.exact_name".into()),
    }
}

/// Reuse the lexical pass's reader and remaining deadline: opening a fresh
/// stage here would give a short query a second, unaccounted retrieval budget.
/// The unique namespace/slug key bounds this to one row. Custom slugs outside
/// the import convention remain outside the exact-name recovery guarantee.
async fn fetch_exact_name_candidate(
    reader: &mut dyn khive_storage::SqlReader,
    ns: &str,
    raw_query: &str,
    type_filter: Option<&str>,
    statuses: &[String],
    exclude_statuses: &[&str],
    stage: &mut LexicalStage,
) -> Result<ExactNameProbe, RuntimeError> {
    let slug = to_slug(raw_query);
    if slug.is_empty() {
        return Ok(ExactNameProbe::Miss);
    }
    let row = stage
        .read(
            LexicalPhase::ExactNameProbe,
            reader.query_row(exact_name_statement(
                ns,
                &slug,
                type_filter,
                statuses,
                exclude_statuses,
            )),
        )
        .await;
    match row {
        Ok(Some(row)) if row_i64(&row, "exact_name_eligible") != Some(1) => {
            Ok(ExactNameProbe::Filtered)
        }
        Ok(Some(row)) => Ok(atom_from_row(&row).map_or(ExactNameProbe::Miss, |atom| {
            ExactNameProbe::Hit(Box::new(atom))
        })),
        Ok(None) => Ok(ExactNameProbe::Miss),
        Err(e) if is_timeout(&e) => Ok(ExactNameProbe::TimedOut),
        Err(e) => Err(sql_err("search exact-name query", e)),
    }
}

#[derive(Debug)]
enum ExactNameProbe {
    Hit(Box<Atom>),
    Miss,
    Filtered,
    TimedOut,
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
    term_budget: &'a FtsTermBudget,
}

// ─── core single-pass search ──────────────────────────────────────────────────

/// `search_core`'s result plus any lexical/FTS read timeout diagnostics.
/// A caller sees `hits` possibly empty/partial and timeout details instead of an
/// `Err` — never a verb-level error for a genuine timeout.
struct SearchCoreOutcome {
    hits: Vec<ScoredHit>,
    lexical_timeouts: Vec<LexicalTimeout>,
    lexical_state: LexicalCandidateState,
}

async fn search_core(
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

fn merge_decomposed_hits(
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

// ─── embedding rerank ────────────────────────────────────────────────────────

/// Request-local cache for the query's embedding vector(s), threaded through
/// search/suggest/compose so a query is embedded at most once per role it is
/// actually needed in.
///
/// `EmbeddingService::embed_query` and `::embed` are different provider
/// methods: instruction-tuned models (E5, Qwen) prepend a query prompt for
/// the former, so the two calls land in different sides of the retrieval
/// space for the identical text. `role_specific` holds the outcome of the
/// request's one attempt at `runtime.embed_query` (the ANN and KG-blend
/// dense-search path); `generic` holds the first slot of a combined
/// `runtime.embed_batch` rerank call. A `generic` vector is a valid stand-in
/// for `role_specific` in rerank cosine math (both are just "the query's
/// embedding" for a same-space comparison against candidates embedded the
/// same generic way), but it must never satisfy a caller that specifically
/// requires `role_specific` — passing it to the KG blend's `hybrid_search`
/// masks a real `embed_query` failure behind a same-shaped, wrong-space
/// vector (#2307).
#[derive(Debug, Default, Clone)]
struct QueryEmbeddingCache {
    role_specific: RoleSpecificEmbedding,
    generic: Option<Vec<f32>>,
}

/// Outcome of the request's (at most one) attempt to obtain the query's
/// role-specific (`embed_query`) vector.
///
/// A plain `Option<Vec<f32>>` cannot tell "never tried" from "tried and the
/// provider failed" — both read as `None`. That collapse is exactly what let
/// a failed attempt in one stage (e.g. `suggest`) get retried by a later
/// stage in the same request (e.g. `compose`'s KG-blend gate): the later
/// stage saw `None` and had no way to know an attempt, and a failure, had
/// already happened (#2307). `Failed` records that the attempt happened so
/// nothing downstream pays for a second failing provider call; only
/// `NotAttempted` authorizes a stage to try.
#[derive(Debug, Clone, Default, PartialEq)]
enum RoleSpecificEmbedding {
    #[default]
    NotAttempted,
    Failed,
    Vector(Vec<f32>),
}

impl RoleSpecificEmbedding {
    fn as_deref(&self) -> Option<&[f32]> {
        match self {
            Self::Vector(v) => Some(v.as_slice()),
            Self::NotAttempted | Self::Failed => None,
        }
    }

    fn is_not_attempted(&self) -> bool {
        matches!(self, Self::NotAttempted)
    }
}

impl QueryEmbeddingCache {
    fn any(&self) -> Option<&[f32]> {
        self.role_specific.as_deref().or(self.generic.as_deref())
    }
}

async fn embed_cosine_scores(
    runtime: &KhiveRuntime,
    query: &str,
    query_embedding: &mut QueryEmbeddingCache,
    candidate_texts: &[String],
) -> Result<Option<Vec<f32>>, RuntimeError> {
    if runtime.default_embedder_name().is_empty() || candidate_texts.is_empty() {
        return Ok(None);
    }

    // When nothing is cached yet, include the query in this one batch and
    // retain its vector as `generic` for downstream cosine math. Otherwise
    // embed only candidates; the query vector is request-local immutable data.
    let query_was_cached = query_embedding.any().is_some();
    let texts = if query_was_cached {
        candidate_texts.to_vec()
    } else {
        let mut texts = Vec::with_capacity(candidate_texts.len() + 1);
        texts.push(query.to_string());
        texts.extend_from_slice(candidate_texts);
        texts
    };
    // A request read-deadline timeout here degrades to "rerank did not run"
    // (`Ok(None)`) rather than propagating, same contract as every other
    // read in this module (issue #1930 Amendment 2): before the lexical
    // stage owned its own budget, this call was only ever reached when the
    // shared request deadline had *not* already expired, so a mid-flight
    // expiry racing this specific await was unreachable in practice. With
    // the stages decoupled, a lexical-only degradation with real time left
    // to spare now reaches this call normally, and the ordinary
    // async-scheduling race — the deadline elapsing while this embed_batch
    // is in flight — is reachable and must degrade, not hard-error.
    let embeddings = match khive_storage::await_request_read_phase(
        "knowledge.embedding_rerank",
        runtime.embed_batch(&texts),
    )
    .await
    {
        Ok(Ok(embeddings)) => embeddings,
        Ok(Err(_)) => return Ok(None),
        Err(e) if is_timeout(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if embeddings.len() != texts.len() {
        return Ok(None);
    }
    let candidate_embeddings = if query_was_cached {
        embeddings.as_slice()
    } else {
        query_embedding.generic = Some(embeddings[0].clone());
        &embeddings[1..]
    };
    let query_emb = query_embedding
        .any()
        .expect("query embedding was cached or populated from non-empty batch");
    Ok(Some(
        candidate_embeddings
            .iter()
            .map(|emb| cosine_similarity(query_emb, emb))
            .collect(),
    ))
}

async fn rerank_with_embeddings(
    runtime: &KhiveRuntime,
    query: &str,
    query_embedding: &mut QueryEmbeddingCache,
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
    if let Some(cosines) = embed_cosine_scores(runtime, query, query_embedding, &texts).await? {
        let max_tfidf = hits
            .iter()
            .map(|h| h.score)
            .fold(0.0f32, f32::max)
            .max(1e-6);
        for (hit, cos) in hits.iter_mut().zip(cosines.iter()) {
            let norm_tfidf = hit.score / max_tfidf;
            hit.score = alpha * norm_tfidf + (1.0 - alpha) * cos.max(0.0);
            hit.provenance.embedding_rerank = true;
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

/// Search uses the same document-intent vectors as knowledge indexing. The
/// older generic reranker remains for suggest, whose candidates are domains.
async fn rerank_search_with_stored_vectors(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    namespace: &str,
    query: &str,
    query_embedding: &mut QueryEmbeddingCache,
    hits: &mut [ScoredHit],
    alpha: f32,
) -> Result<Option<Value>, RuntimeError> {
    if hits.is_empty() {
        return Ok(None);
    }
    if query_embedding.role_specific.is_not_attempted() {
        query_embedding.role_specific = match khive_storage::await_request_read_phase(
            "knowledge.embedding_rerank",
            runtime.embed_query(query),
        )
        .await
        {
            Ok(Ok(vector)) => RoleSpecificEmbedding::Vector(vector),
            Ok(Err(_)) => RoleSpecificEmbedding::Failed,
            Err(error) if is_timeout(&error) => RoleSpecificEmbedding::Failed,
            Err(error) => return Err(error.into()),
        };
    }
    let Some(query_vector) = query_embedding.role_specific.as_deref() else {
        return Ok(None);
    };
    let store = match runtime.vectors_for_model(token, runtime.default_embedder_name()) {
        Ok(store) => Some(store),
        Err(RuntimeError::Storage(khive_storage::StorageError::Unsupported { .. })) => None,
        Err(error) => return Err(error),
    };

    rerank_search_from_store(
        runtime,
        store.as_deref(),
        namespace,
        query_vector,
        hits,
        alpha,
    )
    .await
}

/// Kept separate from store resolution so a backend without by-ID reads can
/// exercise the fallback and its response provenance in focused tests.
async fn rerank_search_from_store(
    runtime: &KhiveRuntime,
    store: Option<&dyn khive_storage::VectorStore>,
    namespace: &str,
    query_vector: &[f32],
    hits: &mut [ScoredHit],
    alpha: f32,
) -> Result<Option<Value>, RuntimeError> {
    let candidate_ids: Vec<Uuid> = hits
        .iter()
        .filter_map(|hit| Uuid::parse_str(&hit.id).ok())
        .collect();
    let mut stored_vectors = HashMap::new();
    let mut stored_vector_lookup = "unsupported";
    if let Some(store) = store {
        if store.capabilities().supports_vector_read {
            stored_vector_lookup = "supported";
            if !candidate_ids.is_empty() {
                stored_vectors = match khive_storage::await_request_read_phase(
                    "knowledge.embedding_rerank.vector_read",
                    store.get_vectors(&candidate_ids, namespace, "knowledge.atom"),
                )
                .await
                {
                    Ok(Ok(vectors)) => vectors,
                    Ok(Err(khive_storage::StorageError::Unsupported { .. })) => {
                        stored_vector_lookup = "unsupported";
                        HashMap::new()
                    }
                    Ok(Err(error)) => return Err(error.into()),
                    Err(error) if is_timeout(&error) => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
            }
        }
    }

    let mut fallback_indices = Vec::new();
    let mut fallback_texts = Vec::new();
    let mut candidate_vectors = vec![None; hits.len()];
    for (index, hit) in hits.iter().enumerate() {
        let stored = Uuid::parse_str(&hit.id)
            .ok()
            .and_then(|id| stored_vectors.get(&id));
        if let Some(vector) = stored {
            candidate_vectors[index] = Some(vector.clone());
            continue;
        }
        fallback_indices.push(index);
        fallback_texts.push(hit.atom_embed_text.clone().unwrap_or_else(|| {
            atom_embed_text_fields(
                &hit.name,
                hit.content.as_deref().unwrap_or(""),
                hit.tags.as_deref().unwrap_or("[]"),
            )
        }));
    }

    if !fallback_texts.is_empty() {
        let embedded = match khive_storage::await_request_read_phase(
            "knowledge.embedding_rerank.fallback",
            runtime.embed_document_batch_outcomes(&fallback_texts),
        )
        .await
        {
            Ok(Ok(outcomes)) => outcomes
                .into_iter()
                .map(|outcome| outcome.vector)
                .collect::<Vec<_>>(),
            Ok(Err(_)) => return Ok(None),
            Err(error) if is_timeout(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if embedded.len() != fallback_indices.len() {
            return Ok(None);
        }
        for (index, vector) in fallback_indices.iter().copied().zip(embedded) {
            candidate_vectors[index] = Some(vector);
        }
    }

    let from_stored = hits.len() - fallback_indices.len();
    let max_score = hits
        .iter()
        .map(|hit| hit.score)
        .fold(0.0f32, f32::max)
        .max(1e-6);
    for (hit, vector) in hits.iter_mut().zip(candidate_vectors) {
        let vector = vector.expect("every rerank candidate has a stored or fallback vector");
        let cosine = cosine_similarity(query_vector, &vector);
        hit.score = alpha * (hit.score / max_score) + (1.0 - alpha) * cosine.max(0.0);
        hit.provenance.embedding_rerank = true;
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    Ok(Some(json!({
        "stored_vector_lookup": stored_vector_lookup,
        "candidates": hits.len(),
        "from_stored": from_stored,
        "embedded_fallback": fallback_indices.len(),
    })))
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
pub(super) const HYDRATION_ID_CHUNK: usize = 900;

/// Build the atom hydration statement for one id chunk.
///
/// The `IN (...)` list runs off `knowledge_atoms`' primary key, so the
/// namespace predicate is deliberately kept out of index selection with
/// SQLite's unary `+` (`+namespace`): a scratch or freshly vacuumed store has
/// no `sqlite_stat1`, and without statistics the planner's default guess for
/// an indexed namespace equality (~10 rows) beats a 250+ id primary-key
/// probe, so it picks `idx_knowledge_atoms_ns_created` and walks the whole
/// namespace once per chunk instead of doing 250 point lookups. `+namespace`
/// removes the column from consideration as an index term so the primary key
/// wins regardless of table size or missing statistics. Do not remove it.
fn hydrate_atoms_statement(ns: &str, ids: &[String]) -> SqlStatement {
    let placeholders = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(ids.iter().cloned().map(SqlValue::Text));
    SqlStatement {
        sql: format!(
            "SELECT id, slug, name, content, tags, finalized, status FROM knowledge_atoms \
             WHERE id IN ({placeholders}) AND +namespace = ?1 AND deleted_at IS NULL"
        ),
        params,
        label: None,
    }
}

/// Build the domain hydration statement for one id chunk.
///
/// Same primary-key-first shape as [`hydrate_atoms_statement`], for the same
/// reason: `knowledge_domains` also carries a namespace index
/// (`idx_knowledge_domains_ns`) that the no-statistics planner would
/// otherwise prefer over the primary key on a large id list.
fn hydrate_domains_statement(ns: &str, ids: &[String]) -> SqlStatement {
    let placeholders = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(ids.iter().cloned().map(SqlValue::Text));
    SqlStatement {
        sql: format!(
            "SELECT id, slug, name, description, tags, status FROM knowledge_domains \
             WHERE id IN ({placeholders}) AND +namespace = ?1 AND deleted_at IS NULL"
        ),
        params,
        label: None,
    }
}

/// Build the phase-B hydration statement for one chunk of phase-A rowids
/// (issue #2396 fix 5).
///
/// `+a.namespace = ?1` (not a bare equality) is deliberate — same reason and
/// shape as [`hydrate_atoms_statement`]/[`hydrate_domains_statement`]:
/// without table statistics (this codebase never runs `ANALYZE` on
/// `knowledge_atoms`), SQLite's planner prefers `idx_knowledge_atoms_ns` over
/// the integer primary key for a large `rowid IN (...)` list, turning an
/// O(chunk) rowid seek into an O(matching-namespace-rows) index scan per
/// chunk. Verified with `EXPLAIN QUERY PLAN` against a freshly loaded,
/// unanalyzed `knowledge_atoms` table at a 900-row chunk (this statement's
/// own `HYDRATION_ID_CHUNK`): without the unary plus the planner chose
/// `SEARCH a USING INDEX idx_knowledge_atoms_ns (namespace=? AND rowid=?)`;
/// with it, `SEARCH a USING INTEGER PRIMARY KEY (rowid=?)`. The unary plus
/// defeats the index without changing the predicate's meaning.
fn phase_b_hydration_statement(
    ns: &str,
    rowids: &[i64],
    statuses: &[String],
    exclude_statuses: &[&str],
    type_clause: &str,
) -> SqlStatement {
    let rowid_placeholders = rowids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let (status_clause, status_params) =
        status_sql_clause(statuses, exclude_statuses, 2 + rowids.len());
    let mut params = vec![SqlValue::Text(ns.to_owned())];
    params.extend(rowids.iter().map(|rowid| SqlValue::Integer(*rowid)));
    params.extend(status_params);
    SqlStatement {
        sql: format!(
            "SELECT a.*, a.rowid AS rowid FROM knowledge_atoms AS a \
             WHERE a.rowid IN ({rowid_placeholders}) \
               AND +a.namespace = ?1 \
               AND a.deleted_at IS NULL{status_clause}{type_clause}"
        ),
        params,
        label: None,
    }
}

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
        match reader.query_all(hydrate_atoms_statement(ns, chunk)).await {
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
            hit.atom_embed_text = Some(atom_embed_text_fields(
                &hit.name,
                hit.content.as_deref().unwrap_or(""),
                hit.tags.as_deref().unwrap_or("[]"),
            ));
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
        match reader.query_all(hydrate_domains_statement(ns, chunk)).await {
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
            hit.atom_embed_text = None;
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
fn attach_lexical_timeout_degradation(out: &mut Value, timeouts: &[LexicalTimeout]) {
    if timeouts.is_empty() {
        return;
    }
    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }

    // `lexical_timeout` STAYS COARSE: any record at all sets it, exactly as
    // before. It is tempting to narrow it to "a candidate fetch was cut", and
    // issue #2879 asked for that, but it cannot be done without turning this
    // boolean into a cross-namespace disclosure channel.
    //
    // The later phases (`phase_a_rowids`, `phase_b_hydration`,
    // `eligibility_fallback`, the namespace probes) are reachable only when the
    // GLOBAL index matched, which can happen on another namespace's rows when
    // the local ones do not match at all. That is why their records are
    // operator-only and withheld from `lexical_timeout_details`. A boolean that
    // is true when one of them timed out and false when only the ordering probe
    // did would republish the very fact the detail list is censored to hide:
    // the caller would learn that a foreign row matched by observing a flag
    // appear. Measured by
    // `mixed_pass_capability_marker_does_not_reveal_foreign_matches`, which is
    // the guard that catches exactly this and did.
    //
    // So the coarse flag is load-bearing PRECISELY because it is coarse, and
    // the ordering-probe fallback is reported ADDITIVELY beside it instead.
    out["degraded"]["lexical_timeout"] = json!(true);
    out["degraded"]["lexical_timeout_instrumented"] = json!(true);

    if timeouts
        .iter()
        .any(|detail| detail.bound == LexicalBound::OrderingProbe)
    {
        // The ordering hint was skipped and the terms were queried in arrival
        // order. Safe to publish, and this is the reason it is a separate key
        // rather than a narrowing of the one above: the probe runs in
        // `term_frequency`, a PUBLIC phase whose entry does not depend on corpus
        // contents, so its presence is already disclosed in the detail list and
        // this flag reveals nothing new. A flag keyed on the later phases would
        // not have that property.
        out["degraded"]["lexical_ordering_probe_timeout"] = json!(true);
    }

    // ONE details list, with the population it always had. Splitting the
    // BOOLEANS is the fix; splitting the disclosure would have been a
    // regression, because `term_frequency` is one of only two public phases and
    // is by far the most common, so a separate list would have emptied the
    // public timing surface for the majority of timeouts. Each record now
    // carries `bound`, which is what lets a caller tell the two apart without
    // losing the timings.
    let details: Vec<_> = timeouts
        .iter()
        .filter(|detail| detail.phase.public())
        .take(3)
        .collect();
    if !details.is_empty() {
        out["degraded"]["lexical_timeout_details"] = json!(details);
    }
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

/// Report unmeasured domains under the stable `member_sizing_timeout` key,
/// whether sizing timed out or the canonical domain row could not be measured.
/// Every affected domain is withheld from `results` — never left in with a
/// `size` the caller cannot price — and listed here instead, as
/// `{id, name, rank, score}`, so the caller sees exactly which ranked hits
/// were excluded and why. `suggest`'s documented contract (issue #105) is
/// that `results` feeds `knowledge.fold`'s `candidates` unmodified;
/// withholding the unpriced domain keeps that passthrough valid instead of
/// turning one unmeasured domain into a hard parse error for the whole fold
/// request.
fn attach_member_sizing_timeout_degradation(out: &mut Value, excluded: &[Value]) {
    if excluded.is_empty() {
        return;
    }

    if !out
        .get("degraded")
        .is_some_and(serde_json::Value::is_object)
    {
        out["degraded"] = json!({});
    }
    out["degraded"]["member_sizing_timeout"] = json!({
        "excluded": excluded,
        "note": "no measurement was produced (timeout or missing canonical domain), so these domains \
                 were withheld from `results` — their cost is unknown and a \
                 budgeted knowledge.fold selection cannot safely admit an unpriced \
                 item. Each entry keeps its id/name/rank/score for reference; \
                 measure size separately before folding one of these in.",
    });
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
                atom_embed_text: None,
                finalized: false,
                is_domain: false,
                status: None,
                score,
                provenance: ScoreProvenance::ann(),
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

struct CandidateStageOutcome {
    ann_hits: Vec<ScoredHit>,
    ann_availability: Option<AnnAvailability>,
    hydration_failures: usize,
    query_embedding: QueryEmbeddingCache,
}

struct CandidateStageOptions {
    query_embedding: QueryEmbeddingCache,
    ann_target: usize,
    ann_initial_k: usize,
    operation: &'static str,
}

async fn run_candidate_stages<F>(
    ctx: &SearchCtx<'_>,
    token: &NamespaceToken,
    ann: &vamana::SharedAnn,
    raw_query: &str,
    options: CandidateStageOptions,
    lexical: F,
) -> Result<(SearchCoreOutcome, CandidateStageOutcome), RuntimeError>
where
    F: std::future::Future<Output = Result<SearchCoreOutcome, RuntimeError>>,
{
    vamana::ensure_ann_background(ctx.runtime, token, ann);
    let ann_stage = async {
        let mut query_embedding = options.query_embedding;
        let mut ann_hits = Vec::new();
        let mut ann_availability = None;
        let mut hydration_failures = 0;

        if query_embedding.role_specific.is_not_attempted() {
            query_embedding.role_specific = match khive_storage::await_request_read_phase(
                options.operation,
                ctx.runtime.embed_query(raw_query),
            )
            .await
            {
                Ok(Ok(vector)) => RoleSpecificEmbedding::Vector(vector),
                Ok(Err(_)) => RoleSpecificEmbedding::Failed,
                Err(error) if is_timeout(&error) => RoleSpecificEmbedding::Failed,
                Err(error) => return Err(error.into()),
            };
        }

        if let Some(vector) = query_embedding.role_specific.as_deref() {
            let key = vamana::AnnKey::new(ctx.ns, ctx.runtime.default_embedder_name());
            match khive_storage::await_request_read_phase(
                options.operation,
                search_eligible_ann_with_refill(
                    ctx,
                    token,
                    ann,
                    &key,
                    vector,
                    options.ann_target,
                    options.ann_initial_k,
                ),
            )
            .await
            {
                Ok(Ok(state)) => {
                    ann_hits = state.hits;
                    ann_availability = Some(state.availability);
                    hydration_failures = state.hydration_failures;
                }
                Ok(Err(error)) if is_read_timeout(&error) => {}
                Ok(Err(error)) => return Err(error),
                Err(error) if is_timeout(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }

        Ok(CandidateStageOutcome {
            ann_hits,
            ann_availability,
            hydration_failures,
            query_embedding,
        })
    };

    let (ann_result, lexical_result) = tokio::join!(ann_stage, lexical);
    let ann_result = ann_result?;
    Ok((lexical_result?, ann_result))
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
                let pattern = format!("{}%", hex_prefix_to_uuid_pattern(&id));
                let rows = reader
                    .query_all(SqlStatement {
                        sql: "SELECT * FROM knowledge_domains WHERE id LIKE ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 2".into(),
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

#[cfg(test)]
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

#[derive(Debug, Default)]
struct DomainMemberSizing {
    tokens: usize,
    live_members: usize,
}

/// Member-token sizing is best-effort: a request read-deadline timeout on
/// reader checkout or the query returns an empty map and `true`, not `Err`.
/// One `query_all` has no partial-completion state, so the flag covers the batch.
/// Every live canonical domain enters the map, with zero size and members
/// when no live member joins. Only absent domains stay unmeasured.
async fn load_domain_member_token_sizes(
    runtime: &KhiveRuntime,
    ns: &str,
    domain_ids: &[String],
) -> Result<(HashMap<String, DomainMemberSizing>, bool), RuntimeError> {
    let mut sizes: HashMap<String, DomainMemberSizing> = HashMap::new();
    if domain_ids.is_empty() {
        return Ok((sizes, false));
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
    let mut reader = match sql.reader().await {
        Ok(reader) => reader,
        Err(e) if is_timeout(&e) => return Ok((sizes, true)),
        Err(e) => return Err(sql_err("suggest member size reader", e)),
    };
    let rows = match reader
        .query_all(SqlStatement {
            // Keep atom identity in DISTINCT: repeated members count once, while
            // different atoms with identical names and content still count separately.
            sql: format!(
                "SELECT DISTINCT d.id AS domain_id, a.id AS atom_id, a.name, a.content \
                 FROM knowledge_domains AS d \
                 LEFT JOIN json_each(d.members) AS member ON 1 = 1 \
                 LEFT JOIN knowledge_atoms AS a \
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
    {
        Ok(rows) => rows,
        Err(e) if is_timeout(&e) => return Ok((sizes, true)),
        Err(e) => return Err(sql_err("suggest member size query", e)),
    };

    for row in rows {
        let Some(domain_id) = row_str(&row, "domain_id") else {
            continue;
        };
        let sizing = sizes.entry(domain_id).or_default();
        let Some(content) = row_str(&row, "content") else {
            continue;
        };
        let name = row_str(&row, "name").unwrap_or_default();
        sizing.tokens = sizing
            .tokens
            .saturating_add(estimate_compose_item_tokens(&name, &content));
        sizing.live_members = sizing.live_members.saturating_add(1);
    }

    Ok((sizes, false))
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
                 WHERE namespace = ?1 AND atom_id IN ({placeholders}) AND {SERVABLE_SECTION} \
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
    query_embedding: &mut QueryEmbeddingCache,
    items: &mut [ScoredTextItem],
) -> Result<(), RuntimeError> {
    if items.is_empty() {
        return Ok(());
    }
    let texts: Vec<String> = items.iter().map(|item| item.text.clone()).collect();
    if let Some(cosines) = embed_cosine_scores(runtime, query, query_embedding, &texts).await? {
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
/// `hybrid_search_each_kind` hands each blend kind the list `hybrid_search`
/// returns for it while issuing the vector query once for all kinds.
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
    query_embedding: &mut QueryEmbeddingCache,
    cap: usize,
    min_score: f32,
) -> Result<Vec<KgEntityHit>, RuntimeError> {
    // KG discovery has two kind-specific hybrid searches that share one vector query.
    // Both are gated on `role_specific` specifically, never `generic`: a vector produced by
    // the rerank's combined batch (`embed_batch`) lands in a different
    // embedding space than `embed_query` for asymmetric-prompt models, so it
    // must never stand in for a role-specific vector here — doing so would
    // mask a real `embed_query` failure behind a same-shaped wrong-space
    // vector instead of degrading to the atom-only briefing (#2307). No
    // role-specific vector (never attempted, or attempted and failed) means
    // the dense blend is unavailable and the caller safely keeps its
    // already-complete atom-only briefing.
    let Some(query_vector) = query_embedding.role_specific.as_deref() else {
        return Ok(Vec::new());
    };
    let candidate_k = ((cap * 4) as u32).max(20);
    let mut candidate_ids: Vec<Uuid> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    let hits_by_kind = runtime
        .hybrid_search_each_kind(
            token,
            query,
            Some(query_vector.to_vec()),
            candidate_k,
            &KG_BLEND_ENTITY_KINDS,
        )
        .await?;
    for hit in hits_by_kind.into_iter().flatten() {
        if seen.insert(hit.entity_id) {
            candidate_ids.push(hit.entity_id);
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
    let cosines = match embed_cosine_scores(runtime, query, query_embedding, &texts).await? {
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

const KG_ENTITIES_HEADING: &str = "\n---\n\n## Knowledge graph\n\n";

fn format_kg_entity_line(entity: &KgEntityHit) -> String {
    let mut line = format!("- **{}** ({})", entity.name, entity.kind);
    if !entity.description.is_empty() {
        line.push_str(&format!(" — {}", entity.description));
    }
    line.push('\n');
    line
}

/// The supplementary KG block has its own heading. Price the exact bytes
/// rendered, and skip a too-large hit so smaller lower-ranked hits can fit.
fn trim_kg_entities_to_budget(hits: Vec<KgEntityHit>, remaining_budget: usize) -> Vec<KgEntityHit> {
    let mut used = 0usize;
    hits.into_iter()
        .filter(|h| {
            let heading = if used == 0 {
                KG_ENTITIES_HEADING.len()
            } else {
                0
            };
            let cost = heading + format_kg_entity_line(h).len();
            if cost > remaining_budget.saturating_sub(used) {
                false
            } else {
                used += cost;
                true
            }
        })
        .collect()
}

fn format_kg_entities_markdown(entities: &[KgEntityHit]) -> String {
    let mut out = String::from(KG_ENTITIES_HEADING);
    for e in entities {
        out.push_str(&format_kg_entity_line(e));
    }
    out
}

fn format_compose_atom_heading(atom: &Atom) -> String {
    format!("\n## {}\n\nSource: {}\n", atom.name, atom.slug)
}

fn format_compose_section(section: &super::compose::ComposeSectionResult, explain: bool) -> String {
    let mut out = if explain {
        format!(
            "\n### {} (score: {:.4})\n\n",
            section.heading, section.score
        )
    } else {
        format!("\n### {}\n\n", section.heading)
    };
    if !section.content.is_empty() {
        out.push_str(&section.content);
        out.push('\n');
    }
    out
}

fn format_compose_whole_atom(atom: &Atom, score: f32, explain: bool) -> String {
    let mut out = format_compose_atom_heading(atom);
    if explain {
        out.push_str(&format!("Score: {score:.4}\n"));
    }
    if !atom.content.is_empty() {
        out.push('\n');
        out.push_str(&atom.content);
        out.push('\n');
    }
    out
}

fn format_compose_domain_footer(domains: &[Domain]) -> String {
    if domains.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = domains.iter().map(|d| d.name.as_str()).collect();
    format!("\n---\n\nDomains: {}\n", names.join(", "))
}

/// A composed body and the exact records that made it into that body.
struct PackedCompose<'a> {
    markdown: String,
    sections: Vec<&'a super::compose::ComposeSectionResult>,
    included_atom_ids: HashSet<String>,
}

/// Greedily pack ranked sections, then whole atoms that have no sections.
/// When no section fits, retain the historical whole-atom fallback. Costs use
/// the same fragments as rendering, including shared headings and metadata.
fn pack_compose_markdown<'a>(
    query: &str,
    domains: &[Domain],
    atoms: &'a [Atom],
    items: &[ScoredTextItem],
    sections: &'a [super::compose::ComposeSectionResult],
    explain: bool,
    char_budget: usize,
) -> PackedCompose<'a> {
    const PREFIX: &str = "# Knowledge Briefing\n\nQuery: ";
    let mut footer = format_compose_domain_footer(domains);
    if PREFIX.len() + 1 + footer.len() > char_budget {
        // Full domain metadata remains in `data.domains`; omit an oversized
        // display footer rather than letting it consume the entire briefing.
        footer.clear();
    }
    let query_budget = char_budget - PREFIX.len() - 1 - footer.len();
    let mut query_end = query.len().min(query_budget);
    while !query.is_char_boundary(query_end) {
        query_end -= 1;
    }
    let mut markdown = format!("{PREFIX}{}\n", &query[..query_end]);
    let mut body_used = 0usize;
    let body_budget = char_budget - markdown.len() - footer.len();
    let by_id: HashMap<String, &Atom> = atoms.iter().map(|a| (a.id.to_string(), a)).collect();
    let sectioned_atom_ids: HashSet<&str> = sections.iter().map(|s| s.atom_id.as_str()).collect();
    let mut selected_by_atom: HashMap<&str, Vec<&super::compose::ComposeSectionResult>> =
        HashMap::new();
    let mut selected_sections = Vec::new();
    let mut included_atom_ids = HashSet::new();
    for section in sections {
        let Some(atom) = by_id.get(&section.atom_id) else {
            continue;
        };
        let atom_header_cost = if selected_by_atom.contains_key(section.atom_id.as_str()) {
            0
        } else {
            format_compose_atom_heading(atom).len()
        };
        let cost = atom_header_cost + format_compose_section(section, explain).len();
        if cost > body_budget.saturating_sub(body_used) {
            continue;
        }
        body_used += cost;
        selected_by_atom
            .entry(section.atom_id.as_str())
            .or_default()
            .push(section);
        included_atom_ids.insert(section.atom_id.clone());
        selected_sections.push(section);
    }

    let mut whole_atoms = Vec::new();
    for item in items {
        if !selected_sections.is_empty() && sectioned_atom_ids.contains(item.id.as_str()) {
            continue;
        }
        let Some(atom) = by_id.get(&item.id) else {
            continue;
        };
        let fragment = format_compose_whole_atom(atom, item.score, explain);
        if fragment.len() > body_budget.saturating_sub(body_used) {
            continue;
        }
        body_used += fragment.len();
        included_atom_ids.insert(item.id.clone());
        whole_atoms.push(fragment);
    }

    // Sections retain their existing per-atom presentation order. The
    // sectionless whole-atom tail follows the atom rerank order.
    for atom in atoms {
        let atom_id = atom.id.to_string();
        if let Some(secs) = selected_by_atom.get(atom_id.as_str()) {
            markdown.push_str(&format_compose_atom_heading(atom));
            for section in secs {
                markdown.push_str(&format_compose_section(section, explain));
            }
        }
    }
    for fragment in whole_atoms {
        markdown.push_str(&fragment);
    }
    markdown.push_str(&footer);
    debug_assert_eq!(markdown.len(), char_budget - body_budget + body_used);
    PackedCompose {
        markdown,
        sections: selected_sections,
        included_atom_ids,
    }
}

#[cfg(test)]
#[path = "search/compose_packing_tests.rs"]
mod compose_packing_tests;

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

        let term_budget = FtsTermBudget::new();
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
            term_budget: &term_budget,
        };

        let ann_k = fetch_limit.max(20);
        let (
            SearchCoreOutcome {
                mut hits,
                lexical_timeouts,
                lexical_state,
            },
            CandidateStageOutcome {
                ann_hits,
                ann_availability,
                hydration_failures,
                mut query_embedding,
            },
        ) = run_candidate_stages(
            &ctx,
            token,
            ann,
            &raw_query,
            CandidateStageOptions {
                query_embedding: QueryEmbeddingCache::default(),
                ann_target: ann_k,
                ann_initial_k: ann_k,
                operation: "knowledge.search",
            },
            async {
                if do_decompose && non_stop_count >= decompose_threshold {
                    search_decomposed(&ctx, &raw_query, intersection_bonus).await
                } else {
                    search_core(&ctx, &raw_query, LexicalPass::Full).await
                }
            },
        )
        .await?;

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

        // The lexical stage now owns its own budget (issue #1930 Amendment
        // 2), so `lexical_timeout` no longer implies the request read
        // deadline is spent — only that stage's narrower budget is. Gate on
        // the live ambient deadline instead: a lexical-only degradation
        // with request time left to spare still gets its embedding rerank.
        let mut rerank_provenance = None;
        if do_rerank && !hits.is_empty() && !khive_storage::request_read_is_cancelled() {
            rerank_provenance = rerank_search_with_stored_vectors(
                runtime,
                token,
                &ns,
                &raw_query,
                &mut query_embedding,
                &mut hits,
                rerank_alpha,
            )
            .await?;
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
        let body_line_counts = if khive_storage::request_read_is_cancelled() {
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
                    "score_provenance": h.provenance.to_json(),
                })
            })
            .collect();
        let count = results.len();

        let mut out = json!({
            "results": results,
            "total": count,
            "candidate_provenance": {
                "lexical": lexical_state.as_str(),
                "fallback": candidate_fallback(&hits),
                "terms_truncated": term_budget.truncated(),
            },
        });
        if let Some(provenance) = rerank_provenance {
            out["rerank_provenance"] = provenance;
        }
        if ann_unavailable {
            out["ann_unavailable"] = json!(true);
        }
        attach_lexical_timeout_degradation(&mut out, &lexical_timeouts);
        if body_lines_timed_out {
            attach_body_lines_timeout_degradation(&mut out);
        }
        attach_hydration_degradation(&mut out, hydration_failures);
        // The lexical stage's own budget no longer implies the request
        // deadline is spent (issue #1930 Amendment 2), so this last check
        // re-reads the live ambient deadline rather than the stage-local
        // flags: skip only when the request has actually stopped, never a
        // verb-level error for a degradation already reported above.
        if !khive_storage::request_read_is_cancelled() {
            khive_storage::ensure_request_read_active("knowledge.search")?;
        }
        Ok(out)
    }

    /// Suggest domains with measured compose-member costs and live member counts.
    /// Present domains without live members have size zero and members zero.
    /// Every unmeasured domain is withheld under the stable
    /// `degraded.member_sizing_timeout.excluded` key, whether sizing timed out or not.
    pub(crate) async fn suggest(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let (out, _) = Self::suggest_with_query_embedding(
            runtime,
            token,
            params,
            ann,
            QueryEmbeddingCache::default(),
        )
        .await?;
        Ok(out)
    }

    /// Internal suggest path that accepts and returns the request's cached
    /// query vector. Auto-compose calls this directly so its suggest, atom,
    /// section, and KG stages all share one successful query embedding.
    async fn suggest_with_query_embedding(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
        query_embedding: QueryEmbeddingCache,
    ) -> Result<(Value, QueryEmbeddingCache), RuntimeError> {
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

        let term_budget = FtsTermBudget::new();
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
            term_budget: &term_budget,
        };

        // Over-fetch aggressively: the corpus is ~27% domains / ~73% atoms, so
        // limit*3 would return mostly atoms that all get dropped after type filtering.
        // 50× over-fetch (floor 200) gives domains a fair chance to appear in the
        // top ANN neighbors before the type gate discards atom hits.
        let ann_k = (limit * 50).max(200);
        let (
            SearchCoreOutcome {
                mut hits,
                lexical_timeouts,
                ..
            },
            CandidateStageOutcome {
                ann_hits,
                ann_availability,
                hydration_failures,
                mut query_embedding,
            },
        ) = run_candidate_stages(
            &ctx,
            token,
            ann,
            &raw_query,
            CandidateStageOptions {
                query_embedding,
                ann_target: ctx.fetch_limit,
                ann_initial_k: ann_k,
                operation: "knowledge.suggest",
            },
            search_core(&ctx, &raw_query, LexicalPass::Full),
        )
        .await?;

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

        // The lexical stage now owns its own budget (issue #1930 Amendment
        // 2), so `lexical_timeout` no longer implies the request read
        // deadline is spent — only that stage's narrower budget is. Gate on
        // the live ambient deadline instead: a lexical-only degradation
        // with request time left to spare still gets its embedding rerank.
        let fresh_rerank_applied = if khive_storage::request_read_is_cancelled() {
            false
        } else {
            rerank_with_embeddings(
                runtime,
                &raw_query,
                &mut query_embedding,
                &mut hits,
                D_SUGGEST_RERANK_ALPHA,
            )
            .await?
        };

        // Safety net: retain only domain hits in case any non-domain survived above.
        hits.retain(|h| h.is_domain);
        hits.truncate(limit);

        let domain_ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
        // A cancelled ambient deadline skips the call the same way an
        // internal timeout inside it does — both leave every domain in this
        // batch unmeasured, never a measured member cost
        // (issue #2396 fix 3).
        let (member_token_sizes, member_sizing_timed_out) =
            if khive_storage::request_read_is_cancelled() {
                (HashMap::new(), !domain_ids.is_empty())
            } else {
                #[cfg(test)]
                let measured = if MEMBER_SIZING_READ_TIMEOUT.try_with(|_| ()).is_ok() {
                    khive_storage::scope_request_read_deadline(
                        std::time::Duration::ZERO,
                        load_domain_member_token_sizes(runtime, &ns, &domain_ids),
                    )
                    .await
                } else {
                    load_domain_member_token_sizes(runtime, &ns, &domain_ids).await
                };
                #[cfg(not(test))]
                let measured = load_domain_member_token_sizes(runtime, &ns, &domain_ids).await;
                measured?
            };
        if !khive_storage::request_read_is_cancelled() {
            khive_storage::ensure_request_read_active("knowledge.suggest")?;
        }
        // A domain the sizing pass returned no row for is unmeasured as well:
        // a missing entry never defaults to a fabricated zero.
        let unmeasured_domain_ids: HashSet<&str> = domain_ids
            .iter()
            .map(String::as_str)
            .filter(|id| member_sizing_timed_out || !member_token_sizes.contains_key(*id))
            .collect();

        // Price the member atom bodies that compose expands, not the much smaller
        // domain mirror description used for retrieval. The batched join keeps the
        // suggest -> fold budget in compose's estimated-token unit without an N+1
        // hydration pass. A domain whose members were not measured is withheld
        // from `results` entirely and reported under
        // `degraded.member_sizing_timeout.excluded` instead — `suggest`'s
        // documented contract (issue #105) is that `results` feeds
        // `knowledge.fold`'s `candidates` unmodified, and `FoldCandidate::size`
        // is a non-optional `usize`, so leaving an unpriced item in `results`
        // (as `size: null`, issue #2396 fix 3) turned one unmeasured domain into
        // a hard parse error for the whole fold request. Exclusion keeps the
        // passthrough valid while still refusing to let an unpriced domain enter
        // a budgeted fold selection for free.
        let results: Vec<Value> = hits
            .iter()
            .filter(|h| !unmeasured_domain_ids.contains(h.id.as_str()))
            .filter_map(|h| {
                let sizing = member_token_sizes.get(&h.id)?;
                Some(json!({
                    "id": h.id,
                    "name": h.name,
                    "score": h.score,
                    "size": sizing.tokens,
                    "members": sizing.live_members,
                }))
            })
            .collect();
        let count = results.len();
        let excluded: Vec<Value> = hits
            .iter()
            .enumerate()
            .filter(|(_, h)| unmeasured_domain_ids.contains(h.id.as_str()))
            .map(|(i, h)| {
                json!({
                    "id": h.id,
                    "name": h.name,
                    "rank": i + 1,
                    "score": h.score,
                })
            })
            .collect();

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
        attach_lexical_timeout_degradation(&mut out, &lexical_timeouts);
        attach_hydration_degradation(&mut out, hydration_failures);
        attach_member_sizing_timeout_degradation(&mut out, &excluded);
        // The lexical stage's own budget no longer implies the request
        // deadline is spent (issue #1930 Amendment 2), so this last check
        // re-reads the live ambient deadline rather than the stage-local
        // flag: skip only when the request has actually stopped, never a
        // verb-level error for a degradation already reported above.
        if !khive_storage::request_read_is_cancelled() {
            khive_storage::ensure_request_read_active("knowledge.suggest")?;
        }
        Ok((out, query_embedding))
    }

    const COMPOSE_ATOM_CHUNK_SIZE: usize = 64;

    fn compose_domain_member_ids(atoms: &[Atom], members: &[String]) -> HashSet<String> {
        let mut first_by_slug = HashMap::with_capacity(atoms.len());
        for atom in atoms {
            first_by_slug.entry(atom.slug.as_str()).or_insert(atom.id);
        }
        members
            .iter()
            .filter_map(|slug| first_by_slug.get(slug.as_str()))
            .map(|id| id.to_string())
            .collect()
    }

    async fn load_compose_atom_window(
        runtime: &KhiveRuntime,
        ns: &str,
        references: &[String],
    ) -> Result<Vec<Result<Atom, RuntimeError>>, RuntimeError> {
        if references.is_empty() {
            return Ok(Vec::new());
        }
        assert!(references.len() <= Self::COMPOSE_ATOM_CHUNK_SIZE);
        khive_storage::ensure_request_read_active("knowledge.compose")?;
        let raw: Vec<_> = references
            .iter()
            .map(|reference| reference.trim())
            .collect();
        let mut inputs = Vec::with_capacity(raw.len());
        let mut params = vec![SqlValue::Text(ns.to_owned())];
        for (ordinal, reference) in raw.iter().enumerate() {
            let is_uuid = Uuid::parse_str(reference).is_ok();
            let is_prefix = !is_uuid
                && reference.len() >= 8
                && reference.len() <= 36
                && reference
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() || character == '-');
            let first = params.len() + 1;
            inputs.push(format!(
                "({ordinal},?{first},{},?{})",
                usize::from(is_uuid),
                first + 1
            ));
            params.push(SqlValue::Text((*reference).to_owned()));
            params.push(if is_prefix {
                SqlValue::Text(format!("{}%", hex_prefix_to_uuid_pattern(reference)))
            } else {
                SqlValue::Null
            });
        }
        let statement = SqlStatement {
            sql: format!(
                "WITH input(ordinal,raw_ref,is_uuid,prefix) AS (VALUES {}) \
                 SELECT input.ordinal AS compose_ordinal, \
                    CASE WHEN input.is_uuid=0 AND NOT EXISTS( \
                        SELECT 1 FROM knowledge_atoms slug WHERE slug.slug=input.raw_ref \
                        AND slug.namespace=?1 AND slug.deleted_at IS NULL) THEN 1 ELSE 0 END AS compose_prefix, atom.* \
                 FROM input JOIN knowledge_atoms atom ON \
                    atom.rowid IN(SELECT by_id.rowid FROM knowledge_atoms by_id \
                        WHERE input.is_uuid=1 AND by_id.id=input.raw_ref \
                        AND by_id.namespace=?1 AND by_id.deleted_at IS NULL \
                        UNION ALL SELECT by_slug.rowid FROM knowledge_atoms by_slug \
                        WHERE input.is_uuid=0 AND by_slug.slug=input.raw_ref \
                        AND by_slug.namespace=?1 AND by_slug.deleted_at IS NULL LIMIT 1) \
                    OR atom.rowid IN(SELECT prefixed.rowid FROM knowledge_atoms prefixed \
                        WHERE input.is_uuid=0 AND input.prefix IS NOT NULL \
                        AND NOT EXISTS(SELECT 1 FROM knowledge_atoms slug \
                            WHERE slug.slug=input.raw_ref AND slug.namespace=?1 AND slug.deleted_at IS NULL) \
                        AND prefixed.namespace=?1 AND prefixed.deleted_at IS NULL \
                        AND prefixed.id LIKE input.prefix LIMIT 2) \
                 ORDER BY input.ordinal",
                inputs.join(",")
            ),
            params,
            label: Some("compose_atom_window".into()),
        };
        let context = if Uuid::parse_str(raw[0]).is_ok() {
            "compose atom by id"
        } else {
            "compose atom by slug"
        };
        let access = runtime.sql();
        let mut reader = access
            .reader()
            .await
            .map_err(|error| sql_err("compose atom reader", error))?;
        #[cfg(test)]
        compose_read_tests::observe_reader();
        #[cfg(test)]
        let before = runtime
            .backend()
            .pool()
            .reader_acquisition_snapshot()
            .pooled_checkouts;
        let rows = reader
            .query_all(statement)
            .await
            .map_err(|error| sql_err(context, error))?;
        #[cfg(test)]
        compose_read_tests::observe_query(runtime, before);
        let mut aligned: Vec<Vec<khive_storage::types::SqlRow>> =
            (0..raw.len()).map(|_| Vec::new()).collect();
        for row in rows {
            let ordinal = row_i64(&row, "compose_ordinal")
                .and_then(|ordinal| usize::try_from(ordinal).ok())
                .filter(|ordinal| *ordinal < aligned.len())
                .ok_or_else(|| {
                    RuntimeError::Internal("invalid compose atom window ordinal".into())
                })?;
            aligned[ordinal].push(row);
        }
        let outcomes = raw
            .iter()
            .zip(aligned)
            .map(|(reference, rows)| {
                if rows.len() > 1
                    && rows.first().and_then(|row| row_i64(row, "compose_prefix")) == Some(1)
                {
                    return Err(RuntimeError::InvalidInput(format!(
                        "ambiguous atom prefix {reference:?} matches multiple atoms"
                    )));
                }
                rows.into_iter()
                    .next()
                    .and_then(|row| atom_from_row(&row))
                    .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {reference:?}")))
            })
            .collect();
        #[cfg(test)]
        compose_read_tests::pause_after_window().await;
        Ok(outcomes)
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
        let mut query_embedding = QueryEmbeddingCache::default();
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
            let suggest_attempt = Self::suggest_with_query_embedding(
                runtime,
                token,
                json!({ "query": &raw_query, "limit": auto_limit }),
                ann,
                std::mem::take(&mut query_embedding),
            )
            .await;
            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));
            let suggest_result = match suggest_attempt {
                Ok((v, reused_query_embedding)) => {
                    query_embedding = reused_query_embedding;
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
                for r in results.iter().filter(|r| r["members"] != 0) {
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
        let mut omitted_members: Vec<String> = Vec::new();

        for window in member_slugs.chunks(Self::COMPOSE_ATOM_CHUNK_SIZE) {
            let outcomes =
                try_or_finish!(Self::load_compose_atom_window(runtime, &ns, window).await);
            for (slug, outcome) in window.iter().zip(outcomes) {
                try_or_finish!(khive_storage::ensure_request_read_active(
                    "knowledge.compose"
                ));
                let atom = match outcome {
                    Ok(atom) => atom,
                    Err(RuntimeError::NotFound(_)) => {
                        omitted_members.push(slug.clone());
                        continue;
                    }
                    Err(error) => {
                        timing.finish(0);
                        return Err(error);
                    }
                };
                if seen_ids.insert(atom.id.to_string()) {
                    ordered_atoms.push(atom);
                }
            }
        }
        for window in atom_ids.chunks(Self::COMPOSE_ATOM_CHUNK_SIZE) {
            let outcomes =
                try_or_finish!(Self::load_compose_atom_window(runtime, &ns, window).await);
            for outcome in outcomes {
                try_or_finish!(khive_storage::ensure_request_read_active(
                    "knowledge.compose"
                ));
                let atom = try_or_finish!(outcome);
                if seen_ids.insert(atom.id.to_string()) {
                    ordered_atoms.push(atom);
                }
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
            if !omitted_members.is_empty() {
                data["omissions"] = json!(omitted_members);
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
        // The KG blend below requires a role-specific vector (search_kg_entities
        // gates on it, never a generic one — #2307). When this compose call can
        // reach the blend, attempt embed_query directly so a real failure keeps
        // the blend on its degradation path; rerank_text_items's own
        // combined-batch fallback still covers the failure case below. When the
        // blend cannot run anyway (blend_kg is off, or atom_ids_only), skip the
        // solo call and let rerank_text_items embed query + candidates in one
        // combined batch, matching the pre-cache single-job cost.
        if blend_kg
            && query_embedding.role_specific.is_not_attempted()
            && !runtime.default_embedder_name().is_empty()
        {
            let embedded = try_or_finish!(
                khive_storage::await_request_read_phase(
                    "knowledge.compose",
                    runtime.embed_query(&raw_query),
                )
                .await
            );
            query_embedding.role_specific = match embedded {
                Ok(v) => RoleSpecificEmbedding::Vector(v),
                Err(_) => RoleSpecificEmbedding::Failed,
            };
        }
        try_or_finish!(
            rerank_text_items(runtime, &raw_query, &mut query_embedding, &mut items,).await
        );

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

        let section_results = if has_sections {
            let domain_member_ids = Self::compose_domain_member_ids(&ordered_atoms, &member_slugs);

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

            try_or_finish!(khive_storage::ensure_request_read_active(
                "knowledge.compose"
            ));

            if let Some(qe) = query_embedding.any() {
                try_or_finish!(super::compose::score_sections(
                    &raw_query,
                    qe,
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

        let packed = pack_compose_markdown(
            &raw_query,
            &resolved_domains,
            &ordered_atoms,
            &items,
            &section_results,
            explain,
            char_budget,
        );
        let section_json: Vec<Value> = if explain {
            packed
                .sections
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
        let entity_score_floor: Option<f32> = packed
            .included_atom_ids
            .iter()
            .filter_map(|id| atom_cosine_scores.get(id).copied())
            .fold(None, |acc, s| Some(acc.map_or(s, |a: f32| a.min(s))));
        let mut markdown = packed.markdown;
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
                    &mut query_embedding,
                    KG_BLEND_CAP,
                    floor,
                )
                .await
                {
                    Ok(kg_hits) => {
                        let remaining_budget = char_budget.saturating_sub(markdown.len());
                        let kg_hits = trim_kg_entities_to_budget(kg_hits, remaining_budget);
                        if !kg_hits.is_empty() {
                            let kg_markdown = format_kg_entities_markdown(&kg_hits);
                            if kg_markdown.len() <= remaining_budget {
                                markdown.push_str(&kg_markdown);
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
            .filter(|item| packed.included_atom_ids.contains(&item.id))
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
        if !omitted_members.is_empty() {
            data["omissions"] = json!(omitted_members);
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
#[path = "compose_read_tests.rs"]
mod compose_read_tests;

#[cfg(test)]
#[path = "lexical_timeout_tests.rs"]
mod lexical_timeout_tests;

#[cfg(all(test, feature = "namespace-trigram-proto"))]
#[path = "namespace_trigram_proto_tests.rs"]
mod namespace_trigram_proto_tests;

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
