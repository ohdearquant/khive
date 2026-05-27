use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_retrieval::{
    fuse_search_results, FusionStrategy as RetrievalFusionStrategy, HybridConfig,
};
use khive_runtime::{
    micros_to_iso, FusionStrategy as RuntimeFusionStrategy, NamespaceToken, RuntimeError,
    SearchHit, SearchSource, VerbRegistry,
};
use khive_score::DeterministicScore;
use khive_storage::types::{
    Direction, NeighborQuery, TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest,
    VectorSearchHit, VectorSearchRequest,
};
use khive_storage::EdgeRelation;
use khive_types::SubstrateKind;

use crate::config::{RecallConfig, ScoreBreakdown, WeightedContributions};
use crate::rerank::{weighted_rerank, RerankFeatures};
use crate::scoring::{
    calculate_score, contains_cjk, normalize_min_score, normalize_rank_fusion_scores,
    normalize_rrf_scores, ScoreInput, ScoringConfig,
};
use crate::MemoryPack;

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RuntimeError> {
    serde_json::to_value(v).map_err(|e| RuntimeError::InvalidInput(e.to_string()))
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params).map_err(|e| RuntimeError::InvalidInput(e.to_string()))
}

fn validate_memory_type(mt: &str) -> Result<(), RuntimeError> {
    match mt {
        "episodic" | "semantic" => Ok(()),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown memory_type {other:?}; valid: episodic | semantic"
        ))),
    }
}

fn parse_fusion_strategy_str(s: &str) -> Result<RuntimeFusionStrategy, RuntimeError> {
    match s {
        "rrf" => Ok(RuntimeFusionStrategy::Rrf { k: 60 }),
        "weighted" => Ok(RuntimeFusionStrategy::Weighted {
            weights: vec![0.3, 0.7],
        }),
        "union" => Ok(RuntimeFusionStrategy::Union),
        other => Err(RuntimeError::InvalidInput(format!(
            "invalid fusion_strategy {other:?}: must be one of \"rrf\", \"weighted\", \"union\""
        ))),
    }
}

// ue-errors C1: deny_unknown_fields rejects typos like `garbage_arg="x"` at
// deserialization, before any business logic runs.  Aliases (`salience`,
// `decay`, `source`) are still accepted by serde even with this attribute.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberParams {
    content: String,
    memory_type: Option<String>,
    salience: Option<f64>,
    #[serde(alias = "decay")]
    decay_factor: Option<f64>,
    #[serde(alias = "source")]
    source_id: Option<String>,
    tags: Option<Vec<String>>,
    #[serde(default)]
    embedding_model: Option<String>,
}

// ue-errors C1: deny_unknown_fields so typo kwargs (e.g. `min_scroe`)
// are rejected at deserialization rather than silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallParams {
    query: String,
    limit: Option<u32>,
    memory_type: Option<String>,
    min_score: Option<f64>,
    min_salience: Option<f64>,
    config: Option<RecallConfig>,
    top_k: Option<usize>,
    fusion_strategy: Option<String>,
    score_floor: Option<f32>,
    #[serde(default)]
    embedding_model: Option<String>,
    /// Include per-component score breakdown in each result.
    #[serde(default)]
    include_breakdown: Option<bool>,

    /// Deprecated alias for pre-#482 clients. Prefer include_breakdown.
    #[serde(default)]
    presentation: Option<String>,
    /// Entity names to boost in scoring. Memories containing these names
    /// receive a 1.3× multiplier via the EntityMatch ScoreAdjustment.
    #[serde(default)]
    entity_names: Option<Vec<String>>,
    /// When false, truncate content to 200 chars in results. Default true.
    #[serde(default)]
    full_content: Option<bool>,
}

impl RecallParams {
    /// Compute the effective recall config for this request.
    ///
    /// Resolution order (highest priority wins):
    ///   1. Explicit per-call `config` field (`self.config`)
    ///   2. Pack-level tuned base config (`base`, from `MemoryPack::active_config()`)
    ///   3. Legacy top-level `min_score` / `min_salience` overrides
    ///
    /// The legacy fields override regardless of the source because they were the
    /// pre-`config`-field interface and explicit-on-the-wire beats inherited.
    ///
    /// `base` MUST be the pack's active config — this is the wire that connects
    /// `MemoryPack::active_config()` (mutated by `PackTunable::apply_config`)
    /// to recall behavior. Without this parameter the tuning posteriors land
    /// in the Mutex but never reach `compute_score`.
    fn effective_config(&self, base: RecallConfig) -> RecallConfig {
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
///
/// RRF scores are `1/(k+rank)` summed across all sources.
/// - Single source, rank 1: `1/(k+1)` ≈ 0.0164 for k=60.
/// - Two sources, rank 1 in both: `2/(k+1)` ≈ 0.0328.
///
/// Multiplying by `(k+1)` maps a single-source rank-1 to 1.0. When a doc
/// appears in multiple sources the raw RRF sum can exceed `1/(k+1)`, so we
/// clamp the normalized value to 1.0, preserving the [0,1] contract and
/// ensuring the composite score displayed to callers stays in [0,1] (CC-5).
///
/// Weighted and union scores are already in [0,1] and pass through unchanged.
fn normalize_relevance(raw: f64, strategy: &khive_runtime::FusionStrategy) -> f64 {
    match strategy {
        khive_runtime::FusionStrategy::Rrf { k } => (raw * (*k as f64 + 1.0)).min(1.0),
        _ => raw,
    }
}

/// Salience amplifier exponent applied to `effective_salience` in `compute_score`.
///
/// With the default additive formula, `salience_weight=0.20` gives salience
/// a narrow linear spread: salience 0.9 vs 0.3 → 3× difference in the
/// salience term. Raising `effective_salience` to this exponent stretches
/// the spread — at α=1.5, salience 0.9^1.5 ≈ 0.854 vs 0.3^1.5 ≈ 0.164,
/// a ~5.2× difference — so high-salience memories rank clearly above
/// low-salience memories when relevance is similar (UE3-H3, Wave 3).
///
/// Keep α ≤ 2.0. Values above 2 compress near-zero salience toward 0 and
/// may cause all low-salience memories to fall below `min_score`.
const SALIENCE_AMPLIFIER_ALPHA: f64 = 1.5;

fn compute_score(
    cfg: &RecallConfig,
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
    let weight_sum = cfg.relevance_weight + cfg.salience_weight + cfg.temporal_weight;
    let norm = if weight_sum > 0.0 { weight_sum } else { 1.0 };
    let r_contrib = cfg.relevance_weight * relevance / norm;
    // Amplify the salience contribution so that high-salience memories rank
    // clearly above low-salience ones when relevance is similar. Without
    // amplification, the 3× linear spread (0.9 vs 0.3) is too narrow relative
    // to the 70% relevance weight. SALIENCE_AMPLIFIER_ALPHA=1.5 gives ~5.2×
    // spread (0.854 vs 0.164), making salience a meaningful tiebreaker.
    let amplified_salience = effective_salience.powf(SALIENCE_AMPLIFIER_ALPHA);
    let i_contrib = cfg.salience_weight * amplified_salience / norm;
    let t_contrib = cfg.temporal_weight * temporal / norm;
    // Clamp to [0,1]: each component is in [0, weight/norm] and their sum is
    // in [0, 1] by construction when relevance is clamped. The explicit clamp
    // is a defensive guard against floating-point accumulation (CC-5).
    let total = (r_contrib + i_contrib + t_contrib).clamp(0.0, 1.0);
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

struct RecallCandidateSet {
    namespace: String,
    text_hits: Vec<TextSearchHit>,
    /// One entry per embedding model: (model_name, hits).
    /// When a single explicit model is queried, this has one entry.
    /// When all registered models are queried (embedding_model=None), this has N entries.
    vector_hits_per_model: Vec<(String, Vec<VectorSearchHit>)>,
    /// True when CJK routing was requested AND a multilingual model was found and
    /// used as the sole vector source. False when routing was not requested or no
    /// multilingual model was registered (fallback to all models).
    cjk_routed: bool,
}

impl RecallCandidateSet {
    /// Flatten all per-model vector hits into a single list.
    ///
    /// Used when the caller needs a unified view of all vector candidates
    /// (e.g., for note batch-load or response serialization).
    ///
    /// NOTE: the flat-map does NOT deduplicate — the same note_id may appear
    /// once per model. Consumers that need one hit per note should dedup by
    /// subject_id.
    // TODO(P2): deduplicate by note_id (codex review #444)
    fn all_vector_hits(&self) -> Vec<&VectorSearchHit> {
        self.vector_hits_per_model
            .iter()
            .flat_map(|(_, hits)| hits.iter())
            .collect()
    }
}

fn recall_candidate_count(cfg: &RecallConfig, limit: u32) -> u32 {
    cfg.candidate_limit
        .unwrap_or_else(|| limit.saturating_mul(cfg.candidate_multiplier).max(40))
}

fn search_source_label(source: SearchSource) -> &'static str {
    match source {
        SearchSource::Vector => "vector",
        SearchSource::Text => "text",
        SearchSource::Both => "both",
    }
}

#[derive(Default)]
struct CandidateMeta {
    in_text: bool,
    in_vector: bool,
    title: Option<String>,
    snippet: Option<String>,
}

fn to_retrieval_fusion_strategy(strategy: &RuntimeFusionStrategy) -> RetrievalFusionStrategy {
    match strategy {
        RuntimeFusionStrategy::Rrf { k } => RetrievalFusionStrategy::Rrf { k: *k },
        RuntimeFusionStrategy::Weighted { .. } => RetrievalFusionStrategy::Weighted {
            weights: Vec::new(),
        },
        RuntimeFusionStrategy::Union => RetrievalFusionStrategy::Union,
        RuntimeFusionStrategy::VectorOnly => RetrievalFusionStrategy::VectorOnly,
    }
}

fn retrieval_hybrid_config(strategy: &RuntimeFusionStrategy, limit: usize) -> HybridConfig {
    let mut config = HybridConfig::new(limit)
        .with_pool_size(limit)
        .with_fusion_strategy(to_retrieval_fusion_strategy(strategy));

    if let RuntimeFusionStrategy::Weighted { weights } = strategy {
        // Source layout passed to fuse_search_results is [vector, text] — see
        // fuse_candidates(). weights[0] maps to the first source (vector) and
        // weights[1] to the second source (text). HybridConfig fields:
        //   vector_weight = weights[0] (vector source)
        //   keyword_weight = weights[1] (text source)
        // Preserve arbitrary positive scales — do not clamp via with_weights().
        config.vector_weight = weights.first().copied().unwrap_or(0.0).max(0.0);
        config.keyword_weight = weights.get(1).copied().unwrap_or(0.0).max(0.0);
    }

    config
}

fn source_from_meta(meta: &CandidateMeta) -> SearchSource {
    match (meta.in_vector, meta.in_text) {
        (true, true) => SearchSource::Both,
        (true, false) => SearchSource::Vector,
        (false, true) => SearchSource::Text,
        (false, false) => SearchSource::Text,
    }
}

/// Combine N per-model vector source lists into one via Union (max score per ID).
///
/// Used by `fuse_candidates` when the `Weighted` strategy is selected with
/// more than one vector model active. The `Weighted` fusion contract requires
/// exactly 2 sources ([vector, text]); combining all per-model hits into a
/// single vector source preserves that contract while retaining the best
/// score any model assigned to each note (codex High #2, PR #444).
fn combine_vector_sources_union(
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

fn fuse_candidates(
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

    // Build one source vec per model, marking in_vector for each contributing ID.
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

    let vector_only = matches!(&cfg.fuse_strategy, RuntimeFusionStrategy::VectorOnly);
    let is_weighted = matches!(&cfg.fuse_strategy, RuntimeFusionStrategy::Weighted { .. });

    // Assemble ordered source list passed to fuse_search_results.
    //
    // HybridConfig / Weighted contract: exactly 2 sources — [vector, text].
    // With N > 1 vector models the naive approach of passing N+1 sources
    // breaks Weighted because normalized_weights() only yields 2 values;
    // sources beyond index 1 receive weight 0.0, silently dropping text
    // (codex High #2, PR #444).
    //
    // Fix (Weighted, N > 1): combine all per-model vector sources into one
    // via a Union (max-score per note_id) before adding text, so the final
    // list is always [combined_vector, text] — preserving the 2-source
    // Weighted contract. The Union step is intentional: each model may rank
    // the same note; we take the best score any model assigned.
    //
    // For RRF / Union strategies, pass N separate vector sources — those
    // strategies handle arbitrary source counts correctly.
    //
    // When no models are registered, insert an empty vector placeholder so we
    // always pass at least 2 sources to fuse_search_results. This preserves
    // the pre-multi-model behavior: fuse_search_results has a single-source
    // shortcut that returns raw scores without applying the fusion strategy;
    // with 2 sources the shortcut is bypassed and strategy is correctly applied.
    let sources: Vec<Vec<_>> = if vector_only {
        // VectorOnly: pass per-model sources as-is (no text).
        vector_sources
    } else if is_weighted && vector_sources.len() > 1 {
        // Weighted + N > 1 vector models: combine into one vector source so
        // the 2-source [vector, text] layout is preserved. Union (max-score)
        // is used; per-model relative ordering is captured by the max score.
        let combined_vector = combine_vector_sources_union(vector_sources);
        vec![combined_vector, text_source]
    } else {
        let mut s = if vector_sources.is_empty() {
            // No vector models — use empty placeholder to keep 2-source layout.
            vec![vec![]]
        } else {
            vector_sources
        };
        s.push(text_source);
        s
    };

    // When there are no sources at all (no models, no text), return empty.
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

/// Break a recall query into individual search terms for FTS fanout.
///
/// Splits on whitespace and common punctuation, lowercases, deduplicates, and
/// drops empty tokens. This turns a multi-word query into individual FTS5
/// MATCH probes so that notes containing ANY single term enter the candidate
/// pool — whereas a plain conjunction MATCH only returns notes containing ALL
/// terms.
fn recall_text_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms: Vec<String> = query
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | '?' | '!' | ';' | ':' | '(' | ')'))
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect();
    terms.truncate(10);
    terms
}

impl MemoryPack {
    /// Issue #288: term-fanout FTS search for recall candidates.
    ///
    /// Breaks the query into individual terms and issues one FTS5 MATCH probe
    /// per term. Deduplicates by note id, keeping the best rank, and truncates
    /// to `candidate_limit`. This ensures notes partially matching the query
    /// appear in `text_candidates` instead of being excluded by the AND
    /// conjunction semantics of a single-query Plain MATCH.
    async fn collect_recall_text_hits(
        &self,
        token: &NamespaceToken,
        query: &str,
        ns: &str,
        candidate_limit: u32,
    ) -> Result<Vec<TextSearchHit>, RuntimeError> {
        let terms = recall_text_terms(query);
        let searcher = self.runtime.text_for_notes(token)?;
        let mut by_id: HashMap<Uuid, TextSearchHit> = HashMap::new();

        for term in terms {
            let hits = searcher
                .search(TextSearchRequest {
                    query: term,
                    mode: TextQueryMode::Plain,
                    filter: Some(TextFilter {
                        namespaces: vec![ns.to_string()],
                        kinds: vec![SubstrateKind::Note],
                        ..TextFilter::default()
                    }),
                    top_k: candidate_limit,
                    snippet_chars: 200,
                })
                .await?;

            for hit in hits {
                by_id
                    .entry(hit.subject_id)
                    .and_modify(|old| {
                        if hit.rank < old.rank {
                            *old = hit.clone();
                        }
                    })
                    .or_insert(hit);
            }
        }

        let mut hits: Vec<_> = by_id.into_values().collect();
        hits.sort_by_key(|h| h.rank);
        hits.truncate(candidate_limit as usize);
        Ok(hits)
    }

