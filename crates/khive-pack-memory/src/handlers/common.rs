//! Shared types, utilities, and pipeline helpers for the memory verb handlers.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use khive_fusion::FusionStrategy;
use khive_retrieval::{fuse_search_results, HybridConfig};
use khive_runtime::{
    MemoryRecallPipeline, NamespaceToken, NoteCandidate, RuntimeError, SearchHit, SearchSource,
};
use khive_score::DeterministicScore;
use khive_storage::types::{
    TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, VectorSearchHit,
    VectorSearchRequest,
};
use khive_types::SubstrateKind;

use crate::ann::{self, AnnKey};
use crate::config::{RecallConfig, ScoreBreakdown, WeightedContributions};
use crate::query_cache::QueryEmbeddingCache;
use crate::MemoryPack;

// ---------------------------------------------------------------------------
// Per-call stage profiling, gated by KHIVE_RECALL_PROFILE=1.
// Emits JSON lines to stderr: {"c":<call_id>,"s":<stage>,"us":<microseconds>}
// ---------------------------------------------------------------------------
pub(super) static RECALL_CALL_ID: AtomicU64 = AtomicU64::new(0);

thread_local! {
    pub(super) static PROF_CID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(super) fn recall_profile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("KHIVE_RECALL_PROFILE").is_ok())
}

pub(super) fn ann_overfetch_max_rounds() -> usize {
    static ROUNDS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *ROUNDS.get_or_init(|| {
        std::env::var("ANN_OVERFETCH_MAX_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3)
    })
}

#[inline(always)]
pub(super) fn plog(call_id: u64, stage: &str, us: u128) {
    eprintln!(r#"{{"c":{},"s":"{}","us":{}}}"#, call_id, stage, us);
}

#[inline(always)]
pub(super) fn plog_n(call_id: u64, stage: &str, us: u128, n: usize) {
    eprintln!(
        r#"{{"c":{},"s":"{}","us":{},"n":{}}}"#,
        call_id, stage, us, n
    );
}

/// Embed one query string for one model, checking the pack-local LRU cache first.
///
/// Uses query-side instruction prefix so instruction-tuned models (e.g.
/// multilingual-e5) land in the correct retrieval space. For models with no
/// query instruction this is identical to the generic embed path.
pub(super) async fn embed_query_model(
    runtime: khive_runtime::KhiveRuntime,
    cache: QueryEmbeddingCache,
    model_name: String,
    query: String,
) -> Result<(String, Vec<f32>), RuntimeError> {
    if let Some(v) = cache.get(&model_name, &query) {
        return Ok((model_name, v));
    }
    let handle = tokio::runtime::Handle::current();
    let model_name_blk = model_name.clone();
    let query_blk = query.clone();
    let v = tokio::task::spawn_blocking(move || {
        handle.block_on(runtime.embed_query_with_model(&model_name_blk, &query_blk))
    })
    .await
    .map_err(|e| RuntimeError::Internal(format!("recall embed task panicked: {e}")))??;
    cache.put(&model_name, &query, v.clone());
    Ok((model_name, v))
}

pub(super) fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RuntimeError> {
    serde_json::to_value(v).map_err(|e| RuntimeError::InvalidInput(e.to_string()))
}

pub(super) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params).map_err(|e| RuntimeError::InvalidInput(e.to_string()))
}

pub(super) fn validate_memory_type(mt: &str) -> Result<(), RuntimeError> {
    match mt {
        "episodic" | "semantic" => Ok(()),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown memory_type {other:?}; valid: episodic | semantic"
        ))),
    }
}

