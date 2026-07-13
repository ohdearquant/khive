//! Dotted sub-verb handlers: recall.embed, recall.candidates, recall.fuse, recall.rerank, recall.score.

use serde::Deserialize;
use serde_json::{json, Value};

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};

use crate::config::RecallConfig;
use crate::rerank::{weighted_rerank, RerankFeatures};
use crate::MemoryPack;

use super::common::{
    compute_score, deser, fuse_candidates, make_pipeline, note_matches_tags,
    recall_candidate_count, search_source_label, to_json, validate_memory_type,
    RecallCandidateParams, RecallParams, TextSnippetPolicy, DEFAULT_DECAY_EPISODIC,
    RECALL_DIAGNOSTIC_SNIPPET_CHARS,
};

impl MemoryPack {
    pub(crate) async fn handle_recall_embed(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct EmbedParams {
            query: String,
            #[serde(default)]
            include_embeddings: bool,
        }
        let p: EmbedParams = deser(params)?;

        let model_names = self.runtime.registered_embedding_model_names();
        if model_names.is_empty() {
            return to_json(&json!({
                "embedding": null,
                "model": null,
                "engines": [],
            }));
        }

        let mut engines: Vec<Value> = Vec::with_capacity(model_names.len());
        let mut primary_embedding: Option<Vec<f32>> = None;
        let primary_model = self.runtime.default_embedder_name().to_owned();

        for model_name in &model_names {
            match self
                .runtime
                .embed_query_with_model(model_name, &p.query)
                .await
            {
                Ok(vec) => {
                    let dims = vec.len();
                    if primary_embedding.is_none() || model_name == &primary_model {
                        primary_embedding = Some(vec.clone());
                    }
                    let mut engine = json!({
                        "model": model_name,
                        "dimensions": dims,
                    });
                    if p.include_embeddings {
                        engine["embedding"] = json!(vec);
                    }
                    engines.push(engine);
                }
                Err(e) => {
                    engines.push(json!({
                        "model": model_name,
                        "error": e.to_string(),
                    }));
                }
            }
        }

        match primary_embedding {
            Some(vec) => {
                let dims = vec.len();
                let mut response = json!({
                    "dimensions": dims,
                    "engines": engines,
                });
                if p.include_embeddings {
                    response["embedding"] = json!(vec);
                }
                to_json(&response)
            }
            None => to_json(&json!({
                "embedding": null,
                "model": null,
                "engines": engines,
            })),
        }
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
        let effective_fts_gather_cand = crate::config::RecallFtsGatherConfig::from_env()
            .map_err(|e| RuntimeError::InvalidInput(format!("fts_gather env parse error: {e}")))?
            .unwrap_or_else(|| cfg.fts_gather.clone());
        let ann_overfetch_max_rounds_cand = cfg
            .ann_overfetch_max_rounds
            .unwrap_or_else(super::common::ann_overfetch_max_rounds);
        let ann_ready_timeout_ms_cand = cfg
            .ann_ready_timeout_ms
            .unwrap_or_else(super::common::ann_ready_timeout_ms);

        let candidates = self
            .collect_recall_candidates(
                &p.query,
                token,
                RecallCandidateParams {
                    candidate_limit,
                    embedding_model: p.embedding_model.as_deref(),
                    cjk_fts_bypass: false,
                    use_multilingual: false,
                    scoring_cfg: &scoring_cfg,
                    snippet_policy: TextSnippetPolicy::Include {
                        chars: RECALL_DIAGNOSTIC_SNIPPET_CHARS,
                    },
                    fts_gather: &effective_fts_gather_cand,
                    ann_overfetch_max_rounds: ann_overfetch_max_rounds_cand,
                    ann_ready_timeout_ms: ann_ready_timeout_ms_cand,
                },
            )
            .await?;

        let (memory_ids, _) = self.load_memory_candidate_notes(token, &candidates).await?;
        let text_candidates: Vec<Value> = candidates
            .text_hits
            .iter()
            .filter(|hit| memory_ids.contains(&hit.subject_id))
            .map(|hit| {
                json!({
                    "id": hit.subject_id.to_string(),
                    "score": hit.score.to_f64(),
                    "rank": hit.rank,
                    "title": hit.title.as_deref(),
                    "snippet": hit.snippet.as_deref(),
                })
            })
            .collect();

        let all_vector_hits = candidates.all_vector_hits();
        // Filter to memory_ids: this drops over-fetched hits from outside the
        // caller's visible namespace set (those were filtered at hydration time).
        let vector_candidates: Vec<Value> = all_vector_hits
            .iter()
            .filter(|hit| memory_ids.contains(&hit.subject_id))
            .map(|hit| {
                json!({
                    "id": hit.subject_id.to_string(),
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
            // Fix (#733): same fix as
            // `handle_recall`'s verbose multi-model breakdown — the global
            // per-model ANN candidate lists are pre-hydration and must be
            // filtered through `memory_ids` (the visible-namespace
            // post-filter already applied to `vector_candidates` above)
            // before serialization, or this diagnostic view can leak
            // off-namespace candidate UUIDs.
            let per_model: serde_json::Map<String, Value> = candidates
                .vector_hits_per_model
                .iter()
                .map(|(model, hits)| {
                    let hits_json: Vec<Value> = hits
                        .iter()
                        .filter(|h| memory_ids.contains(&h.subject_id))
                        .map(|h| {
                            json!({
                                "id": h.subject_id.to_string(),
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
        let effective_fts_gather_fuse = crate::config::RecallFtsGatherConfig::from_env()
            .map_err(|e| RuntimeError::InvalidInput(format!("fts_gather env parse error: {e}")))?
            .unwrap_or_else(|| cfg.fts_gather.clone());
        let ann_overfetch_max_rounds_fuse = cfg
            .ann_overfetch_max_rounds
            .unwrap_or_else(super::common::ann_overfetch_max_rounds);
        let ann_ready_timeout_ms_fuse = cfg
            .ann_ready_timeout_ms
            .unwrap_or_else(super::common::ann_ready_timeout_ms);

        let candidates = self
            .collect_recall_candidates(
                &p.query,
                token,
                RecallCandidateParams {
                    candidate_limit,
                    embedding_model: p.embedding_model.as_deref(),
                    cjk_fts_bypass: false,
                    use_multilingual: false,
                    scoring_cfg: &scoring_cfg_fuse,
                    snippet_policy: TextSnippetPolicy::Include {
                        chars: RECALL_DIAGNOSTIC_SNIPPET_CHARS,
                    },
                    fts_gather: &effective_fts_gather_fuse,
                    ann_overfetch_max_rounds: ann_overfetch_max_rounds_fuse,
                    ann_ready_timeout_ms: ann_ready_timeout_ms_fuse,
                },
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
                if let Some(filter_tags) = p.tags.as_ref().filter(|tags| !tags.is_empty()) {
                    if !note_matches_tags(note.properties.as_ref(), filter_tags, p.tag_mode) {
                        return None;
                    }
                }
                Some(json!({
                    "id": hit.entity_id.to_string(),
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

    pub(crate) async fn handle_recall_rerank(&self, params: Value) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct RerankParams {
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
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                if cfg.reranker_weights.is_empty() {
                    return json!({
                        "id": id,
                        "rerank_scores": {},
                        "rerank_score": 0.0_f64,
                    });
                }

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
                    .unwrap_or(DEFAULT_DECAY_EPISODIC);
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
                    "id": id,
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
        for (name, val) in [
            ("rrf", p.rrf),
            ("salience", p.salience),
            ("decay_factor", p.decay_factor),
            ("age_days", p.age_days),
        ] {
            if !val.is_finite() {
                return Err(RuntimeError::InvalidInput(format!(
                    "{name} must be a finite number, got {val}"
                )));
            }
        }
        let cfg = p.config.unwrap_or_else(|| self.active_config());
        cfg.validate()?;
        let pipeline = make_pipeline(&cfg);
        let (total, breakdown) = compute_score(
            &cfg,
            &pipeline,
            p.rrf,
            p.salience,
            p.decay_factor,
            p.age_days,
        );
        to_json(&json!({
            "total": total,
            "breakdown": breakdown,
        }))
    }
}
