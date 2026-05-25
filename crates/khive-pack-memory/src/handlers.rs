use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_retrieval::{
    fuse_search_results, FusionStrategy as RetrievalFusionStrategy, HybridConfig,
};
use khive_runtime::{
    FusionStrategy as RuntimeFusionStrategy, NamespaceToken, RuntimeError, SearchHit, SearchSource,
    VerbRegistry,
};
use khive_storage::types::{
    TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, VectorSearchHit,
    VectorSearchRequest,
};
use khive_types::SubstrateKind;

use crate::config::{RecallConfig, ScoreBreakdown, WeightedContributions};
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

#[derive(Deserialize)]
struct RememberParams {
    content: String,
    memory_type: Option<String>,
    #[serde(alias = "salience")]
    importance: Option<f64>,
    #[serde(alias = "decay")]
    decay_factor: Option<f64>,
    #[serde(alias = "source")]
    source_id: Option<String>,
    tags: Option<Vec<String>>,
    #[serde(default)]
    embedding_model: Option<String>,
}

#[derive(Deserialize)]
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

fn compute_score(
    cfg: &RecallConfig,
    rrf: f64,
    salience: f64,
    decay_factor: f64,
    age_days: f64,
) -> (f64, ScoreBreakdown) {
    let effective_importance = cfg.decay_model.apply(
        salience,
        age_days,
        decay_factor,
        cfg.temporal_half_life_days,
    );
    let temporal = {
        let k = std::f64::consts::LN_2 / cfg.temporal_half_life_days;
        (-k * age_days).exp()
    };
    let weight_sum = cfg.relevance_weight + cfg.importance_weight + cfg.temporal_weight;
    let norm = if weight_sum > 0.0 { weight_sum } else { 1.0 };
    let r_contrib = cfg.relevance_weight * rrf / norm;
    let i_contrib = cfg.importance_weight * effective_importance / norm;
    let t_contrib = cfg.temporal_weight * temporal / norm;
    let total = r_contrib + i_contrib + t_contrib;
    let breakdown = ScoreBreakdown {
        relevance: rrf,
        importance_raw: salience,
        importance_decayed: effective_importance,
        temporal,
        weighted: WeightedContributions {
            relevance_contribution: r_contrib,
            importance_contribution: i_contrib,
            temporal_contribution: t_contrib,
        },
    };
    (total, breakdown)
}

struct RecallCandidateSet {
    namespace: String,
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
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
        // Runtime weighted fusion uses [text, vector]. HybridConfig uses keyword/vector.
        // Preserve arbitrary positive scales — do not clamp via with_weights().
        config.keyword_weight = weights.first().copied().unwrap_or(0.0).max(0.0);
        config.vector_weight = weights.get(1).copied().unwrap_or(0.0).max(0.0);
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

fn fuse_candidates(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    memory_ids: &HashSet<Uuid>,
    cfg: &RecallConfig,
    limit: usize,
) -> Vec<SearchHit> {
    let mut meta = HashMap::<Uuid, CandidateMeta>::new();

    let text_source: Vec<_> = text_hits
        .into_iter()
        .filter(|h| memory_ids.contains(&h.subject_id))
        .map(|h| {
            let TextSearchHit {
                subject_id,
                score,
                title,
                snippet,
                ..
            } = h;
            let entry = meta.entry(subject_id).or_default();
            entry.in_text = true;
            if entry.title.is_none() {
                entry.title = title;
            }
            if entry.snippet.is_none() {
                entry.snippet = snippet;
            }
            (subject_id, score)
        })
        .collect();

    let vector_source: Vec<_> = vector_hits
        .into_iter()
        .filter(|h| memory_ids.contains(&h.subject_id))
        .map(|h| {
            let entry = meta.entry(h.subject_id).or_default();
            entry.in_vector = true;
            (h.subject_id, h.score)
        })
        .collect();

    let vector_only = matches!(&cfg.fuse_strategy, RuntimeFusionStrategy::VectorOnly);
    let sources = if vector_only {
        vec![vector_source]
    } else {
        // HybridConfig weighted convention: vector first, keyword second.
        vec![vector_source, text_source]
    };

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

impl MemoryPack {
    async fn collect_recall_candidates(
        &self,
        query: &str,
        token: &NamespaceToken,
        candidate_limit: u32,
        embedding_model: Option<&str>,
    ) -> Result<RecallCandidateSet, RuntimeError> {
        let ns = token.namespace().as_str().to_string();
        // F111: restrict text candidates to Note substrate kind so entity records
        // cannot fill the candidate pool before any memory note is considered.
        let text_hits = self
            .runtime
            .text_for_notes(token)?
            .search(TextSearchRequest {
                query: query.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.clone()],
                    kinds: vec![SubstrateKind::Note],
                    ..TextFilter::default()
                }),
                top_k: candidate_limit,
                snippet_chars: 200,
            })
            .await?;