pub(super) fn parse_fusion_strategy_str(s: &str) -> Result<FusionStrategy, RuntimeError> {
    match s {
        "rrf" => Ok(FusionStrategy::Rrf { k: 60 }),
        "weighted" => Ok(FusionStrategy::Weighted {
            weights: vec![0.3, 0.7],
        }),
        "union" => Ok(FusionStrategy::Union),
        "vector_only" => Ok(FusionStrategy::VectorOnly),
        "keyword_only" => Ok(FusionStrategy::KeywordOnly),
        other => Err(RuntimeError::InvalidInput(format!(
            "invalid fusion_strategy {other:?}: must be one of \"rrf\", \"weighted\", \"union\", \"vector_only\", \"keyword_only\""
        ))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RememberParams {
    pub(super) content: String,
    pub(super) memory_type: Option<String>,
    pub(super) salience: Option<f64>,
    #[serde(alias = "decay")]
    pub(super) decay_factor: Option<f64>,
    #[serde(alias = "source")]
    pub(super) source_id: Option<String>,
    pub(super) tags: Option<Vec<String>>,
    #[serde(default)]
    pub(super) embedding_model: Option<String>,
}

/// Tag filter mode: `any` = OR, `all` = AND.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum TagMode {
    #[default]
    Any,
    All,
}

pub(super) fn note_matches_tags(props: Option<&Value>, expected: &[String], mode: TagMode) -> bool {
    let Some(stored) = props
        .and_then(|p| p.get("tags"))
        .and_then(|tags| tags.as_array())
    else {
        return false;
    };
    let stored: HashSet<&str> = stored.iter().filter_map(Value::as_str).collect();
    match mode {
        TagMode::Any => expected.iter().any(|tag| stored.contains(tag.as_str())),
        TagMode::All => expected.iter().all(|tag| stored.contains(tag.as_str())),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecallParams {
    pub(super) query: String,
    pub(super) limit: Option<u32>,
    pub(super) memory_type: Option<String>,
    pub(super) min_score: Option<f64>,
    pub(super) min_salience: Option<f64>,
    pub(super) config: Option<RecallConfig>,
    pub(super) top_k: Option<usize>,
    pub(super) fusion_strategy: Option<String>,
    pub(super) score_floor: Option<f32>,
    #[serde(default)]
    pub(super) embedding_model: Option<String>,
    #[serde(default)]
    pub(super) include_breakdown: Option<bool>,
    #[serde(default)]
    pub(super) tags: Option<Vec<String>>,
    #[serde(default)]
    pub(super) tag_mode: TagMode,
    /// Entity names to boost in scoring.
    #[serde(default)]
    pub(super) entity_names: Option<Vec<String>>,
    #[serde(default)]
    pub(super) full_content: Option<bool>,
}

impl RecallParams {
    pub(super) fn effective_config(&self, base: RecallConfig) -> RecallConfig {
        let mut cfg = self.config.clone().unwrap_or(base);
        if let Some(ms) = self.min_score {
            cfg.min_score = ms;
        }
        if let Some(ms) = self.min_salience {
            cfg.min_salience = ms;
        }
        cfg
    }
}

/// Normalize a raw fusion score to the [0, 1] range.
pub(super) fn normalize_relevance(raw: f64, strategy: &FusionStrategy) -> f64 {
    match strategy {
        FusionStrategy::Rrf { k } => (raw * (*k as f64 + 1.0)).min(1.0),
        _ => raw,
    }
}

/// Salience amplifier exponent applied to `effective_salience` in `compute_score`.
pub(super) const SALIENCE_AMPLIFIER_ALPHA: f64 = 1.5;

/// Default salience for episodic memories (session events; decay quickly).
pub(super) const DEFAULT_SALIENCE_EPISODIC: f64 = 0.3;
/// Default salience for semantic memories (durable facts; stronger base weight).
pub(super) const DEFAULT_SALIENCE_SEMANTIC: f64 = 0.5;
/// Default decay_factor for episodic memories (~35-day half-life).
pub(super) const DEFAULT_DECAY_EPISODIC: f64 = 0.02;
/// Default decay_factor for semantic memories (~139-day half-life).
pub(super) const DEFAULT_DECAY_SEMANTIC: f64 = 0.005;

pub(super) fn compute_score(
    cfg: &RecallConfig,
    pipeline: &MemoryRecallPipeline,
    raw_relevance: f64,
    salience: f64,
    decay_factor: f64,
    age_days: f64,
) -> (f64, ScoreBreakdown) {
    let relevance = normalize_relevance(raw_relevance, &cfg.fuse_strategy);

    let effective_salience = cfg.decay_model.apply(
        salience,
        age_days,
        decay_factor,
        cfg.temporal_half_life_days,
    );
    let temporal = {
        let k = std::f64::consts::LN_2 / cfg.temporal_half_life_days;
        (-k * age_days).exp()
    };

    use uuid::Uuid;
    let candidate = NoteCandidate {
        id: Uuid::nil(),
        rrf_score: Some(relevance),
        salience,
        decay_factor,
        age_days,
        effective_salience,
        rerank_scores: std::collections::HashMap::new(),
    };

    let total = pipeline.score(&candidate);

    let weight_sum = cfg.relevance_weight + cfg.salience_weight + cfg.temporal_weight;
    let norm = if weight_sum > 0.0 { weight_sum } else { 1.0 };
    let amplified_salience = effective_salience.powf(SALIENCE_AMPLIFIER_ALPHA);
    let r_contrib = cfg.relevance_weight * relevance / norm;
    let i_contrib = cfg.salience_weight * amplified_salience / norm;
    let t_contrib = cfg.temporal_weight * temporal / norm;

    let breakdown = ScoreBreakdown {
        relevance,
        salience_raw: salience,
        salience_decayed: effective_salience,
        temporal,
        weighted: WeightedContributions {
            relevance_contribution: r_contrib,
            salience_contribution: i_contrib,
            temporal_contribution: t_contrib,
        },
    };
    (total, breakdown)
}

/// Build a `MemoryRecallPipeline` from a `RecallConfig`.
pub(super) fn make_pipeline(cfg: &RecallConfig) -> MemoryRecallPipeline {
    MemoryRecallPipeline::new(
        cfg.relevance_weight,
        cfg.salience_weight,
        cfg.temporal_weight,
        cfg.temporal_half_life_days,
        SALIENCE_AMPLIFIER_ALPHA,
    )
}

pub(super) struct RecallCandidateSet {
    pub(super) namespace: String,
    pub(super) text_hits: Vec<TextSearchHit>,
    /// One entry per embedding model: (model_name, hits). These have already been
    /// filtered to the caller's visible namespace set via over-fetch + post-filter.
    pub(super) vector_hits_per_model: Vec<(String, Vec<VectorSearchHit>)>,
    /// True when multilingual dense routing was requested AND a multilingual model was found.
    pub(super) multilingual_routed: bool,
    /// The caller's full visible namespace set (primary + any explicit extras).
    pub(super) visible_namespaces: Vec<String>,
}

impl RecallCandidateSet {
    pub(super) fn all_vector_hits(&self) -> Vec<&VectorSearchHit> {
        self.vector_hits_per_model
            .iter()
            .flat_map(|(_, hits)| hits.iter())
            .collect()
    }
}

pub(super) fn recall_candidate_count(cfg: &RecallConfig, limit: u32) -> u32 {
    cfg.candidate_limit
        .unwrap_or_else(|| limit.saturating_mul(cfg.candidate_multiplier).max(40))
}

pub(super) fn search_source_label(source: SearchSource) -> &'static str {
    match source {
        SearchSource::Vector => "vector",
        SearchSource::Text => "text",
        SearchSource::Both => "both",
    }
}

/// Controls whether the FTS5 `snippet(...)` function is called during text search.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum TextSnippetPolicy {
    Omit,
    Include { chars: usize },
}

impl TextSnippetPolicy {
    pub(crate) fn snippet_chars(self) -> usize {
        match self {
            Self::Omit => 0,
            Self::Include { chars } => chars.max(1),
        }
    }
}

pub(super) const RECALL_DIAGNOSTIC_SNIPPET_CHARS: usize = 200;

#[derive(Default)]
pub(super) struct CandidateMeta {
    pub(super) in_text: bool,
    pub(super) in_vector: bool,
    pub(super) title: Option<String>,
    pub(super) snippet: Option<String>,
}

pub(super) struct RecallCandidateParams<'a> {
    pub(super) candidate_limit: u32,
    pub(super) embedding_model: Option<&'a str>,
    /// Route the FTS path through the CJK-bypass tokenizer. Keyed on `contains_cjk`.
    pub(super) cjk_fts_bypass: bool,
    /// Route the dense/vector path to the multilingual model. Keyed on `needs_multilingual`.
    pub(super) use_multilingual: bool,
    pub(super) scoring_cfg: &'a crate::scoring::ScoringConfig,
    pub(super) snippet_policy: TextSnippetPolicy,
    pub(super) fts_gather: &'a crate::config::RecallFtsGatherConfig,
}

pub(super) struct RecallVectorCandidateParams<'a> {
    pub(super) candidate_limit: u32,
    pub(super) embedding_model: Option<&'a str>,
    /// Route the dense/vector path to the multilingual model. Keyed on `needs_multilingual`.
    pub(super) use_multilingual: bool,
    pub(super) scoring_cfg: &'a crate::scoring::ScoringConfig,
    /// Namespace set the caller is allowed to read. ANN returns global candidates;
    /// post-filter trims to this set before returning hits.
    pub(super) visible_namespaces: Vec<String>,
}

