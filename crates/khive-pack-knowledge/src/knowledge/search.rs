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
use khive_runtime::Namespace;
use khive_runtime::{hex_prefix_to_uuid_pattern, KhiveRuntime, NamespaceToken, RuntimeError};
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
            .map(|emb| cosine_similarity(query_emb, emb, false))
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
        hits.sort_by(compare_scored_hits);
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
        let cosine = cosine_similarity(query_vector, &vector, false);
        hit.score = alpha * (hit.score / max_score) + (1.0 - alpha) * cosine.max(0.0);
        hit.provenance.embedding_rerank = true;
    }
    hits.sort_by(compare_scored_hits);
    Ok(Some(json!({
        "stored_vector_lookup": stored_vector_lookup,
        "candidates": hits.len(),
        "from_stored": from_stored,
        "embedded_fallback": fallback_indices.len(),
    })))
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