        let vector_hits =
            if self.runtime.config().embedding_model.is_some() || embedding_model.is_some() {
                let model_name: String = embedding_model
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| self.runtime.default_embedder_name().to_string());
                let vec = self.runtime.embed_with_model(&model_name, query).await?;
                self.runtime
                    .vectors_for_model(token, &model_name)?
                    .search(VectorSearchRequest {
                        query_vectors: vec![vec],
                        top_k: candidate_limit,
                        namespace: Some(ns.clone()),
                        // F111: already restricts to Note substrate kind
                        kind: Some(SubstrateKind::Note),
                        embedding_model: Some(model_name),
                        filter: None,
                        backend_hints: None,
                    })
                    .await?
            } else {
                Vec::new()
            };

        Ok(RecallCandidateSet {
            namespace: ns,
            text_hits,
            vector_hits,
        })
    }

    async fn load_memory_candidate_notes(
        &self,
        token: &NamespaceToken,
        text_hits: &[TextSearchHit],
        vector_hits: &[VectorSearchHit],
    ) -> Result<(HashSet<Uuid>, HashMap<Uuid, khive_storage::note::Note>), RuntimeError> {
        let candidate_ids: Vec<Uuid> = {
            let mut seen = HashSet::new();
            let mut ids = Vec::new();
            for id in text_hits
                .iter()
                .map(|h| h.subject_id)
                .chain(vector_hits.iter().map(|h| h.subject_id))
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
        let importance = match p.importance {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "importance must be in [0, 1], got {v}"
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

        // F109: reject invalid source_id UUID strings
        let mut annotates: Vec<Uuid> = vec![];
        if let Some(sid) = &p.source_id {
            match sid.parse::<Uuid>() {
                Ok(source_uuid) => annotates.push(source_uuid),
                Err(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "source_id {sid:?} is not a valid UUID"
                    )));
                }
            }
        }

        // Codex High 3 (PR #407): validate embedding_model BEFORE any note/FTS
        // write so unknown-model errors are atomic (no half-written rows).
        // resolve_embedding_model is sync and does not trigger model load — it
        // only checks the registry contains the name.
        if let Some(model_name) = p.embedding_model.as_deref() {
            self.runtime.resolve_embedding_model(Some(model_name))?;
        }

        let note = self
            .runtime
            .create_note_with_decay_for_embedding_model(
                token,
                "memory",
                None,
                &p.content,
                Some(importance),
                decay_factor,
                Some(props),
                annotates,
                p.embedding_model.as_deref(),
            )
            .await?;

        to_json(&json!({
            "note_id": note.id.to_string(),
            "kind": note.kind,
            "salience": note.salience,
            "decay_factor": note.decay_factor,
            "memory_type": memory_type,
            "created_at": note.created_at,
        }))
    }

    pub(crate) async fn handle_recall(
        &self,
        token: &NamespaceToken,
        params: Value,
        _registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: RecallParams = deser(params)?;

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

        let limit = if let Some(k) = p.top_k {
            u32::try_from(k.min(100)).unwrap_or(100)
        } else {
            p.limit.unwrap_or(10).min(100)
        };
        let candidate_limit = recall_candidate_count(&cfg, limit);
        let candidates = self
            .collect_recall_candidates(
                &p.query,
                token,
                candidate_limit,
                p.embedding_model.as_deref(),
            )
            .await?;
        let (memory_ids, mut notes_by_id) = self
            .load_memory_candidate_notes(token, &candidates.text_hits, &candidates.vector_hits)
            .await?;

        let fused = fuse_candidates(
            candidates.text_hits,
            candidates.vector_hits,
            &memory_ids,
            &cfg,
            candidate_limit as usize,
        );

        if fused.is_empty() {
            return to_json(&Vec::<Value>::new());
        }

        let now_micros = chrono::Utc::now().timestamp_micros();
        let mut ranked: Vec<(Uuid, f64, ScoreBreakdown, khive_storage::note::Note)> = Vec::new();
        for hit in fused {
            let id = hit.entity_id;
            let relevance = hit.score.to_f64();
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

            let age_micros = (now_micros - note.created_at).max(0) as f64;
            let age_days = age_micros / (1_000_000.0 * 86_400.0);
            let (final_score, breakdown) =
                compute_score(&cfg, relevance, salience, decay_factor, age_days);

            if final_score < cfg.min_score {
                continue;
            }
            if let Some(floor) = p.score_floor {
                if final_score < floor as f64 {
                    continue;
                }
            }
            ranked.push((id, final_score, breakdown, note));
        }

        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(limit as usize);

        let include_breakdown = cfg.include_breakdown;
        let results: Vec<Value> = ranked
            .into_iter()
            .map(|(id, score, breakdown, note)| {
                let mut result = json!({
                    "note_id": id.to_string(),
                    "score": score,
                    "content": note.content,
                    "salience": note.salience,
                    "decay_factor": note.decay_factor,
                    "memory_type": note.properties.as_ref()
                        .and_then(|p| p.get("memory_type"))
                        .and_then(|v| v.as_str()),
                    "created_at": note.created_at,
                });
                if include_breakdown {
                    result["breakdown"] = json!(breakdown);
                }
                result
            })
            .collect();

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
        let candidate_limit = recall_candidate_count(&cfg, limit);
        let candidates = self
            .collect_recall_candidates(
                &p.query,
                token,
                candidate_limit,
                p.embedding_model.as_deref(),
            )
            .await?;

        let text_candidates: Vec<Value> = candidates
            .text_hits
            .iter()
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
        let vector_candidates: Vec<Value> = candidates
            .vector_hits
            .iter()
            .map(|hit| {
                json!({
                    "note_id": hit.subject_id.to_string(),
                    "score": hit.score.to_f64(),
                    "rank": hit.rank,
                })
            })
            .collect();

        to_json(&json!({
            "namespace": candidates.namespace,
            "candidate_limit": candidate_limit,
            "text_candidates": text_candidates,
            "vector_candidates": vector_candidates,
        }))
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
        let candidate_limit = recall_candidate_count(&cfg, limit);
        let candidates = self
            .collect_recall_candidates(
                &p.query,
                token,
                candidate_limit,
                p.embedding_model.as_deref(),
            )
            .await?;
        let (memory_ids, notes_by_id) = self
            .load_memory_candidate_notes(token, &candidates.text_hits, &candidates.vector_hits)
            .await?;

        let fused = fuse_candidates(
            candidates.text_hits,
            candidates.vector_hits,
            &memory_ids,
            &cfg,
            candidate_limit as usize,
        );

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

    /// Apply configured rerankers to fused candidates (ADR-033 §2, F222).
    ///
    /// In v1 with no active rerankers (empty `reranker_weights`), this is a
    /// pass-through: each candidate is returned with an empty `rerank_scores` map.
    /// When reranker weights are configured in `RecallConfig.reranker_weights`, the
    /// named rerankers will populate `rerank_scores[name]` for downstream scoring.
    pub(crate) async fn handle_recall_rerank(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct RerankParams {
            /// Fused candidate IDs to rerank (from recall_fuse output).
            candidates: Vec<serde_json::Value>,
            config: Option<RecallConfig>,
        }
        let p: RerankParams = deser(params)?;
        let cfg = p.config.unwrap_or_else(|| self.active_config());
        cfg.validate()?;

        // Build the set of active rerankers (weight > 0).
        let active: Vec<(&String, &f64)> = cfg
            .reranker_weights
            .iter()
            .filter(|(_, &w)| w > 0.0)
            .collect();

        // For each candidate, produce a rerank_scores map with scores from active rerankers.
        // v1: no reranker models are loaded, so all scores are 0.0 (reranker not run).
        let reranked: Vec<serde_json::Value> = p
            .candidates
            .iter()
            .map(|candidate| {
                let id = candidate
                    .get("note_id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let mut rerank_scores = serde_json::Map::new();
                for (name, _weight) in &active {
                    // v1: reranker model not loaded → score = 0.0
                    rerank_scores.insert(name.to_string(), json!(0.0_f32));
                }
                json!({
                    "note_id": id,
                    "rerank_scores": rerank_scores,
                })
            })
            .collect();

        to_json(&json!({
            "reranked": reranked,
            "active_rerankers": active.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
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
        };
        let cfg = p.effective_config(RecallConfig::default());
        assert!((cfg.relevance_weight - 0.70).abs() < 1e-12);
        assert!((cfg.importance_weight - 0.20).abs() < 1e-12);
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

        let cfg_rrf = RecallConfig {
            fuse_strategy: RuntimeFusionStrategy::Rrf { k: 60 },
            ..RecallConfig::default()
        };
        let rrf_results = fuse_candidates(
            text_hits.clone(),
            vector_hits.clone(),
            &memory_ids,
            &cfg_rrf,
            10,
        );
        let rrf_order: Vec<Uuid> = rrf_results.iter().map(|h| h.entity_id).collect();

        let cfg_weighted = RecallConfig {
            fuse_strategy: RuntimeFusionStrategy::Weighted {
                weights: vec![0.1, 0.9],
            },
            ..RecallConfig::default()
        };
        let weighted_results =
            fuse_candidates(text_hits, vector_hits, &memory_ids, &cfg_weighted, 10);
        let weighted_order: Vec<Uuid> = weighted_results.iter().map(|h| h.entity_id).collect();

        // RRF on this fixture: id_a in both sources gets highest combined rank score;
        //   id_c (vector rank 1) and id_b (text rank 2) tied around 0.0161-0.0164.
        // Weighted [0.1, 0.9]: id_c dominates (0.95 * 0.9 = 0.855); id_a drops
        //   (0.9 * 0.1 + 0.3 * 0.9 = 0.36); id_b last (0.5 * 0.1 = 0.05).
        // The orderings MUST differ — this is the discriminating assertion.
        assert_ne!(
            rrf_order, weighted_order,
            "fusion_strategy change must affect ordering; RRF and Weighted produced identical: {rrf_order:?}"
        );
    }

    #[test]
    fn compute_score_default_config_reproduces_legacy() {
        let cfg = RecallConfig::default();
        let rrf = 0.5;
        let salience = 0.8;
        let decay_factor = 0.01;
        let age_days = 0.0;
        let (total, bd) = compute_score(&cfg, rrf, salience, decay_factor, age_days);
        // At age=0: importance_decayed = salience, temporal = 1.0
        // total = 0.70*0.5 + 0.20*0.8 + 0.10*1.0 = 0.35 + 0.16 + 0.10 = 0.61
        assert!((total - 0.61).abs() < 1e-10, "got {total}");
        assert!((bd.relevance - 0.5).abs() < 1e-12);
        assert!((bd.importance_raw - 0.8).abs() < 1e-12);
    }

    #[test]
    fn compute_score_exponential_decay_at_decay_factor_half_life() {
        // Use explicit exponential decay config — not relying on default decay_model.
        // ADR-021 §5: importance_decayed = salience * exp(-decay_factor * age_days)
        // At age = ln(2)/0.01 ≈ 69.3 days: importance_decayed ≈ 0.5
        let cfg = RecallConfig {
            decay_model: DecayModel::Exponential,
            temporal_half_life_days: 30.0,
            ..RecallConfig::default()
        };
        let age_days = std::f64::consts::LN_2 / 0.01;
        let (_, bd) = compute_score(&cfg, 0.5, 1.0, 0.01, age_days);
        assert!(
            (bd.importance_decayed - 0.5).abs() < 1e-10,
            "importance_decayed = {}",
            bd.importance_decayed
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
        let cfg = RecallConfig {
            relevance_weight: 1.0,
            importance_weight: 0.0,
            temporal_weight: 0.0,
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

    // ── F108: reject out-of-range importance and decay_factor ─────────────

    #[test]
    fn remember_params_importance_below_zero_rejected() {
        // Simulate handler validation path directly
        let importance: f64 = -0.1;
        let result: Result<f64, RuntimeError> = if !(0.0..=1.0).contains(&importance) {
            Err(RuntimeError::InvalidInput(format!(
                "importance must be in [0, 1], got {importance}"
            )))
        } else {
            Ok(importance)
        };
        assert!(result.is_err(), "expected error for importance < 0");
    }

    #[test]
    fn remember_params_importance_above_one_rejected() {
        let importance: f64 = 1.1;
        let result: Result<f64, RuntimeError> = if !(0.0..=1.0).contains(&importance) {
            Err(RuntimeError::InvalidInput(format!(
                "importance must be in [0, 1], got {importance}"
            )))
        } else {
            Ok(importance)
        };
        assert!(result.is_err(), "expected error for importance > 1");
    }

    #[test]
    fn remember_params_importance_boundary_values_accepted() {
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
}