pub(super) struct RecallVectorCandidateResult {
    pub(super) vector_hits_per_model: Vec<(String, Vec<VectorSearchHit>)>,
    pub(super) multilingual_routed: bool,
}

pub(super) fn retrieval_hybrid_config(strategy: &FusionStrategy, limit: usize) -> HybridConfig {
    let mut config = HybridConfig::new(limit)
        .with_pool_size(limit)
        .with_fusion_strategy(strategy.clone());

    if let FusionStrategy::Weighted { weights } = strategy {
        config.vector_weight = weights.first().copied().unwrap_or(0.0).max(0.0);
        config.keyword_weight = weights.get(1).copied().unwrap_or(0.0).max(0.0);
    }

    config
}

pub(super) fn source_from_meta(meta: &CandidateMeta) -> SearchSource {
    match (meta.in_vector, meta.in_text) {
        (true, true) => SearchSource::Both,
        (true, false) => SearchSource::Vector,
        (false, true) => SearchSource::Text,
        (false, false) => SearchSource::Text,
    }
}

/// Combine N per-model vector source lists into one via Union (max score per ID).
pub(super) fn combine_vector_sources_union(
    sources: Vec<Vec<(Uuid, DeterministicScore)>>,
) -> Vec<(Uuid, DeterministicScore)> {
    use std::collections::hash_map::Entry;
    let capacity: usize = sources.iter().map(|s| s.len()).sum();
    let mut combined: HashMap<Uuid, DeterministicScore> = HashMap::with_capacity(capacity);
    for source in sources {
        for (id, score) in source {
            match combined.entry(id) {
                Entry::Occupied(mut e) => {
                    if score > *e.get() {
                        *e.get_mut() = score;
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(score);
                }
            }
        }
    }
    let mut result: Vec<(Uuid, DeterministicScore)> = combined.into_iter().collect();
    result.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then(a.cmp(b)));
    result
}

