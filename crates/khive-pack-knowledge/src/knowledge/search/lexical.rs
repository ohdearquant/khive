use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use uuid::Uuid;

use super::super::lexical_timeout::{LexicalBound, LexicalPhase, LexicalStage, LexicalTimeout};
use super::super::matching;
use super::super::schema::Atom;
use super::super::scoring::expand_terms;
use super::super::sections::to_slug;
#[cfg(all(test, feature = "namespace-trigram-proto"))]
use super::super::util::row_str;
use super::super::util::{
    atom_from_row, is_stop, row_i64, sql_err, status_sql_clause, MIN_TERM_LEN,
};
#[cfg(test)]
use super::tests;
use super::{phase_b_hydration_statement, HYDRATION_ID_CHUNK, LEXICAL_STAGE_BUDGET_MS};

#[path = "lexical_sql.rs"]
mod lexical_sql;

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
pub(super) fn fts5_candidate_terms(raw_query: &str) -> Vec<String> {
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
pub(super) enum NamespaceTrigramExperiment {
    SlotTable { key: String },
    Prefixed { key: String },
}

#[cfg(all(test, feature = "namespace-trigram-proto"))]
tokio::task_local! {
    pub(super) static NAMESPACE_TRIGRAM_EXPERIMENT: NamespaceTrigramExperiment;
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
/// Domain mirrors use the shared decoded-tag and legacy-text classifier. Applying
/// this predicate in FTS hydration and recovery is load-bearing: filtering after
/// `LIMIT` lets the wrong kind consume every candidate slot.
fn type_eligibility_sql(type_filter: Option<&str>, atom_alias: &str) -> String {
    match type_filter {
        Some("domain") => format!(" AND khive_tag_contains({atom_alias}.tags, 'type:domain')"),
        Some(filter) if !filter.is_empty() => {
            format!(" AND NOT khive_tag_contains({atom_alias}.tags, 'type:domain')")
        }
        _ => String::new(),
    }
}

// ─── FTS5 candidate pool fetch ────────────────────────────────────────────────

/// Per-term candidate cap. FTS enumerates in rowid order and stops at the
/// limit; application TF-IDF scoring ranks only the admitted candidates.
pub(super) const FTS_TERM_LIMIT: usize = 500;

/// Shared by the full query and both decomposed passes. Admission happens
/// before rarity probes; every later stage reuses that admitted term set.
pub(super) const FTS_TERM_COUNT_LIMIT: usize = 32;

pub(super) struct FtsTermBudget {
    remaining: AtomicUsize,
    truncated: AtomicBool,
}

impl FtsTermBudget {
    pub(super) fn new() -> Self {
        Self {
            remaining: AtomicUsize::new(FTS_TERM_COUNT_LIMIT),
            truncated: AtomicBool::new(false),
        }
    }

    pub(super) fn admit(&self, mut terms: Vec<String>) -> Vec<String> {
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

    pub(super) fn truncated(&self) -> bool {
        self.truncated.load(Ordering::Relaxed)
    }

    /// Count only terms this pass can actually admit. Decomposed passes share
    /// the request allowance, so a pass after exhaustion must not acquire a
    /// larger deadline just because its raw query contains many terms.
    pub(super) fn available_for(&self, raw_query: &str) -> usize {
        fts5_candidate_terms(raw_query)
            .len()
            .min(self.remaining.load(Ordering::Relaxed))
    }
}

pub(super) fn phase_a_rowids_statement(term: &str, limit: usize) -> SqlStatement {
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
        sql: khive_runtime::sql!("knowledge_search_fts_rowids").into(),
        params: vec![SqlValue::Text(term.into()), SqlValue::Integer(limit as i64)],
        label: Some("knowledge.fts_rowids".into()),
    }
}

pub(super) fn term_frequency_statement(term: &str) -> SqlStatement {
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
        sql: khive_runtime::sql!("knowledge_search_fts_term_frequency").into(),
        params: vec![
            SqlValue::Text(term.into()),
            SqlValue::Integer((FTS_TERM_LIMIT + 1) as i64),
        ],
        label: Some("knowledge.fts_term_frequency".into()),
    }
}

pub(super) async fn rarest_fts_terms_first(
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
pub(super) const PHASE_A_OVERFETCH_FACTOR: usize = 4;

/// Ceiling a single term's phase-A probe window is allowed to widen to
/// (issue #1930 Amendment 2). Bounds the worst case — every phase-A row
/// ineligible, and the corpus large enough to keep returning full pages — to
/// a fixed per-term cost instead of an unbounded retry loop.
const PHASE_A_WIDEN_CEILING: usize = 8000;

/// The fixed 2 s stage allowance leaves the same time for one or many
/// sequential bounded term reads. Give each additional admitted term one
/// quarter of the base budget, capped at 4x for a pass. The request's outer
/// read deadline remains authoritative (normally 30 s), and the 32-term
/// request-wide admission bound still limits actual FTS work.
const LEXICAL_STAGE_EXTRA_TERM_QUARTERS: usize = 12;

pub(super) fn lexical_stage_budget_for_terms(
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
    pub(super) static MEMBER_SIZING_READ_TIMEOUT: ();
}

#[cfg(test)]
pub(crate) async fn with_member_sizing_read_timeout<F: std::future::Future>(
    future: F,
) -> F::Output {
    MEMBER_SIZING_READ_TIMEOUT.scope((), future).await
}

pub(super) fn lexical_stage_budget() -> std::time::Duration {
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
pub(super) enum LexicalCandidateState {
    Matched,
    ExactName,
    NoMatch,
    Filtered,
    PartialTimeout,
    TimedOut,
}

impl LexicalCandidateState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::ExactName => "exact_name",
            Self::NoMatch => "no_match",
            Self::Filtered => "filtered",
            Self::PartialTimeout => "partial_timeout",
            Self::TimedOut => "timed_out",
        }
    }

    pub(super) fn merge(states: &[Self]) -> Self {
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
pub(super) struct FtsFetchOutcome {
    pub(super) atoms: Vec<Atom>,
    pub(super) timeout: Option<LexicalTimeout>,
    pub(super) state: LexicalCandidateState,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) struct FtsTestDeadlineAdvance {
    pub(super) after_completed_terms: usize,
    pub(super) by: std::time::Duration,
}

#[cfg(test)]
tokio::task_local! {
    pub(super) static FTS_TEST_DEADLINE_ADVANCE: FtsTestDeadlineAdvance;
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

pub(super) fn is_timeout(e: &khive_storage::StorageError) -> bool {
    matches!(e, khive_storage::StorageError::Timeout { .. })
}

/// Same check, for the `RuntimeError` shape `?`-propagated storage timeouts
/// arrive in at the handler layer.
pub(super) fn is_read_timeout(e: &RuntimeError) -> bool {
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
pub(super) async fn fetch_fts_candidates(
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
                    sql: khive_runtime::sql!("knowledge_search_namespace_key").into(),
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
                // ever probed. Recover from the namespace index and probe FTS
                // membership for each local row, retaining this term's eligible
                // rowid order and `per_term_limit`.
                let (scoped_status_clause, scoped_status_params) =
                    status_sql_clause(statuses, exclude_statuses, 4);
                let scoped_sql =
                    lexical_sql::eligibility_fallback_sql(&scoped_status_clause, &type_clause);
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
                sql: khive_runtime::sql!("knowledge_search_fts_namespace_present").into(),
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

pub(super) fn exact_name_statement(
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
pub(super) async fn fetch_exact_name_candidate(
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
pub(super) enum ExactNameProbe {
    Hit(Box<Atom>),
    Miss,
    Filtered,
    TimedOut,
}
