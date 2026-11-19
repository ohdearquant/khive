//! Search, suggest, and compose handlers.
//!
//! TF-IDF scoring primitives live in `super::scoring`; this module owns the
//! FTS/ANN pipeline, reranking, hydration, and handler dispatch.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use uuid::Uuid;

#[cfg(test)]
use khive_runtime::hex_prefix_to_uuid_pattern;
#[cfg(test)]
use khive_runtime::Namespace;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_score::vector::cosine_similarity;
use khive_score::DeterministicScore;
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_storage::EntityFilter;

use super::compose;
use super::lexical_timeout::{LexicalBound, LexicalPass, LexicalTimeout};
#[cfg(test)]
use super::lexical_timeout::{LexicalPhase, LexicalStage};
use super::schema::{Atom, ComposeParams, Domain, SearchParams, SuggestParams};
use super::scoring::Weights;
#[cfg(test)]
use super::util::CANDIDATE_POOL;
use super::util::{
    atom_embed_text, atom_embed_text_fields, atom_from_row, deser, domain_from_row,
    estimate_compose_item_tokens, explicitly_requested_status, has_domain_mirror_tag, is_stop,
    row_bool, row_i64, row_str, sql_err, status_multiplier, status_sql_clause, status_values,
    CHARS_PER_TOKEN, D_SUGGEST_RERANK_ALPHA, MIN_TERM_LEN, SERVABLE_SECTION,
};
use super::vamana;
use super::KnowledgeHandlers;

mod ann_search;
use ann_search::{
    apply_status_multipliers, candidate_fallback, deprecated_allowed_by_status_policy,
    enforce_min_score_floor, filter_hits_by_status, filter_hits_by_type, fuse_ann_hits,
    search_ann_with_warm_wait, AnnAvailability, AnnSearchState, ScoreProvenance,
};
#[cfg(test)]
use ann_search::{filter_by_excluded_statuses, normalize_rrf_score, RRF_K};

mod compose_packing;
use compose_packing::{
    format_kg_entities_markdown, pack_compose_markdown, trim_kg_entities_to_budget,
};

mod lexical;
#[cfg(test)]
use lexical::{
    exact_name_statement, fetch_exact_name_candidate, fts5_candidate_terms,
    phase_a_rowids_statement, rarest_fts_terms_first, term_frequency_statement, ExactNameProbe,
    FtsTestDeadlineAdvance, FTS_TERM_COUNT_LIMIT, FTS_TERM_LIMIT, FTS_TEST_DEADLINE_ADVANCE,
    MEMBER_SIZING_READ_TIMEOUT, PHASE_A_OVERFETCH_FACTOR,
};
use lexical::{
    fetch_fts_candidates, is_read_timeout, is_timeout, lexical_stage_budget,
    lexical_stage_budget_for_terms, FtsFetchOutcome, FtsTermBudget, LexicalCandidateState,
};
#[cfg(test)]
pub(crate) use lexical::{
    with_fts_deadline_advance_after_term, with_fts_probe_deadline_advance_after_term,
    with_lexical_stage_budget_override_ms, with_member_sizing_read_timeout,
    with_phase_a_widen_ceiling_override,
};
#[cfg(all(test, feature = "namespace-trigram-proto"))]
use lexical::{NamespaceTrigramExperiment, NAMESPACE_TRIGRAM_EXPERIMENT};

mod search_core;
#[cfg(test)]
use search_core::merge_decomposed_hits;
use search_core::{search_core, search_decomposed, SearchCoreOutcome, SearchCtx};

mod hydration;
use hydration::{
    attach_body_lines_timeout_degradation, attach_hydration_degradation,
    attach_lexical_timeout_degradation, attach_member_sizing_timeout_degradation,
    phase_b_hydration_statement, run_candidate_stages, CandidateStageOptions,
    CandidateStageOutcome,
};
#[cfg(test)]
use hydration::{hydrate_atoms_statement, hydrate_domains_statement, hydrate_empty_hits};

mod compose_helpers;
#[cfg(test)]
use compose_helpers::load_atom_by_id_or_slug;
use compose_helpers::{
    load_atom_body_line_counts, load_domain_by_id_or_slug, load_domain_member_token_sizes,
    parse_domain_members, rerank_text_items, search_kg_entities, KgEntityHit, ScoredTextItem,
    KG_BLEND_CAP,
};

mod handlers;
mod rerank;

use rerank::*;

/// Independent read-deadline budget for the lexical/FTS candidate fetch
/// (issue #1930 Amendment 2). `khive_storage::scope_request_read_deadline`
/// keeps whichever deadline is earlier, so nesting this around just the
/// fetch only ever *tightens* its effective deadline; popping back out of
/// the scope once the fetch returns restores the wider request deadline for
/// every stage that runs after it (rerank, body-line counts, member
/// sizing). A lexical-stage timeout therefore no longer means the request
/// itself is out of time — only that this one stage's own budget is.
pub(crate) const LEXICAL_STAGE_BUDGET_MS: u64 = 2_000;

// Keep one namespace bind plus the ID binds comfortably below SQLite's
// portable 999-variable ceiling.
pub(super) const HYDRATION_ID_CHUNK: usize = 900;

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

fn compare_scored_hits(a: &ScoredHit, b: &ScoredHit) -> std::cmp::Ordering {
    khive_score::cmp_desc_then_id_f32(a.score, &a.slug, b.score, &b.slug)
}

#[cfg(test)]
#[path = "search/compose_packing_tests.rs"]
mod compose_packing_tests;

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

#[cfg(test)]
#[path = "search/ordering_tests.rs"]
mod ordering_tests;