pub(super) fn fuse_candidates(
    candidates: &RecallCandidateSet,
    memory_ids: &HashSet<Uuid>,
    cfg: &RecallConfig,
    limit: usize,
) -> Vec<SearchHit> {
    let mut meta = HashMap::<Uuid, CandidateMeta>::new();

    let text_source: Vec<_> = candidates
        .text_hits
        .iter()
        .filter(|h| memory_ids.contains(&h.subject_id))
        .map(|h| {
            let entry = meta.entry(h.subject_id).or_default();
            entry.in_text = true;
            if entry.title.is_none() {
                entry.title = h.title.clone();
            }
            if entry.snippet.is_none() {
                entry.snippet = h.snippet.clone();
            }
            (h.subject_id, h.score)
        })
        .collect();

    let vector_sources: Vec<Vec<_>> = candidates
        .vector_hits_per_model
        .iter()
        .map(|(_, hits)| {
            hits.iter()
                .filter(|h| memory_ids.contains(&h.subject_id))
                .map(|h| {
                    meta.entry(h.subject_id).or_default().in_vector = true;
                    (h.subject_id, h.score)
                })
                .collect()
        })
        .collect();

    let vector_only = matches!(&cfg.fuse_strategy, FusionStrategy::VectorOnly);
    let keyword_only = matches!(&cfg.fuse_strategy, FusionStrategy::KeywordOnly);
    let is_weighted = matches!(&cfg.fuse_strategy, FusionStrategy::Weighted { .. });

    let sources: Vec<Vec<_>> = if vector_only {
        vector_sources
    } else if keyword_only {
        vec![text_source]
    } else if is_weighted && vector_sources.len() > 1 {
        let combined_vector = combine_vector_sources_union(vector_sources);
        vec![combined_vector, text_source]
    } else {
        let mut s = if vector_sources.is_empty() {
            vec![vec![]]
        } else {
            vector_sources
        };
        s.push(text_source);
        s
    };

    if sources.is_empty() || sources.iter().all(|s| s.is_empty()) {
        return vec![];
    }

    let retrieval_cfg = retrieval_hybrid_config(&cfg.fuse_strategy, limit);
    fuse_search_results(sources, &retrieval_cfg)
        .into_iter()
        .map(|(id, score)| {
            let m = meta.remove(&id).unwrap_or_default();
            let (source, title, snippet) = if vector_only {
                (SearchSource::Vector, None, None)
            } else if keyword_only {
                (SearchSource::Text, m.title, m.snippet)
            } else {
                (source_from_meta(&m), m.title, m.snippet)
            };
            SearchHit {
                entity_id: id,
                score,
                source,
                title,
                snippet,
            }
        })
        .collect()
}