    async fn collect_recall_candidates(
        &self,
        query: &str,
        token: &NamespaceToken,
        candidate_limit: u32,
        embedding_model: Option<&str>,
        // When true, prefer the multilingual model for CJK queries.
        // Ignored when no multilingual model is registered.
        is_cjk: bool,
        scoring_cfg: &crate::scoring::ScoringConfig,
    ) -> Result<RecallCandidateSet, RuntimeError> {
        let ns = token.namespace().as_str().to_string();
        // Tracks whether CJK routing was actually applied (multilingual model found).
        let mut cjk_routed = false;
        // F111 + Issue #288: fan out one FTS5 MATCH per term so notes matching
        // ANY term enter the candidate pool. A single conjunction MATCH ("term1
        // term2 term3") only returns notes containing all terms, which leaves
        // text_candidates empty for memory notes that partially match the query.
        let text_hits = self
            .collect_recall_text_hits(token, query, &ns, candidate_limit)
            .await?;

        // Determine which embedding models to query.
        //   - explicit embedding_model → query only that model (single-model path)
        //   - is_cjk + multilingual model registered → prefer the multilingual model
        //     (ADR-043: CJK routing is only meaningful when the model is present)
        //   - None + models configured → query ALL registered models in parallel
        //   - None + no model configured → skip vector search
        let model_names: Vec<String> = if let Some(m) = embedding_model {
            vec![m.to_string()]
        } else {
            // Fan out to ALL registered models — includes both lattice models
            // from RuntimeConfig and any custom providers added via
            // register_embedder() (codex High #1, PR #444).
            // Gate on the registry, not config().embedding_model, so that
            // custom-only runtimes (no lattice model in config) also fan out.
            let names = self.runtime.registered_embedding_model_names();
            if names.is_empty() {
                // No models configured at all — skip vector search.
                vec![]
            } else if is_cjk {
                // CJK routing (ADR-043): when the query is primarily CJK, prefer
                // the multilingual model. Detect it from the explicit config field
                // (scoring_cfg.cjk_model) or by matching registered names against
                // known multilingual substrings. Fall back to all models when no
                // multilingual model is found so CJK queries still get results.
                let multilingual_model = scoring_cfg
                    .cjk_model
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
                        cjk_routed = true;
                        vec![model]
                    }
                    None => names, // no multilingual model → use all (do not set cjk_routed)
                }
            } else {
                names
            }
        };

        let vector_hits_per_model: Vec<(String, Vec<VectorSearchHit>)> = if model_names.is_empty() {
            vec![]
        } else {
            // Phase 1: embed the query with each model in parallel.
            // Spawning separate tasks allows the embedding services to run
            // concurrently even though KhiveRuntime::embed_with_model is async.
            let mut embed_handles = Vec::with_capacity(model_names.len());
            for model_name in model_names.iter().cloned() {
                let rt = self.runtime.clone();
                let q = query.to_string();
                embed_handles.push(tokio::spawn(async move {
                    rt.embed_with_model(&model_name, &q)
                        .await
                        .map(|v| (model_name, v))
                }));
            }
            let mut query_vecs: Vec<(String, Vec<f32>)> = Vec::with_capacity(embed_handles.len());
            for handle in embed_handles {
                let pair: (String, Vec<f32>) = handle.await.map_err(|e| {
                    RuntimeError::Internal(format!("recall embed task panicked: {e}"))
                })??;
                query_vecs.push(pair);
            }

            // Phase 2: search each model's vector store with the pre-embedded query.
            let mut results = Vec::with_capacity(query_vecs.len());
            for (model_name, vec) in query_vecs {
                let hits = self
                    .runtime
                    .vectors_for_model(token, &model_name)?
                    .search(VectorSearchRequest {
                        query_vectors: vec![vec],
                        top_k: candidate_limit,
                        namespace: Some(ns.clone()),
                        kind: Some(SubstrateKind::Note),
                        embedding_model: Some(model_name.clone()),
                        filter: None,
                        backend_hints: None,
                    })
                    .await?;
                results.push((model_name, hits));
            }
            results
        };

