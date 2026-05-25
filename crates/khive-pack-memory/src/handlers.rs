use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::fusion::fuse_with_strategy;
use khive_runtime::{NamespaceToken, RuntimeError, SearchHit, SearchSource, VerbRegistry};
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
}

#[derive(Deserialize)]
struct RecallParams {
    query: String,
    limit: Option<u32>,
    memory_type: Option<String>,
    min_score: Option<f64>,
    min_salience: Option<f64>,
    config: Option<RecallConfig>,
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

fn fuse_candidates(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    memory_ids: &HashSet<Uuid>,
    cfg: &RecallConfig,
    limit: usize,
) -> Vec<SearchHit> {
    let text: Vec<TextSearchHit> = text_hits
        .into_iter()
        .filter(|h| memory_ids.contains(&h.subject_id))
        .collect();
    let vec: Vec<VectorSearchHit> = vector_hits
        .into_iter()
        .filter(|h| memory_ids.contains(&h.subject_id))
        .collect();
    fuse_with_strategy(text, vec, &cfg.fuse_strategy, limit)
}

impl MemoryPack {
    async fn collect_recall_candidates(
        &self,
        query: &str,
        token: &NamespaceToken,
        candidate_limit: u32,
    ) -> Result<RecallCandidateSet, RuntimeError> {
        let ns = token.namespace().as_str().to_string();
        let text_hits = self
            .runtime
            .text_for_notes(token)?
            .search(TextSearchRequest {
                query: query.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.clone()],
                    ..TextFilter::default()
                }),
                top_k: candidate_limit,
                snippet_chars: 200,
            })
            .await?;

        let vector_hits = if self.runtime.config().embedding_model.is_some() {
            let vec = self.runtime.embed(query).await?;
            self.runtime
                .vectors(token)?
                .search(VectorSearchRequest {
                    query_embedding: vec,
                    top_k: candidate_limit,
                    namespace: Some(ns.clone()),
                    kind: Some(SubstrateKind::Note),
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

        if let Some(mt) = &p.memory_type {
            validate_memory_type(mt)?;
        }

        let importance = p.importance.unwrap_or(0.5).clamp(0.0, 1.0);
        let decay_factor = p.decay_factor.unwrap_or(0.01).clamp(0.0, 1.0);

        let mut props = serde_json::json!({});
        if let Some(mt) = &p.memory_type {
            props["memory_type"] = json!(mt);
        }
        if let Some(tags) = &p.tags {
            if !tags.is_empty() {
                props["tags"] = json!(tags);
            }
        }
        let properties = if props.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            None
        } else {
            Some(props)
        };

        let mut annotates: Vec<Uuid> = vec![];
        if let Some(sid) = &p.source_id {
            if let Ok(source_uuid) = sid.parse::<Uuid>() {
                annotates.push(source_uuid);
            }
        }

        let note = self
            .runtime
            .create_note_with_decay(
                token,
                "memory",
                None,
                &p.content,
                Some(importance),
                decay_factor,
                properties,
                annotates,
            )
            .await?;

        to_json(&json!({
            "note_id": note.id.to_string(),
            "kind": note.kind,
            "salience": note.salience,
            "decay_factor": note.decay_factor,
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

        let cfg = p.effective_config(self.active_config());
        cfg.validate()?;

        let limit = p.limit.unwrap_or(10).min(100);
        let candidate_limit = recall_candidate_count(&cfg, limit);
        let candidates = self
            .collect_recall_candidates(&p.query, token, candidate_limit)
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
            .collect_recall_candidates(&p.query, token, candidate_limit)
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
            .collect_recall_candidates(&p.query, token, candidate_limit)
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
        };
        let cfg = p.effective_config(RecallConfig::default());
        assert!((cfg.relevance_weight - 0.50).abs() < 1e-12);
        // legacy min_score overrides config's default
        assert!((cfg.min_score - 0.1).abs() < 1e-12);
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
    fn compute_score_exponential_decay_at_half_life() {
        let cfg = RecallConfig::default(); // half_life = 30 days
        let (_, bd) = compute_score(&cfg, 0.5, 1.0, 0.01, 30.0);
        // At age = half_life: importance_decayed ≈ 0.5, temporal ≈ 0.5
        assert!(
            (bd.importance_decayed - 0.5).abs() < 1e-10,
            "importance_decayed = {}",
            bd.importance_decayed
        );
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
}