/// Maximum number of OR terms sent to the FTS5 trigram index per recall query.
pub(super) const RECALL_FTS_TERM_FANOUT_LIMIT: usize = 10;

/// Break a recall query into individual search terms for FTS fanout.
#[doc(hidden)]
pub fn recall_text_terms(query: &str) -> Vec<String> {
    recall_text_terms_with_limit(query, RECALL_FTS_TERM_FANOUT_LIMIT)
}

pub(super) fn recall_text_terms_with_limit(query: &str, limit: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms: Vec<String> = query
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ',' | '.' | '?' | '!' | ';' | ':' | '(' | ')')
        })
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect();
    terms.truncate(limit);
    terms
}

impl MemoryPack {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn collect_recall_text_hits(
        &self,
        token: &NamespaceToken,
        query: &str,
        namespaces: &[String],
        candidate_limit: u32,
        snippet_policy: TextSnippetPolicy,
        cjk_fts_bypass: bool,
        fts_gather: &crate::config::RecallFtsGatherConfig,
    ) -> Result<Vec<TextSearchHit>, RuntimeError> {
        let terms = recall_text_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let prof = recall_profile_enabled();
        let call_id = PROF_CID.with(|c| c.get());
        let t_fts = if prof { Some(Instant::now()) } else { None };
        let searcher = self.runtime.text_for_notes(token)?;

        let hits = if fts_gather.enabled {
            crate::text_gather::collect_text_hits(
                searcher.as_ref(),
                query,
                namespaces,
                candidate_limit,
                snippet_policy,
                cjk_fts_bypass,
                fts_gather,
                &terms,
            )
            .await?
        } else {
            let mut h = searcher
                .search(TextSearchRequest {
                    query: terms.join(" "),
                    mode: TextQueryMode::AnyTerm,
                    filter: Some(TextFilter {
                        namespaces: namespaces.to_vec(),
                        kinds: vec![SubstrateKind::Note],
                        ..TextFilter::default()
                    }),
                    top_k: candidate_limit,
                    snippet_chars: snippet_policy.snippet_chars(),
                })
                .await?;
            h.sort_by_key(|h| h.rank);
            h.truncate(candidate_limit as usize);
            h
        };

        if prof {
            if let Some(t) = t_fts {
                plog_n(call_id, "fts", t.elapsed().as_micros(), hits.len());
            }
        }
        Ok(hits)
    }

    pub(super) async fn collect_recall_candidates(
        &self,
        query: &str,
        token: &NamespaceToken,
        opts: RecallCandidateParams<'_>,
    ) -> Result<RecallCandidateSet, RuntimeError> {
        let RecallCandidateParams {
            candidate_limit,
            embedding_model,
            cjk_fts_bypass,
            use_multilingual,
            scoring_cfg,
            snippet_policy,
            fts_gather,
        } = opts;

        // FTS recall uses the single shared fts_notes table (V4 migration). Namespace
        // filtering is applied via TextFilter.namespaces with the full visible set
        // (ADR-007 Rev 4 Phase 1.5 — visible includes {local} ∪ {actor.id} ∪
        // {actor.visible_namespaces} as built by token.visible_namespace_strs()).
        // ANN recall uses the single global index per model (spans all namespaces).
        // Namespace scoping is applied post-search: the vector path over-fetches,
        // then filters candidates to the caller's visible set before returning.
        let visible: Vec<String> = token
            .visible_namespace_strs()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let primary_ns = token.namespace().as_str().to_string();

        let text_fut = self.collect_recall_text_hits(
            token,
            query,
            &visible,
            candidate_limit,
            snippet_policy,
            cjk_fts_bypass,
            fts_gather,
        );
        let vector_fut = self.collect_recall_vector_hits(
            token,
            query,
            &primary_ns,
            RecallVectorCandidateParams {
                candidate_limit,
                embedding_model,
                use_multilingual,
                scoring_cfg,
                visible_namespaces: visible.clone(),
            },
        );
        let (text_hits, vector_result) = tokio::try_join!(text_fut, vector_fut)?;
        Ok(RecallCandidateSet {
            namespace: primary_ns,
            text_hits,
            vector_hits_per_model: vector_result.vector_hits_per_model,
            multilingual_routed: vector_result.multilingual_routed,
            visible_namespaces: visible,
        })
    }