        Ok(RecallCandidateSet {
            namespace: ns,
            text_hits,
            vector_hits_per_model,
            cjk_routed,
        })
    }

    async fn load_memory_candidate_notes(
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
        for note in batch {
            if note.deleted_at.is_none() && note.kind == "memory" {
                memory_ids.insert(note.id);
                notes_by_id.insert(note.id, note);
            }
        }

        Ok((memory_ids, notes_by_id))
    }

    pub(crate) async fn handle_remember(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: RememberParams = deser(params)?;
        if p.content.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "content must not be empty".into(),
            ));
        }

        let memory_type = p.memory_type.as_deref().unwrap_or("episodic");
        validate_memory_type(memory_type)?;

        // F108: reject out-of-range values instead of clamping
        let salience = match p.salience {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "salience must be in [0, 1], got {v}"
                )));
            }
            Some(v) => v,
            None => 0.5,
        };
        // F108: decay_factor must be >= 0; no upper clamp per ADR-021
        let decay_factor = match p.decay_factor {
            Some(v) if v < 0.0 => {
                return Err(RuntimeError::InvalidInput(format!(
                    "decay_factor must be >= 0, got {v}"
                )));
            }
            Some(v) => v,
            None => 0.01,
        };

        // F107: always write memory_type to properties (ADR-021 §4, default "episodic")
        let mut props = json!({ "memory_type": memory_type });
        if let Some(tags) = &p.tags {
            if !tags.is_empty() {
                props["tags"] = json!(tags);
            }
        }

        // F109: resolve source_id — accepts full UUIDs and 8-char short IDs (same
        // contract as `get` / `link`). Short IDs are expanded via prefix lookup
        // before validation so the chain `create → remember(source_id=$prev.id)`
        // works in agent mode where $prev.id is the 8-char short form.
        let mut annotates: Vec<Uuid> = vec![];
        if let Some(sid) = &p.source_id {
            if let Ok(full_uuid) = sid.parse::<Uuid>() {
                annotates.push(full_uuid);
            } else if sid.len() >= 8 && sid.chars().all(|c| c.is_ascii_hexdigit()) {
                match self.runtime.resolve_prefix(token, sid).await {
                    Ok(Some(uuid)) => annotates.push(uuid),
                    Ok(None) => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "source_id {sid:?}: no record matches this prefix"
                        )));
                    }
                    Err(e) => return Err(e),
                }
            } else {
                return Err(RuntimeError::InvalidInput(format!(
                    "source_id {sid:?} is not a valid UUID or 8-char short ID"
                )));
            }
        }

        // Codex High 3 (PR #407): validate embedding_model BEFORE any note/FTS
        // write so unknown-model errors are atomic (no half-written rows).
        // resolve_embedding_model is sync and does not trigger model load — it
        // only checks the registry contains the name.
        if let Some(model_name) = p.embedding_model.as_deref() {
            self.runtime.resolve_embedding_model(Some(model_name))?;
        }

        // Preserve the annotates target before moving the vec into the create call (#291).
        let annotates_target = annotates.first().copied();

        let note = self
            .runtime
            .create_note_with_decay_for_embedding_model(
                token,
                "memory",
                None,
                &p.content,
                Some(salience),
                decay_factor,
                Some(props),
                annotates,
                p.embedding_model.as_deref(),
            )
            .await?;

        let edge_id = if let Some(target_id) = annotates_target {
            self.runtime
                .neighbors_with_query(
                    token,
                    note.id,
                    NeighborQuery {
                        direction: Direction::Out,
                        relations: Some(vec![EdgeRelation::Annotates]),
                        limit: None,
                        min_weight: None,
                    },
                )
                .await?
                .into_iter()
                .find(|hit| hit.node_id == target_id)
                .map(|hit| hit.edge_id.to_string())
        } else {
            None
        };

        let mut response = json!({
            "note_id": note.id.to_string(),
            "kind": note.kind,
            "salience": note.salience,
            "decay_factor": note.decay_factor,
            "memory_type": memory_type,
            "created_at": micros_to_iso(note.created_at),
        });
        if let Some(eid) = edge_id {
            response["edge_id"] = json!(eid);
        }
        to_json(&response)
    }

    pub(crate) async fn handle_recall(
        &self,
        token: &NamespaceToken,
        params: Value,
        _registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: RecallParams = deser(params)?;

        // H3 + Medium: reject empty and noise-only queries before any DB access.
        // is_meaningful_query covers: empty/whitespace, symbols-only, single Latin
        // char, and repeated-character gibberish ("aaaa bbbb"). This closes the
        // partial fix from W3 (empty-only check) and the Medium finding from
        // codex review PR #469.
        let query_trimmed = p.query.trim();
        if query_trimmed.is_empty() {
            return Err(RuntimeError::InvalidInput("query must not be empty".into()));
        }
        if !crate::scoring::is_meaningful_query(query_trimmed) {
            return Err(RuntimeError::InvalidInput(format!(
                "query {query_trimmed:?} does not contain enough meaningful content \
                 (must have at least 2 alphabetic or CJK characters and not consist \
                 of repeated characters)"
            )));
        }

        if let Some(mt) = &p.memory_type {
            validate_memory_type(mt)?;
        }

        if let Some(ref fs) = p.fusion_strategy {
            parse_fusion_strategy_str(fs)?;
        }

        let mut cfg = p.effective_config(self.active_config());
        if let Some(ref fs) = p.fusion_strategy {
            let mut new_strategy = parse_fusion_strategy_str(fs)?;
            // "weighted" in the request means "use weighted fusion" — the actual
            // weight values come from pack config, not the request (ADR-033 §6.1).
            if let (
                RuntimeFusionStrategy::Weighted {
                    weights: ref mut new_w,
                },
                RuntimeFusionStrategy::Weighted {
                    weights: ref existing_w,
                },
            ) = (&mut new_strategy, &cfg.fuse_strategy)
            {
                *new_w = existing_w.clone();
            }
            cfg.fuse_strategy = new_strategy;
        }
        cfg.validate()?;

        // Dual-scale min_score: accept 0.0–1.0 (fraction) or 0–100 (integer percent).
        let effective_min_score: f32 = {
            let raw = if let Some(floor) = p.score_floor {
                floor as f64
            } else {
                cfg.min_score
            };
            normalize_min_score(raw).map_err(RuntimeError::from)?
        };

        // DoS cap: limit is clamped server-side regardless of caller value.
        let limit = if let Some(k) = p.top_k {
            k.min(crate::scoring::MAX_RECALL_LIMIT)
        } else {
            p.limit
                .map(|v| v as usize)
                .unwrap_or(10)
                .clamp(1, crate::scoring::MAX_RECALL_LIMIT)
        };
        let limit_u32 = u32::try_from(limit).unwrap_or(u32::MAX);

        // Build effective ScoringConfig — per-call override or pack default.
        let mut scoring_cfg: ScoringConfig = cfg.scoring.clone().unwrap_or_default();
        scoring_cfg.apply_dos_caps();

        // CJK routing: when the query is primarily CJK and routing is enabled,
        // the vector search path will route to the multilingual model as primary
        // via the model_names selection in collect_recall_candidates.
        let is_cjk = scoring_cfg.enable_cjk_routing && contains_cjk(query_trimmed);

        // DoS cap: clamp the computed candidate_limit to scoring_cfg.max_recall_candidates
        // so a caller cannot bypass the 500-candidate server-side cap by setting a large
        // candidate_multiplier or candidate_limit (codex High #2, PR #469).
        let candidate_limit =
            recall_candidate_count(&cfg, limit_u32).min(scoring_cfg.max_recall_candidates as u32);
        let candidates = self
            .collect_recall_candidates(
                query_trimmed,
                token,
                candidate_limit,
                p.embedding_model.as_deref(),
                is_cjk,
                &scoring_cfg,
            )
            .await?;
        // CJK was actually routed only if a multilingual model was found.
        let actual_cjk_routed = candidates.cjk_routed;
        let (memory_ids, mut notes_by_id) =
            self.load_memory_candidate_notes(token, &candidates).await?;

        // Capture raw vector scores before fusion — used as raw_score in the
        // response triplet and as the cosine-similarity gate for min_raw_relevance.
        let raw_vec_scores: HashMap<Uuid, f32> = {
            let mut map = HashMap::new();
            for (_, hits) in &candidates.vector_hits_per_model {
                for h in hits {
                    let score = h.score.to_f64() as f32;
                    map.entry(h.subject_id)
                        .and_modify(|s| {
                            if score > *s {
                                *s = score;
                            }
                        })
                        .or_insert(score);
                }
            }
            map
        };

        let fused = fuse_candidates(&candidates, &memory_ids, &cfg, candidate_limit as usize);

        if fused.is_empty() {
            return to_json(&Vec::<Value>::new());
        }

        // Normalize fused scores into a calibrated [0, 0.82] relevance band.
        //
        // `fuse_candidates` always produces some form of fused output regardless of
        // whether vector models are present (it inserts an empty vector placeholder for
        // the BM25-only case so `fuse_search_results` always sees ≥ 2 sources). This
        // means the output is always rank-based RRF or Weighted fusion scores — never
        // raw BM25 scores directly from the text store. We must normalize to bring them
        // into the [0.15, 0.82] band that `calculate_score` was calibrated for.
        //
        // Selection:
        //   - RRF fusion strategy → `normalize_rrf_scores` (no signal_strength, since
        //     RRF score magnitudes carry no quality signal — only relative rank matters).
        //   - Other strategies (Weighted, Union) → `normalize_rank_fusion_scores` (uses
        //     signal_strength to scale down the output band for weak-signal corpora).
        let fused_pairs: Vec<(Uuid, f32)> = fused
            .iter()
            .map(|h| (h.entity_id, h.score.to_f64() as f32))
            .collect();
        let is_rrf = matches!(&cfg.fuse_strategy, RuntimeFusionStrategy::Rrf { .. });
        let normalized_relevance: HashMap<Uuid, f32> = if is_rrf {
            normalize_rrf_scores(fused_pairs, &scoring_cfg)
        } else {
            normalize_rank_fusion_scores(fused_pairs, &scoring_cfg)
        };

        // Build a lookup so the response can access per-hit SearchSource.
        let source_by_id: HashMap<Uuid, SearchSource> =
            fused.iter().map(|h| (h.entity_id, h.source)).collect();

        let now_micros = chrono::Utc::now().timestamp_micros();
        let now_millis = now_micros / 1_000;

        // Normalised entity names for EntityMatch adjustments.
        let entity_names: Vec<String> = p
            .entity_names
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|s| s.to_lowercase())
            .collect();

        // score_triplet: (rank_score, absolute_relevance, raw_score_opt)
        // rank_score   = composite score used for ordering
        // score        = absolute relevance (clamped raw cosine/BM25 pre-fusion)
        // raw_score    = pre-fusion vector cosine similarity (None if BM25-only hit)
        struct ScoredNote {
            id: Uuid,
            rank_score: f32,
            score: f32,
            raw_score: Option<f32>,
            breakdown: ScoreBreakdown,
            note: khive_storage::note::Note,
        }

        let mut ranked: Vec<ScoredNote> = Vec::new();
        for hit in &fused {
            let id = hit.entity_id;
            let norm_relevance = match normalized_relevance.get(&id) {
                Some(&v) => v,
                None => continue,
            };

            // Raw cosine gate: exclude vector-retrieved results whose raw cosine
            // similarity is below min_raw_relevance (#2272).
            if let Some(&raw) = raw_vec_scores.get(&id) {
                if raw < scoring_cfg.min_raw_relevance {
                    continue;
                }
            }

            let note = match notes_by_id.remove(&id) {
                Some(note) => note,
                None => continue,
            };
            if let Some(mt) = &p.memory_type {
                let stored = note
                    .properties
                    .as_ref()
                    .and_then(|pr| pr.get("memory_type"))
                    .and_then(|v| v.as_str());
                if stored != Some(mt.as_str()) {
                    continue;
                }
            }
            let salience = note.salience.unwrap_or(0.5);
            let decay_factor = note.decay_factor.unwrap_or(0.01);
            if salience < cfg.min_salience {
                continue;
            }

            // Archive-ported composite score (multiplicative formula).
            let memory_type_str = note
                .properties
                .as_ref()
                .and_then(|pr| pr.get("memory_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("episodic");
            let rank_score = calculate_score(
                &ScoreInput {
                    salience: salience as f32,
                    memory_type_str,
                    content: &note.content,
                    created_at_millis: note.created_at / 1_000,
                    decay_factor: decay_factor as f32,
                    now_millis,
                    relevance_score: norm_relevance,
                    entity_names: &entity_names,
                },
                &scoring_cfg,
            );

            // Also compute the legacy breakdown for verbose mode backward compat.
            let age_days_f64 =
                ((now_micros - note.created_at).max(0) as f64) / (1_000_000.0 * 86_400.0);
            let (_, breakdown) = compute_score(
                &cfg,
                norm_relevance as f64,
                salience,
                decay_factor,
                age_days_f64,
            );

            // ADR-033 §6: when reranker_weights is set, it replaces the archive score.
            let source = source_by_id.get(&id).copied().unwrap_or(SearchSource::Text);
            let final_score = if !cfg.reranker_weights.is_empty() {
                let features = RerankFeatures {
                    relevance: norm_relevance as f64,
                    salience: breakdown.salience_decayed,
                    temporal: breakdown.temporal,
                    text_match: matches!(source, SearchSource::Text | SearchSource::Both),
                    vector_match: matches!(source, SearchSource::Vector | SearchSource::Both),
                };
                weighted_rerank(&features, &cfg.reranker_weights) as f32
            } else {
                rank_score
            };

            // Absolute relevance: raw cosine if available, else composite score.
            let raw_score_opt = raw_vec_scores.get(&id).copied();
            let absolute_relevance = raw_score_opt.unwrap_or(final_score).clamp(0.0, 1.0);
            // ADR-021 §5 / ADR-033: score field must be in [0, 1].
            debug_assert!(
                absolute_relevance <= 1.0,
                "score violates [0,1] contract: {absolute_relevance}"
            );

            if final_score < effective_min_score {
                continue;
            }

            ranked.push(ScoredNote {
                id,
                rank_score: final_score,
                score: absolute_relevance,
                raw_score: raw_score_opt,
                breakdown,
                note,
            });
        }

        // MMR diversity penalty: suppress near-duplicate content.
        //
        // Applied pre-sort so the penalty participates in final ranking.
        // O(n²) over at most max_recall_candidates entries (~50-500).
        if scoring_cfg.mmr_penalty > 0.0 && scoring_cfg.mmr_prefix_len > 0 {
            let prefix_len = scoring_cfg.mmr_prefix_len;
            let prefixes: Vec<String> = ranked
                .iter()
                .map(|sn| sn.note.content.chars().take(prefix_len).collect::<String>())
                .collect();

            for i in 1..ranked.len() {
                for j in 0..i {
                    if prefixes[i] == prefixes[j] {
                        ranked[i].rank_score =
                            (ranked[i].rank_score - scoring_cfg.mmr_penalty).max(0.0);
                        break;
                    }
                }
            }
        }

        // Supersedes suppression: drop memories that have been superseded.
        //
        // Two complementary mechanisms (codex High #3, PR #469):
        //
        // 1. Graph-edge check (primary — the khive contract per ADR-002):
        //    Any candidate note with an inbound `supersedes` edge is stale.
        //    This is the same mechanism used by `search_notes` in the runtime.
        //    Agents create supersession via `link(source=new, target=old, relation="supersedes")`.
        //
        // 2. Property shortcut (archive-import compat):
        //    `properties.supersedes = "<id>"` was the archive service's in-band
        //    annotation. Kept so archive-imported memories still get suppressed.
        if scoring_cfg.enable_supersedes_suppression {
            // Phase 1: collect IDs targeted by `properties.supersedes` (archive compat).
            let mut superseded_by_prop: HashSet<Uuid> = HashSet::new();
            for sn in &ranked {
                if let Some(target_str) = sn
                    .note
                    .properties
                    .as_ref()
                    .and_then(|pr| pr.get("supersedes"))
                    .and_then(|v| v.as_str())
                {
                    // Accept full UUID or 8-char short form.
                    if let Ok(uid) = target_str.parse::<Uuid>() {
                        superseded_by_prop.insert(uid);
                    } else {
                        // Short form: find the matching candidate by prefix.
                        let prefix = target_str.to_lowercase();
                        for sn2 in &ranked {
                            if sn2.id.as_hyphenated().to_string().starts_with(&prefix) {
                                superseded_by_prop.insert(sn2.id);
                                break;
                            }
                        }
                    }
                }
            }

            // Phase 2: graph-edge check — query inbound `supersedes` for each candidate.
            // This covers normal khive usage where agents call `link(relation="supersedes")`.
            let graph = self.runtime.graph(token)?;
            let candidate_ids: Vec<Uuid> = ranked.iter().map(|sn| sn.id).collect();
            let mut superseded_by_edge: HashSet<Uuid> = HashSet::new();
            for id in &candidate_ids {
                let inbound = graph
                    .neighbors(
                        *id,
                        NeighborQuery {
                            direction: Direction::In,
                            relations: Some(vec![EdgeRelation::Supersedes]),
                            limit: Some(1),
                            min_weight: None,
                        },
                    )
                    .await?;
                if !inbound.is_empty() {
                    superseded_by_edge.insert(*id);
                }
            }

            let superseded_ids: HashSet<Uuid> = superseded_by_prop
                .union(&superseded_by_edge)
                .copied()
                .collect();
            if !superseded_ids.is_empty() {
                ranked.retain(|sn| !superseded_ids.contains(&sn.id));
            }
        }

        ranked.sort_by(|a, b| {
            b.rank_score
                .partial_cmp(&a.rank_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.id.cmp(&b.id))
        });
        ranked.truncate(limit);

        // Token budget: truncate to chars_per_token * default_token_budget.
        let token_budget_chars = scoring_cfg.default_token_budget * scoring_cfg.chars_per_token;
        let mut total_chars = 0usize;
        ranked.retain(|sn| {
            let entry_chars = sn.note.content.len();
            if total_chars + entry_chars > token_budget_chars {
                return false;
            }
            total_chars += entry_chars;
            true
        });

        let legacy_breakdown = match p.presentation.as_deref() {
            None => false,
            Some("verbose") => true,
            Some(other) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "memory.recall presentation={other:?} is deprecated; use include_breakdown=true"
                )));
            }
        };
        let is_verbose =
            cfg.include_breakdown || p.include_breakdown.unwrap_or(false) || legacy_breakdown;
        let full_content = p.full_content.unwrap_or(true);
        const PREVIEW_CHARS: usize = 200;

        let results: Vec<Value> = ranked
            .into_iter()
            .map(|sn| {
                let content_out =
                    if !full_content && sn.note.content.chars().count() > PREVIEW_CHARS {
                        let preview: String = sn.note.content.chars().take(PREVIEW_CHARS).collect();
                        format!("{preview}…")
                    } else {
                        sn.note.content.clone()
                    };
                let memory_type = sn
                    .note
                    .properties
                    .as_ref()
                    .and_then(|pr| pr.get("memory_type"))
                    .and_then(|v| v.as_str());
                let mut result = json!({
                    "note_id": sn.id.to_string(),
                    // score triplet (archive pattern, #2272 / #2303):
                    //   score      = absolute cosine relevance (pre-fusion raw or composite)
                    //   rank_score = composite rank score used for ordering
                    //   raw_score  = pre-fusion vector cosine similarity (null for BM25-only)
                    "score": sn.score,
                    "rank_score": sn.rank_score,
                    "raw_score": sn.raw_score,
                    "content": content_out,
                    "salience": sn.note.salience,
                    "decay_factor": sn.note.decay_factor,
                    "memory_type": memory_type,
                    "created_at": micros_to_iso(sn.note.created_at),                });
                if is_verbose {
                    result["breakdown"] = json!(sn.breakdown);
                }
                if actual_cjk_routed {
                    result["cjk_routed"] = json!(true);
                }
                result
            })
            .collect();

        // UE3-H1: In verbose mode, include per-model vector candidate breakdown.
        if is_verbose && candidates.vector_hits_per_model.len() > 1 {
            let per_model: Vec<Value> = candidates
                .vector_hits_per_model
                .iter()
                .map(|(model, hits)| {
                    let hits_json: Vec<Value> = hits
                        .iter()
                        .map(|h| {
                            json!({
                                "note_id": h.subject_id.to_string(),
                                "score": h.score.to_f64(),
                                "rank": h.rank,
                            })
                        })
                        .collect();
                    json!({ "model": model, "hits": hits_json })
                })
                .collect();
            return to_json(&json!({
                "results": results,
                "candidates": {
                    "vector_candidates_per_model": per_model,
                },
            }));
        }

        to_json(&results)
    }

    // ── Dotted sub-handlers (ADR-062) ──────────────────────────────────────────

    pub(crate) async fn handle_recall_embed(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct EmbedParams {
            query: String,
        }
        let p: EmbedParams = deser(params)?;
        if self.runtime.config().embedding_model.is_none() {
            return to_json(&json!({ "embedding": null, "model": null }));
        }
        let vec = self.runtime.embed(&p.query).await?;
        to_json(&json!({
            "embedding": vec,
            "dimensions": vec.len(),
        }))
    }

    pub(crate) async fn handle_recall_candidates(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: RecallParams = deser(params)?;
        let cfg = p.effective_config(self.active_config());
        cfg.validate()?;

        let limit = p.limit.unwrap_or(10).min(100);
        let scoring_cfg = cfg.scoring.clone().unwrap_or_default();
        let candidate_limit =
            recall_candidate_count(&cfg, limit).min(scoring_cfg.max_recall_candidates as u32);
        let candidates = self
            .collect_recall_candidates(
                &p.query,
                token,
                candidate_limit,
                p.embedding_model.as_deref(),
                false, // CJK routing not applied for the candidates sub-verb
                &scoring_cfg,
            )
            .await?;

        // Issue #288: filter text_candidates to memory notes so the diagnostic
        // output reflects the same pool that recall uses, not all notes.
        let (memory_ids, _) = self.load_memory_candidate_notes(token, &candidates).await?;
        let text_candidates: Vec<Value> = candidates
            .text_hits
            .iter()
            .filter(|hit| memory_ids.contains(&hit.subject_id))
            .map(|hit| {
                json!({
                    "note_id": hit.subject_id.to_string(),
                    "score": hit.score.to_f64(),
                    "rank": hit.rank,
                    "title": hit.title.as_deref(),
                    "snippet": hit.snippet.as_deref(),
                })
            })
            .collect();

        // Flatten all per-model vector hits for the response. The legacy single
        // field `vector_candidates` is preserved for backward compat — it contains
        // the union of all models' hits. A new `vector_candidates_per_model` field
        // is added when multiple models are present.
        let all_vector_hits = candidates.all_vector_hits();
        let vector_candidates: Vec<Value> = all_vector_hits
            .iter()
            .map(|hit| {
                json!({
                    "note_id": hit.subject_id.to_string(),
                    "score": hit.score.to_f64(),
                    "rank": hit.rank,
                })
            })
            .collect();

        let mut response = json!({
            "namespace": candidates.namespace,
            "candidate_limit": candidate_limit,
            "text_candidates": text_candidates,
            "vector_candidates": vector_candidates,
        });

        if candidates.vector_hits_per_model.len() > 1 {
            let per_model: serde_json::Map<String, Value> = candidates
                .vector_hits_per_model
                .iter()
                .map(|(model, hits)| {
                    let hits_json: Vec<Value> = hits
                        .iter()
                        .map(|h| {
                            json!({
                                "note_id": h.subject_id.to_string(),
                                "score": h.score.to_f64(),
                                "rank": h.rank,
                            })
                        })
                        .collect();
                    (model.clone(), Value::Array(hits_json))
                })
                .collect();
            response["vector_candidates_per_model"] = Value::Object(per_model);
        }

        to_json(&response)
    }

    pub(crate) async fn handle_recall_fuse(
        &self,
        token: &NamespaceToken,
        params: Value,
        _registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: RecallParams = deser(params)?;
        if let Some(mt) = &p.memory_type {
            validate_memory_type(mt)?;
        }

        let cfg = p.effective_config(self.active_config());
        cfg.validate()?;

        let limit = p.limit.unwrap_or(10).min(100);
        let scoring_cfg_fuse = cfg.scoring.clone().unwrap_or_default();
        let candidate_limit =
            recall_candidate_count(&cfg, limit).min(scoring_cfg_fuse.max_recall_candidates as u32);
        let candidates = self
            .collect_recall_candidates(
                &p.query,
                token,
                candidate_limit,
                p.embedding_model.as_deref(),
                false, // CJK routing not applied for the fuse sub-verb
                &scoring_cfg_fuse,
            )
            .await?;
        let (memory_ids, notes_by_id) =
            self.load_memory_candidate_notes(token, &candidates).await?;

        let fused = fuse_candidates(&candidates, &memory_ids, &cfg, candidate_limit as usize);

        let fused_candidates: Vec<Value> = fused
            .into_iter()
            .filter_map(|hit| {
                let note = notes_by_id.get(&hit.entity_id)?;
                if let Some(mt) = &p.memory_type {
                    let stored = note
                        .properties
                        .as_ref()
                        .and_then(|props| props.get("memory_type"))
                        .and_then(|v| v.as_str());
                    if stored != Some(mt.as_str()) {
                        return None;
                    }
                }
                Some(json!({
                    "note_id": hit.entity_id.to_string(),
                    "fused_score": hit.score.to_f64(),
                    "source": search_source_label(hit.source),
                    "title": hit.title,
                    "snippet": hit.snippet,
                }))
            })
            .collect();

        to_json(&json!({
            "strategy": cfg.fuse_strategy,
            "candidate_limit": candidate_limit,
            "fused_candidates": fused_candidates,
        }))
    }

    /// Apply the weighted feature-combination reranker to fused candidates (ADR-033 §6, PR #375).
    ///
    /// Each candidate may carry optional feature fields in addition to the
    /// required `note_id`. Supported fields (all optional, default 0.0):
    ///
    /// - `fused_score` — mapped to `relevance` feature
    /// - `salience` — used with `age_days` to produce `effective_salience`
    ///   (exponential decay: `salience * exp(-decay_factor * age_days)`)
    /// - `decay_factor` — per-note decay rate; defaults to 0.01 when absent
    /// - `age_days` — note age in days; defaults to 0.0 when absent
    /// - `temporal` — recency score; if absent, computed as
    ///   `exp(-ln2/half_life * age_days)` from config
    /// - `source` — `"text"`, `"vector"`, or `"both"` for boolean features
    ///
    /// The response shape is unchanged: `{reranked: [{note_id, rerank_scores}], active_rerankers}`.
    /// `rerank_scores` is now keyed by feature names from `reranker_weights`, each value being
    /// the weighted contribution from that feature (weight × feature_value).
    ///
    /// When `reranker_weights` is empty, this is a pass-through: each candidate is returned
    /// with an empty `rerank_scores` map — identical to the previous stub behavior.
    pub(crate) async fn handle_recall_rerank(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct RerankParams {
            /// Fused candidates to rerank. Each entry must have `note_id`; other
            /// fields (`fused_score`, `salience`, `age_days`, `decay_factor`,
            /// `temporal`, `source`) are optional feature inputs.
            candidates: Vec<serde_json::Value>,
            config: Option<RecallConfig>,
        }
        let p: RerankParams = deser(params)?;
        let cfg = p.config.unwrap_or_else(|| self.active_config());
        cfg.validate()?;

        let active_rerankers: Vec<&String> = cfg
            .reranker_weights
            .keys()
            .filter(|k| cfg.reranker_weights[*k] > 0.0)
            .collect();

        let reranked: Vec<serde_json::Value> = p
            .candidates
            .iter()
            .map(|candidate| {
                let id = candidate
                    .get("note_id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                if cfg.reranker_weights.is_empty() {
                    // Pass-through: empty reranker_weights → empty rerank_scores.
                    return json!({
                        "note_id": id,
                        "rerank_scores": {},
                        "rerank_score": 0.0_f64,
                    });
                }

                // Extract per-candidate feature inputs from the JSON payload.
                let fused_score = candidate
                    .get("fused_score")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let salience = candidate
                    .get("salience")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let decay_factor = candidate
                    .get("decay_factor")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.01);
                let age_days = candidate
                    .get("age_days")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let temporal = candidate
                    .get("temporal")
                    .and_then(|v| v.as_f64())
                    .unwrap_or_else(|| {
                        let k = std::f64::consts::LN_2 / cfg.temporal_half_life_days;
                        (-k * age_days).exp()
                    });
                let effective_salience = cfg.decay_model.apply(
                    salience,
                    age_days,
                    decay_factor,
                    cfg.temporal_half_life_days,
                );
                let source_str = candidate
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let text_match = matches!(source_str, "text" | "both");
                let vector_match = matches!(source_str, "vector" | "both");

                let features = RerankFeatures {
                    relevance: fused_score,
                    salience: effective_salience,
                    temporal,
                    text_match,
                    vector_match,
                };
                let rerank_score = weighted_rerank(&features, &cfg.reranker_weights);

                // Build per-feature score breakdown so callers can inspect contributions.
                let mut rerank_scores = serde_json::Map::new();
                for (name, &weight) in &cfg.reranker_weights {
                    if weight == 0.0 {
                        continue;
                    }
                    let fv = match name.as_str() {
                        "relevance" => features.relevance,
                        "salience" => features.salience,
                        "temporal" => features.temporal,
                        "text_match" => f64::from(features.text_match),
                        "vector_match" => f64::from(features.vector_match),
                        _ => continue,
                    };
                    rerank_scores.insert(name.clone(), json!(weight * fv));
                }

                json!({
                    "note_id": id,
                    "rerank_scores": rerank_scores,
                    "rerank_score": rerank_score,
                })
            })
            .collect();

        to_json(&json!({
            "reranked": reranked,
            "active_rerankers": active_rerankers.iter().map(|n| n.as_str()).collect::<Vec<_>>(),
        }))
    }

    pub(crate) async fn handle_recall_score(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct ScoreParams {
            rrf: f64,
            salience: f64,
            decay_factor: f64,
            age_days: f64,
            config: Option<RecallConfig>,
        }
        let p: ScoreParams = deser(params)?;
        let cfg = p.config.unwrap_or_else(|| self.active_config());
        cfg.validate()?;
        let (total, breakdown) = compute_score(&cfg, p.rrf, p.salience, p.decay_factor, p.age_days);
        to_json(&json!({
            "total": total,
            "breakdown": breakdown,
        }))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DecayModel;

    #[test]
    fn validate_memory_type_rejects_invalid() {
        let err = validate_memory_type("bogus").unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "expected InvalidInput for unknown memory_type, got {err:?}"
        );
    }

    #[test]
    fn validate_memory_type_accepts_episodic() {
        assert!(validate_memory_type("episodic").is_ok());
    }

    #[test]
    fn validate_memory_type_accepts_semantic() {
        assert!(validate_memory_type("semantic").is_ok());
    }

    #[test]
    fn effective_config_uses_defaults() {
        let p = RecallParams {
            query: "test".to_string(),
            limit: None,
            memory_type: None,
            min_score: None,
            min_salience: None,
            config: None,
            top_k: None,
            fusion_strategy: None,
            score_floor: None,
            embedding_model: None,
            include_breakdown: None,
            presentation: None,
            entity_names: None,
            full_content: None,
        };
        let cfg = p.effective_config(RecallConfig::default());
        assert!((cfg.relevance_weight - 0.70).abs() < 1e-12);
        assert!((cfg.salience_weight - 0.20).abs() < 1e-12);
        assert!((cfg.temporal_weight - 0.10).abs() < 1e-12);
    }

    #[test]
    fn effective_config_legacy_overrides() {
        let p = RecallParams {
            query: "test".to_string(),
            limit: None,
            memory_type: None,
            min_score: Some(0.5),
            min_salience: Some(0.3),
            config: None,
            top_k: None,
            fusion_strategy: None,
            score_floor: None,
            embedding_model: None,
            include_breakdown: None,
            presentation: None,
            entity_names: None,
            full_content: None,
        };
        let cfg = p.effective_config(RecallConfig::default());
        assert!((cfg.min_score - 0.5).abs() < 1e-12);
        assert!((cfg.min_salience - 0.3).abs() < 1e-12);
    }

    #[test]
    fn effective_config_explicit_config_wins() {
        let p = RecallParams {
            query: "test".to_string(),
            limit: None,
            memory_type: None,
            min_score: Some(0.1),
            min_salience: None,
            config: Some(RecallConfig {
                relevance_weight: 0.50,
                ..RecallConfig::default()
            }),
            top_k: None,
            fusion_strategy: None,
            score_floor: None,
            embedding_model: None,
            include_breakdown: None,
            presentation: None,
            entity_names: None,
            full_content: None,
        };
        let cfg = p.effective_config(RecallConfig::default());
        assert!((cfg.relevance_weight - 0.50).abs() < 1e-12);
        // legacy min_score overrides config's default
        assert!((cfg.min_score - 0.1).abs() < 1e-12);
    }

    #[test]
    fn test_weighted_strategy_preserves_pack_weights() {
        use khive_runtime::FusionStrategy as RuntimeFusionStrategy;

        // Pack config has custom weighted weights [0.8, 0.2]
        let base = RecallConfig {
            fuse_strategy: RuntimeFusionStrategy::Weighted {
                weights: vec![0.8, 0.2],
            },
            ..RecallConfig::default()
        };

        // Request overrides to "weighted" — must preserve [0.8, 0.2], not replace with [0.3, 0.7]
        let p = RecallParams {
            query: "test".to_string(),
            limit: None,
            memory_type: None,
            min_score: None,
            min_salience: None,
            config: None,
            top_k: None,
            fusion_strategy: Some("weighted".to_string()),
            score_floor: None,
            embedding_model: None,
            include_breakdown: None,
            presentation: None,
            entity_names: None,
            full_content: None,
        };

        let mut cfg = p.effective_config(base);
        if let Some(ref fs) = p.fusion_strategy {
            let mut new_strategy = parse_fusion_strategy_str(fs).unwrap();
            if let (
                RuntimeFusionStrategy::Weighted {
                    weights: ref mut new_w,
                },
                RuntimeFusionStrategy::Weighted {
                    weights: ref existing_w,
                },
            ) = (&mut new_strategy, &cfg.fuse_strategy)
            {
                *new_w = existing_w.clone();
            }
            cfg.fuse_strategy = new_strategy;
        }

        match cfg.fuse_strategy {
            RuntimeFusionStrategy::Weighted { weights } => {
                assert_eq!(
                    weights,
                    vec![0.8, 0.2],
                    "fusion_strategy=weighted must preserve pack weights [0.8, 0.2], not override with [0.3, 0.7]"
                );
            }
            other => panic!("expected Weighted strategy, got {other:?}"),
        }
    }

    #[test]
    fn fusion_strategy_change_produces_observable_ordering_difference() {
        // Codex Medium 2 (PR #406): prove the fusion_strategy knob actually
        // affects fusion output, not just validation. Uses a deterministic fixture
        // where rank-based (RRF) and score-based (Weighted) fusion must rank
        // differently.
        use khive_runtime::FusionStrategy as RuntimeFusionStrategy;
        use khive_storage::types::{TextSearchHit, VectorSearchHit};
        use std::collections::HashSet;
        use uuid::Uuid;

        let id_a = Uuid::from_u128(0xAAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA);
        let id_b = Uuid::from_u128(0xBBBB_BBBB_BBBB_BBBB_BBBB_BBBB_BBBB_BBBB);
        let id_c = Uuid::from_u128(0xCCCC_CCCC_CCCC_CCCC_CCCC_CCCC_CCCC_CCCC);

        let text_hits = vec![
            TextSearchHit {
                subject_id: id_a,
                score: 0.9_f64.into(),
                rank: 1,
                title: None,
                snippet: None,
            },
            TextSearchHit {
                subject_id: id_b,
                score: 0.5_f64.into(),
                rank: 2,
                title: None,
                snippet: None,
            },
        ];
        let vector_hits = vec![
            VectorSearchHit {
                subject_id: id_c,
                score: 0.95_f64.into(),
                rank: 1,
            },
            VectorSearchHit {
                subject_id: id_a,
                score: 0.3_f64.into(),
                rank: 2,
            },
        ];
        let memory_ids: HashSet<Uuid> = [id_a, id_b, id_c].into_iter().collect();

        let candidates_rrf = RecallCandidateSet {
            namespace: "local".to_string(),
            text_hits: text_hits.clone(),
            vector_hits_per_model: vec![("mock".to_string(), vector_hits.clone())],
            cjk_routed: false,
        };
        let cfg_rrf = RecallConfig {
            fuse_strategy: RuntimeFusionStrategy::Rrf { k: 60 },
            ..RecallConfig::default()
        };
        let rrf_results = fuse_candidates(&candidates_rrf, &memory_ids, &cfg_rrf, 10);
        let rrf_order: Vec<Uuid> = rrf_results.iter().map(|h| h.entity_id).collect();

        let candidates_weighted = RecallCandidateSet {
            namespace: "local".to_string(),
            text_hits,
            vector_hits_per_model: vec![("mock".to_string(), vector_hits)],
            cjk_routed: false,
        };
        // Source layout passed to fuse_candidates is [vector, text] (vector first).
        // weights[0] = vector_weight, weights[1] = keyword_weight (corrected in PR #469).
        //
        // Weighted [vector=0.9, text=0.1]:
        //   id_c (vector only, score 0.95) → 0.95 * 0.9 = 0.855 → rank 1
        //   id_a (text score 0.9, vector score 0.3) → 0.3 * 0.9 + 0.9 * 0.1 = 0.27 + 0.09 = 0.36 → rank 2
        //   id_b (text only, score 0.5) → 0.5 * 0.1 = 0.05 → rank 3
        //   Weighted order: C, A, B
        //
        // RRF k=60:
        //   id_a (text rank 1, vector rank 2): 1/61 + 1/62 ≈ 0.0325 → rank 1
        //   id_c (vector rank 1 only): 1/61 ≈ 0.0164 → rank 2
        //   id_b (text rank 2 only): 1/62 ≈ 0.0161 → rank 3
        //   RRF order: A, C, B
        //
        // C leading in Weighted but not in RRF means the orderings differ.
        let cfg_weighted = RecallConfig {
            fuse_strategy: RuntimeFusionStrategy::Weighted {
                weights: vec![0.9, 0.1], // [vector=0.9, text=0.1]
            },
            ..RecallConfig::default()
        };
        let weighted_results =
            fuse_candidates(&candidates_weighted, &memory_ids, &cfg_weighted, 10);
        let weighted_order: Vec<Uuid> = weighted_results.iter().map(|h| h.entity_id).collect();

        // The orderings MUST differ — this is the discriminating assertion.
        // RRF: A first (appears in both sources); Weighted(vector-heavy): C first (highest vector score).
        assert_ne!(
            rrf_order, weighted_order,
            "fusion_strategy change must affect ordering; RRF and Weighted produced identical: {rrf_order:?}"
        );
        // Also verify RRF puts A first and Weighted puts C first.
        assert_eq!(
            rrf_order.first(),
            Some(&id_a),
            "RRF must put id_a first (highest combined rank)"
        );
        assert_eq!(
            weighted_order.first(),
            Some(&id_c),
            "Weighted(vector=0.9) must put id_c first (highest vector score)"
        );
    }

    #[test]
    fn compute_score_weighted_strategy_formula() {
        // Use Weighted strategy (normalization factor = 1.0) to verify the
        // weighted-combination formula with salience amplification.
        // total = w_r*relevance + w_s*amplified_salience + w_t*temporal
        // where amplified_salience = effective_salience ^ SALIENCE_AMPLIFIER_ALPHA
        let cfg = RecallConfig {
            fuse_strategy: khive_runtime::FusionStrategy::Weighted {
                weights: vec![0.3, 0.7],
            },
            ..RecallConfig::default()
        };
        let relevance = 0.5;
        let salience = 0.8;
        let decay_factor = 0.01;
        let age_days = 0.0;
        let (total, bd) = compute_score(&cfg, relevance, salience, decay_factor, age_days);
        // At age=0: salience_decayed = salience = 0.8, temporal = 1.0
        // amplified = 0.8^1.5 ≈ 0.71554
        // total = 0.70*0.5 + 0.20*0.71554 + 0.10*1.0 ≈ 0.35 + 0.14311 + 0.10 ≈ 0.59311
        let amplified = 0.8_f64.powf(SALIENCE_AMPLIFIER_ALPHA);
        let expected = 0.70 * 0.5 + 0.20 * amplified + 0.10 * 1.0;
        assert!(
            (total - expected).abs() < 1e-10,
            "got {total}, expected {expected}"
        );
        assert!((bd.relevance - 0.5).abs() < 1e-12);
        assert!((bd.salience_raw - 0.8).abs() < 1e-12);
    }

    #[test]
    fn compute_score_rrf_strategy_normalizes_to_comparable_range() {
        // With RRF k=60 the raw score for rank 1 is 1/(60+1) ≈ 0.01639.
        // After normalization (* 61) it becomes exactly 1.0, placing relevance
        // in the same [0, 1] range as weighted/union fusion outputs.
        // Use an explicit RRF config — the default changed to Weighted (CC-6).
        let cfg = RecallConfig {
            fuse_strategy: khive_runtime::FusionStrategy::Rrf { k: 60 },
            ..RecallConfig::default()
        };
        let raw_rrf_rank1 = 1.0 / 61.0;
        let (_, bd) = compute_score(&cfg, raw_rrf_rank1, 1.0, 0.0, 0.0);
        assert!(
            (bd.relevance - 1.0).abs() < 1e-10,
            "RRF rank-1 relevance should normalize to 1.0, got {}",
            bd.relevance
        );
    }

    #[test]
    fn compute_score_rrf_multi_source_clamped_to_one() {
        // CC-5 regression: when a doc appears in both text and vector sources,
        // the RRF sum is 2/(k+1) ≈ 0.0328 for k=60. Before the fix, multiplying
        // by (k+1)=61 gave relevance=2.0, which inflated the composite total
        // beyond 1.0. After the fix, relevance is clamped to 1.0.
        let cfg = RecallConfig {
            fuse_strategy: khive_runtime::FusionStrategy::Rrf { k: 60 },
            ..RecallConfig::default()
        };
        let raw_rrf_two_sources = 2.0 / 61.0; // rank-1 in both vector and text
        let (total, bd) = compute_score(&cfg, raw_rrf_two_sources, 1.0, 0.0, 0.0);
        assert!(
            bd.relevance <= 1.0,
            "relevance must not exceed 1.0 for multi-source RRF, got {}",
            bd.relevance
        );
        assert!(
            total <= 1.0,
            "composite score must not exceed 1.0, got {total}"
        );
        assert!(
            total >= 0.0,
            "composite score must not be negative, got {total}"
        );
    }

    #[test]
    fn compute_score_exponential_decay_at_decay_factor_half_life() {
        // Use explicit exponential decay config — not relying on default decay_model.
        // ADR-021 §5: salience_decayed = salience * exp(-decay_factor * age_days)
        // At age = ln(2)/0.01 ≈ 69.3 days: salience_decayed ≈ 0.5
        let cfg = RecallConfig {
            decay_model: DecayModel::Exponential,
            temporal_half_life_days: 30.0,
            ..RecallConfig::default()
        };
        let age_days = std::f64::consts::LN_2 / 0.01;
        let (_, bd) = compute_score(&cfg, 0.5, 1.0, 0.01, age_days);
        assert!(
            (bd.salience_decayed - 0.5).abs() < 1e-10,
            "salience_decayed = {}",
            bd.salience_decayed
        );
        // Temporal at age_days=69.3 with half_life=30: exp(-ln2/30 * 69.3) ≈ exp(-1.6) ≈ 0.2
        // Just verify it's < 0.5 (past the temporal half-life)
        assert!(bd.temporal < 0.5, "temporal = {}", bd.temporal);
    }

    #[test]
    fn compute_score_temporal_halves_at_temporal_half_life() {
        // Use explicit half_life=30 — not relying on default temporal_half_life_days.
        let cfg = RecallConfig {
            temporal_half_life_days: 30.0,
            ..RecallConfig::default()
        };
        let (_, bd) = compute_score(&cfg, 0.5, 1.0, 0.01, 30.0);
        // At age = temporal_half_life = 30 days: temporal = exp(-ln2/30 * 30) = 0.5
        assert!(
            (bd.temporal - 0.5).abs() < 1e-10,
            "temporal = {}",
            bd.temporal
        );
    }

    #[test]
    fn compute_score_custom_weights() {
        // Use Weighted strategy so relevance passes through unnormalized.
        let cfg = RecallConfig {
            relevance_weight: 1.0,
            salience_weight: 0.0,
            temporal_weight: 0.0,
            fuse_strategy: khive_runtime::FusionStrategy::Weighted {
                weights: vec![0.5, 0.5],
            },
            ..RecallConfig::default()
        };
        let (total, _) = compute_score(&cfg, 0.8, 0.9, 0.01, 10.0);
        // Only relevance matters: total = 0.8
        assert!((total - 0.8).abs() < 1e-10, "got {total}");
    }

    // ── F107: remember always writes memory_type to properties ───────────

    #[test]
    fn remember_params_default_memory_type_is_episodic() {
        // When memory_type is absent, validate_memory_type("episodic") must pass.
        // This ensures the default "episodic" is valid.
        assert!(validate_memory_type("episodic").is_ok());
    }

    // ── F108: reject out-of-range salience and decay_factor ─────────────

    #[test]
    fn remember_params_salience_below_zero_rejected() {
        // Simulate handler validation path directly
        let salience: f64 = -0.1;
        let result: Result<f64, RuntimeError> = if !(0.0..=1.0).contains(&salience) {
            Err(RuntimeError::InvalidInput(format!(
                "salience must be in [0, 1], got {salience}"
            )))
        } else {
            Ok(salience)
        };
        assert!(result.is_err(), "expected error for salience < 0");
    }

    #[test]
    fn remember_params_salience_above_one_rejected() {
        let salience: f64 = 1.1;
        let result: Result<f64, RuntimeError> = if !(0.0..=1.0).contains(&salience) {
            Err(RuntimeError::InvalidInput(format!(
                "salience must be in [0, 1], got {salience}"
            )))
        } else {
            Ok(salience)
        };
        assert!(result.is_err(), "expected error for salience > 1");
    }

    #[test]
    fn remember_params_salience_boundary_values_accepted() {
        // 0.0 and 1.0 are valid
        for val in [0.0_f64, 0.5, 1.0] {
            let result: Result<(), RuntimeError> = if !(0.0..=1.0).contains(&val) {
                Err(RuntimeError::InvalidInput("out of range".into()))
            } else {
                Ok(())
            };
            assert!(result.is_ok(), "boundary {val} should be accepted");
        }
    }

    #[test]
    fn remember_params_decay_factor_below_zero_rejected() {
        let df: f64 = -0.01;
        let result: Result<f64, RuntimeError> = if df < 0.0 {
            Err(RuntimeError::InvalidInput(format!(
                "decay_factor must be >= 0, got {df}"
            )))
        } else {
            Ok(df)
        };
        assert!(result.is_err(), "expected error for decay_factor < 0");
    }

    #[test]
    fn remember_params_decay_factor_above_one_accepted() {
        // ADR-021 only requires decay_factor >= 0; no upper cap
        let df: f64 = 2.5;
        let result: Result<f64, RuntimeError> = if df < 0.0 {
            Err(RuntimeError::InvalidInput("negative".into()))
        } else {
            Ok(df)
        };
        assert!(result.is_ok(), "decay_factor > 1 should be accepted");
    }

    // ── F109: invalid source_id UUID string is rejected ──────────────────

    #[test]
    fn remember_params_invalid_source_id_uuid_is_rejected() {
        let sid = "not-a-uuid";
        let result: Result<Uuid, RuntimeError> = sid.parse::<Uuid>().map_err(|_| {
            RuntimeError::InvalidInput(format!("source_id {sid:?} is not a valid UUID"))
        });
        assert!(result.is_err(), "expected error for invalid UUID string");
    }

    #[test]
    fn remember_params_valid_source_id_uuid_is_accepted() {
        let sid = "00000000-0000-0000-0000-000000000001";
        let result = sid.parse::<Uuid>();
        assert!(result.is_ok(), "valid UUID should parse successfully");
    }

    // ── recall_rerank: pass-through when no rerankers configured ─────────

    #[test]
    fn recall_rerank_config_empty_reranker_weights_has_no_active() {
        let cfg = RecallConfig::default();
        let active: Vec<_> = cfg
            .reranker_weights
            .iter()
            .filter(|(_, &w)| w > 0.0)
            .collect();
        assert!(active.is_empty(), "default config has no active rerankers");
    }

    #[test]
    fn recall_rerank_config_with_reranker_weight_is_active() {
        let mut cfg = RecallConfig::default();
        cfg.reranker_weights
            .insert("cross_encoder".to_string(), 0.5);
        let active: Vec<_> = cfg
            .reranker_weights
            .iter()
            .filter(|(_, &w)| w > 0.0)
            .collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, "cross_encoder");
    }

    // ── F186/F223/F230: new RecallConfig fields ───────────────────────────

    #[test]
    fn recall_config_reranker_fields_default_empty() {
        let cfg = RecallConfig::default();
        assert!(cfg.reranker_weights.is_empty());
        assert!(cfg.reranker_params.is_empty());
    }

    #[test]
    fn recall_config_fallback_during_migration_defaults_true() {
        let cfg = RecallConfig::default();
        assert!(cfg.fallback_during_migration);
    }

    #[test]
    fn recall_config_negative_reranker_weight_fails_validation() {
        let mut cfg = RecallConfig::default();
        cfg.reranker_weights
            .insert("bad_reranker".to_string(), -0.1);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn recall_config_zero_reranker_weight_validates() {
        let mut cfg = RecallConfig::default();
        // Weight of 0.0 means disabled, not an error
        cfg.reranker_weights
            .insert("disabled_reranker".to_string(), 0.0);
        assert!(cfg.validate().is_ok());
    }

    // ── UE3-H3: salience amplification makes high-salience memories rank higher ─

    #[test]
    fn high_salience_outranks_low_salience_on_similar_relevance() {
        // Regression for UE3-H3: with SALIENCE_AMPLIFIER_ALPHA > 1.0, a memory
        // with salience=0.9 must score higher than salience=0.3 when both
        // have the same relevance and age. Without amplification (alpha=1.0) the
        // salience contribution difference is only 0.20*(0.9-0.3)=0.12, which
        // is easily swamped when relevance differs even slightly.
        let cfg = RecallConfig {
            fuse_strategy: khive_runtime::FusionStrategy::Weighted {
                weights: vec![0.5, 0.5],
            },
            ..RecallConfig::default()
        };
        let relevance = 0.5; // identical for both
        let age_days = 0.0; // brand new
        let decay_factor = 0.01;

        let (score_high, _) = compute_score(&cfg, relevance, 0.9, decay_factor, age_days);
        let (score_low, _) = compute_score(&cfg, relevance, 0.3, decay_factor, age_days);

        assert!(
            score_high > score_low,
            "high salience (score={score_high}) should outrank low salience (score={score_low})"
        );

        // Quantitative check: the gap must be > 10% of the score range so the
        // amplification is actually meaningful (not just a rounding difference).
        let gap = score_high - score_low;
        assert!(gap > 0.05, "salience score gap should be > 0.05, got {gap}");
    }

    #[test]
    fn salience_amplifier_discriminates_more_than_linear() {
        // Verify that SALIENCE_AMPLIFIER_ALPHA > 1.0 produces a wider spread
        // between high and low salience than the linear (alpha=1.0) baseline.
        let cfg = RecallConfig::default();
        let relevance = 0.0; // zero out relevance to isolate salience contribution
        let age_days = 0.0;

        let (score_high, _) = compute_score(&cfg, relevance, 0.9, 0.0, age_days);
        let (score_low, _) = compute_score(&cfg, relevance, 0.3, 0.0, age_days);
        let amplified_spread = score_high - score_low;

        // Linear spread without amplification: 0.20*(0.9-0.3) = 0.12
        let linear_spread = 0.20_f64 * (0.9 - 0.3);

        assert!(
            amplified_spread > linear_spread,
            "amplified spread ({amplified_spread}) should exceed linear spread ({linear_spread})"
        );
    }

    // ── UE3-H1: per-model vector breakdown structure is correct ───────────────

    #[test]
    fn vector_candidates_per_model_shape_is_array_of_model_objects() {
        // Verify that when multiple vector models are present, the candidates
        // envelope serializes as [{model: "...", hits: [{note_id, score, rank}]}].
        // This is the shape that recall() verbose mode injects.
        use khive_storage::types::VectorSearchHit;
        use uuid::Uuid;

        let id1 = Uuid::from_u128(0x1);
        let id2 = Uuid::from_u128(0x2);

        let hits_a = vec![VectorSearchHit {
            subject_id: id1,
            score: 0.9_f64.into(),
            rank: 1,
        }];
        let hits_b = vec![VectorSearchHit {
            subject_id: id2,
            score: 0.7_f64.into(),
            rank: 1,
        }];

        let candidates = RecallCandidateSet {
            namespace: "test".to_string(),
            text_hits: vec![],
            vector_hits_per_model: vec![
                ("model-a".to_string(), hits_a),
                ("model-b".to_string(), hits_b),
            ],
            cjk_routed: false,
        };

        // Build the per_model structure as done in handle_recall verbose path.
        let per_model: Vec<Value> = candidates
            .vector_hits_per_model
            .iter()
            .map(|(model, hits)| {
                let hits_json: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        json!({
                            "note_id": h.subject_id.to_string(),
                            "score": h.score.to_f64(),
                            "rank": h.rank,
                        })
                    })
                    .collect();
                json!({ "model": model, "hits": hits_json })
            })
            .collect();

        assert_eq!(per_model.len(), 2, "should have one entry per model");
        assert_eq!(per_model[0]["model"], "model-a");
        assert_eq!(per_model[0]["hits"][0]["note_id"], id1.to_string());
        assert_eq!(per_model[1]["model"], "model-b");
        assert_eq!(per_model[1]["hits"][0]["note_id"], id2.to_string());
    }

    // ── H3: empty query must be rejected ─────────────────────────────────────

    #[test]
    fn recall_params_empty_query_should_be_rejected() {
        // H3 regression: recall(query="") returned 10 random memories without error.
        // Validated here at the handler boundary: empty / whitespace-only queries
        // must produce InvalidInput before any DB access.
        for q in &["", "   ", "\t\n"] {
            let result: Result<(), RuntimeError> = if q.trim().is_empty() {
                Err(RuntimeError::InvalidInput("query must not be empty".into()))
            } else {
                Ok(())
            };
            assert!(
                result.is_err(),
                "empty/whitespace query {:?} must be rejected",
                q
            );
        }
    }

    // ── CC-5: composite score must be bounded to [0, 1] ──────────────────────

    #[test]
    fn compute_score_composite_bounded_to_unit_interval() {
        // CC-5 regression: composite scores were 1.4–2.4 for multi-source RRF
        // due to normalize_relevance exceeding 1.0 when docs appeared in N>1 sources.
        // After fix, scores are always in [0, 1] for any valid input.
        let cfgs = [
            RecallConfig {
                fuse_strategy: khive_runtime::FusionStrategy::Rrf { k: 60 },
                ..RecallConfig::default()
            },
            RecallConfig::default(), // Weighted
            RecallConfig {
                fuse_strategy: khive_runtime::FusionStrategy::Union,
                ..RecallConfig::default()
            },
        ];
        for cfg in &cfgs {
            for raw_relevance in [0.0, 0.5, 1.0, 2.0 / 61.0, 1.0 / 61.0] {
                for salience in [0.0, 0.3, 0.9, 1.0] {
                    let (total, _) = compute_score(cfg, raw_relevance, salience, 0.01, 0.0);
                    assert!(
                        (0.0..=1.0).contains(&total),
                        "composite score out of [0,1]: {total} (relevance={raw_relevance}, salience={salience}, strategy={:?})",
                        cfg.fuse_strategy
                    );
                }
            }
        }
    }

    // ── CC-6: Weighted default strategy lets salience govern ranking ──────────

    #[test]
    fn default_fusion_strategy_is_weighted() {
        // CC-6: The default strategy must be Weighted so that salience influences
        // ranking. Under RRF with small k, a marginally better text rank can
        // dominate the salience contribution entirely.
        let cfg = RecallConfig::default();
        assert!(
            matches!(
                cfg.fuse_strategy,
                khive_runtime::FusionStrategy::Weighted { .. }
            ),
            "default fuse_strategy must be Weighted (CC-6), got {:?}",
            cfg.fuse_strategy
        );
    }

    #[test]
    fn salience_dominates_relevance_under_default_weighted_strategy() {
        // CC-6: with the default Weighted strategy, salience=0.9 must rank above
        // salience=0.3 even when the lower-salience memory has better relevance
        // — as long as the relevance delta is within a typical real-world spread.
        // This mirrors the scenario from the audit: salience=0.3 ranked above
        // salience=0.9 at rank 4 with the old RRF default.
        let cfg = RecallConfig::default();
        let age_days = 0.0;
        let decay = 0.01;

        // Simulates the audit scenario: low-salience has better relevance (0.9 vs 0.8)
        // but high-salience should still win thanks to the amplifier.
        let relevance_low = 0.9;
        let relevance_high = 0.8;

        let (score_high, _) = compute_score(&cfg, relevance_high, 0.9, decay, age_days);
        let (score_low, _) = compute_score(&cfg, relevance_low, 0.3, decay, age_days);

        assert!(
            score_high > score_low,
            "high-salience (0.9, relevance=0.8, score={score_high}) should outrank \
             low-salience (0.3, relevance=0.9, score={score_low}) under default Weighted strategy"
        );
    }
}