    /// Collect vector (ANN / sqlite-vec) recall candidates.
    ///
    /// ANN path: the global index spans all namespaces. To respect the caller's
    /// visible namespace set we over-fetch and then apply a post-filter at note
    /// hydration time (see `load_memory_candidate_notes`). The over-fetch factor
    /// is F=4 with a fixed margin M=32: k' = max(k * 4, k + 32). This ensures
    /// enough candidates survive the namespace filter to fill `k` results on a
    /// single-namespace store at no extra cost (all candidates pass the filter
    /// on round 1). On a multi-namespace store the margin absorbs foreign hits.
    ///
    /// sqlite-vec fallback: namespace filter is passed directly into the query
    /// (`namespace = ?`) so no over-fetch is required.
    pub(super) async fn collect_recall_vector_hits(
        &self,
        token: &NamespaceToken,
        query: &str,
        ns: &str,
        opts: RecallVectorCandidateParams<'_>,
    ) -> Result<RecallVectorCandidateResult, RuntimeError> {
        let RecallVectorCandidateParams {
            candidate_limit,
            embedding_model,
            use_multilingual,
            scoring_cfg,
            visible_namespaces,
        } = opts;

        // Over-fetch factor for the ANN path: F=4, M=32.
        // k' = max(k * F, k + M) so a 4× wider search compensates for foreign-namespace hits.
        // On a single-namespace store round-1 always satisfies k (zero foreign hits).
        const ANN_OVERFETCH_FACTOR: usize = 4;
        const ANN_OVERFETCH_MARGIN: usize = 32;
        let ann_fetch_limit = (candidate_limit as usize * ANN_OVERFETCH_FACTOR)
            .max(candidate_limit as usize + ANN_OVERFETCH_MARGIN);
        let prof = recall_profile_enabled();
        let call_id = PROF_CID.with(|c| c.get());

        let mut multilingual_routed = false;
        let model_names: Vec<String> = if let Some(m) = embedding_model {
            vec![m.to_string()]
        } else {
            let names = self.runtime.registered_embedding_model_names();
            if names.is_empty() {
                vec![]
            } else if use_multilingual {
                let multilingual_model = scoring_cfg
                    .multilingual_model
                    .as_deref()
                    .and_then(|m| names.iter().find(|n| n.as_str() == m).cloned())
                    .or_else(|| {
                        names
                            .iter()
                            .find(|n| n.contains("multilingual") || n.contains("paraphrase"))
                            .cloned()
                    });
                match multilingual_model {
                    Some(model) => {
                        multilingual_routed = true;
                        vec![model]
                    }
                    None => names,
                }
            } else {
                names
            }
        };

        let vector_hits_per_model: Vec<(String, Vec<VectorSearchHit>)> = if model_names.is_empty() {
            vec![]
        } else {
            let t_embed = if prof { Some(Instant::now()) } else { None };
            let query_vecs: Vec<(String, Vec<f32>)> = match model_names.len() {
                1 => {
                    let m = model_names.into_iter().next().unwrap();
                    vec![
                        embed_query_model(
                            self.runtime.clone(),
                            self.query_cache.clone(),
                            m,
                            query.to_string(),
                        )
                        .await?,
                    ]
                }
                2 => {
                    let mut it = model_names.into_iter();
                    let m0 = it.next().unwrap();
                    let m1 = it.next().unwrap();
                    let f0 = embed_query_model(
                        self.runtime.clone(),
                        self.query_cache.clone(),
                        m0,
                        query.to_string(),
                    );
                    let f1 = embed_query_model(
                        self.runtime.clone(),
                        self.query_cache.clone(),
                        m1,
                        query.to_string(),
                    );
                    let (r0, r1) = tokio::join!(f0, f1);
                    vec![r0?, r1?]
                }
                _ => {
                    let mut handles = Vec::with_capacity(model_names.len());
                    for model_name in model_names {
                        let rt = self.runtime.clone();
                        let cache = self.query_cache.clone();
                        let q = query.to_string();
                        handles.push(tokio::spawn(async move {
                            embed_query_model(rt, cache, model_name, q).await
                        }));
                    }
                    let mut vecs = Vec::with_capacity(handles.len());
                    for h in handles {
                        let pair = h.await.map_err(|e| {
                            RuntimeError::Internal(format!("recall embed task panicked: {e}"))
                        })??;
                        vecs.push(pair);
                    }
                    vecs
                }
            };

            if prof {
                if let Some(t) = t_embed {
                    plog_n(call_id, "embed", t.elapsed().as_micros(), query_vecs.len());
                }
            }

            // Maximum rounds for the ANN over-fetch retry loop. Round 1 is the initial
            // over-fetch; rounds 2–N double the fetch window until the corpus is
            // exhausted or enough visible-namespace candidates are found.
            // Resolved once per process via OnceLock (see ann_overfetch_max_rounds());
            // tests must set ANN_OVERFETCH_MAX_ROUNDS before process start.
            let ann_overfetch_max_rounds = ann_overfetch_max_rounds();

            let t_ann_total = if prof { Some(Instant::now()) } else { None };
            let mut ann_route = "ann";
            let mut results = Vec::with_capacity(query_vecs.len());
            for (model_name, vec) in query_vecs {
                let key = AnnKey::new(ns, &model_name);

                // ANN path: search the global index with ann_fetch_limit (over-fetch)
                // plus a bounded widening retry. The index spans all namespaces;
                // namespace scoping is enforced here by counting how many returned IDs
                // belong to the caller's visible_namespaces set. If fewer than
                // candidate_limit candidates survive, we double the fetch window and
                // retry — up to ANN_OVERFETCH_MAX_ROUNDS total rounds, or until the
                // index is exhausted (returned hits < requested k). Single-namespace
                // stores fill on round 1 at zero extra cost.
                let initial_raw_hits: Option<Vec<(Uuid, f32)>> =
                    match ann::search_loaded(&self.ann, &key, &vec, ann_fetch_limit).await {
                        Ok(Some(hits)) => Some(hits),
                        Ok(None) => {
                            let status = ann::ensure_ann_for_model(
                                &self.runtime,
                                token,
                                &self.ann,
                                &model_name,
                            )
                            .await?;
                            tracing::debug!(
                                ?status,
                                model = %model_name,
                                namespace = %ns,
                                "memory ANN ensured on recall miss"
                            );
                            ann::search_loaded(&self.ann, &key, &vec, ann_fetch_limit).await?
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                namespace = %ns,
                                model = %model_name,
                                "memory ANN search failed; falling back to exact sqlite-vec"
                            );
                            ann::clear_key(&self.ann, &key).await;
                            None
                        }
                    };

                if let Some(first_raw) = initial_raw_hits {
                    // Bounded retry: widen fetch window if visible-namespace survivors
                    // are short. Termination: enough survivors OR corpus exhausted (returned
                    // hits < requested k means the index has no more candidates).
                    let note_store = self.runtime.notes(token)?;
                    let visible_set: std::collections::HashSet<&str> =
                        visible_namespaces.iter().map(String::as_str).collect();

                    // Gate: run the retry loop only when the global ANN index contains
                    // vectors from namespaces outside the caller's visible set.
                    //
                    // We query the namespace set stored on the loaded AnnBridge. If the
                    // set is empty (e.g. freshly snapshot-restored bridge whose set has
                    // not yet been populated) we treat it conservatively as "may contain
                    // non-visible namespaces" and proceed with the loop. If the index
                    // namespace set is a subset of visible_set then all indexed vectors
                    // pass the post-filter on round 1 — no retry needed.
                    let index_has_non_visible =
                        match ann::index_namespace_set(&self.ann, &key).await {
                            Some(index_ns) if !index_ns.is_empty() => {
                                !index_ns.iter().all(|ns| visible_set.contains(ns.as_str()))
                            }
                            // Empty set (snapshot-restored without population yet) or cache miss:
                            // be conservative — assume non-visible namespaces may be present.
                            _ => true,
                        };

                    let mut best_raw = first_raw;
                    let mut current_fetch_limit = ann_fetch_limit;

                    // Run the retry loop only when the index spans namespaces outside
                    // the caller's visible set. On a single-namespace store where the
                    // index only covers the visible namespace, this is skipped entirely
                    // at zero extra cost (no note-batch fetch, no extra ANN searches).
                    if index_has_non_visible {
                        for _round in 1..ann_overfetch_max_rounds {
                            let corpus_exhausted = best_raw.len() < current_fetch_limit;
                            if corpus_exhausted {
                                break;
                            }
                            // Count visible-namespace survivors via a lightweight note batch fetch.
                            let candidate_ids: Vec<Uuid> =
                                best_raw.iter().map(|(id, _)| *id).collect();
                            let notes = note_store.get_notes_batch(&candidate_ids).await?;
                            let visible_count = notes
                                .iter()
                                .filter(|n| {
                                    n.deleted_at.is_none()
                                        && n.kind == "memory"
                                        && visible_set.contains(n.namespace.as_str())
                                })
                                .count();
                            if visible_count >= candidate_limit as usize {
                                break;
                            }
                            // Widen and retry.
                            current_fetch_limit *= 2;
                            tracing::debug!(
                                model = %model_name,
                                namespace = %ns,
                                visible_count,
                                candidate_limit,
                                new_fetch_limit = current_fetch_limit,
                                "memory ANN: widening over-fetch (visible survivors short)"
                            );
                            if let Ok(Some(wider)) =
                                ann::search_loaded(&self.ann, &key, &vec, current_fetch_limit).await
                            {
                                best_raw = wider;
                            } else {
                                break;
                            }
                        }
                    }

                    tracing::debug!(
                        model = %model_name,
                        namespace = %ns,
                        hits = best_raw.len(),
                        "memory recall via warm ANN"
                    );
                    let hits: Vec<VectorSearchHit> = best_raw
                        .into_iter()
                        .enumerate()
                        .map(|(idx, (uuid, score))| VectorSearchHit {
                            subject_id: uuid,
                            score: khive_score::DeterministicScore::from_f64(score as f64),
                            rank: (idx + 1) as u32,
                        })
                        .collect();
                    results.push((model_name, hits));
                    continue;
                }

                // sqlite-vec fallback: the query includes namespace IN (...) so it
                // respects the caller's visible set directly without over-fetch.
                // When visible_namespaces has multiple entries we fan out one search
                // per namespace and union the results, since VectorSearchRequest
                // accepts a single namespace string.
                tracing::debug!(model = %model_name, namespace = %ns, "memory recall via exact sqlite-vec");
                ann_route = "sqlite_vec";
                let store = self.runtime.vectors_for_model(token, &model_name)?;
                let mut all_hits: Vec<VectorSearchHit> = Vec::new();
                for search_ns in &visible_namespaces {
                    let ns_hits = store
                        .search(VectorSearchRequest {
                            query_vectors: vec![vec.clone()],
                            top_k: candidate_limit,
                            namespace: Some(search_ns.clone()),
                            kind: Some(SubstrateKind::Note),
                            embedding_model: Some(model_name.clone()),
                            filter: None,
                            backend_hints: None,
                        })
                        .await?;
                    all_hits.extend(ns_hits);
                }
                // Merge + re-rank by score descending.
                all_hits.sort_by(|a, b| b.score.cmp(&a.score));
                all_hits.truncate(candidate_limit as usize);
                for (idx, hit) in all_hits.iter_mut().enumerate() {
                    hit.rank = (idx + 1) as u32;
                }
                results.push((model_name, all_hits));
            }
            if prof {
                if let Some(t) = t_ann_total {
                    let total_hits: usize = results.iter().map(|(_, h)| h.len()).sum();
                    eprintln!(
                        r#"{{"c":{},"s":"ann","us":{},"n":{},"route":"{}"}}"#,
                        call_id,
                        t.elapsed().as_micros(),
                        total_hits,
                        ann_route,
                    );
                }
            }
            results
        };

        Ok(RecallVectorCandidateResult {
            vector_hits_per_model,
            multilingual_routed,
        })
    }

    pub(super) async fn load_memory_candidate_notes(
        &self,
        token: &NamespaceToken,
        candidates: &RecallCandidateSet,
    ) -> Result<(HashSet<Uuid>, HashMap<Uuid, khive_storage::note::Note>), RuntimeError> {
        let all_vector_hits = candidates.all_vector_hits();
        let candidate_ids: Vec<Uuid> = {
            let mut seen = HashSet::new();
            let mut ids = Vec::new();
            for id in candidates
                .text_hits
                .iter()
                .map(|h| h.subject_id)
                .chain(all_vector_hits.iter().map(|h| h.subject_id))
            {
                if seen.insert(id) {
                    ids.push(id);
                }
            }
            ids
        };

        let note_store = self.runtime.notes(token)?;
        let batch = note_store.get_notes_batch(&candidate_ids).await?;
        let mut memory_ids = HashSet::new();
        let mut notes_by_id = HashMap::new();
        let visible_set: std::collections::HashSet<&str> = candidates
            .visible_namespaces
            .iter()
            .map(String::as_str)
            .collect();
        for note in batch {
            // Post-filter: ANN over-fetch may include rows from outside the caller's
            // visible namespace set. Drop them here where the note row carries its namespace.
            if note.deleted_at.is_none()
                && note.kind == "memory"
                && visible_set.contains(note.namespace.as_str())
            {
                memory_ids.insert(note.id);
                notes_by_id.insert(note.id, note);
            }
        }

        Ok((memory_ids, notes_by_id))
    }
}
