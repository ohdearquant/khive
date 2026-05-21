use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{RuntimeError, VerbRegistry};
use khive_storage::types::{TextFilter, TextQueryMode, TextSearchRequest, VectorSearchRequest};
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
    namespace: Option<String>,
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
    namespace: Option<String>,
    limit: Option<u32>,
    memory_type: Option<String>,
    min_score: Option<f64>,
    min_salience: Option<f64>,
    config: Option<RecallConfig>,
}

impl RecallParams {
    /// Merge per-call config with legacy field overrides.
    /// Priority: explicit config fields > legacy top-level fields > defaults.
    fn effective_config(&self) -> RecallConfig {
        let mut cfg = self.config.clone().unwrap_or_default();
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

impl MemoryPack {
    pub(crate) async fn handle_remember(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: RememberParams = deser(params)?;

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
                p.namespace.as_deref(),
                "memory",
                None,
                &p.content,
                importance,
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
        params: Value,
        _registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        const RRF_K: f64 = 60.0;
        let p: RecallParams = deser(params)?;

        if let Some(mt) = &p.memory_type {
            validate_memory_type(mt)?;
        }

        let cfg = p.effective_config();
        cfg.validate()?;

        let limit = p.limit.unwrap_or(10).min(100);
        let candidates = limit.saturating_mul(cfg.candidate_multiplier).max(40);
        let ns = self.runtime.ns(p.namespace.as_deref()).to_string();

        // FTS search over notes index
        let text_hits = self
            .runtime
            .text_for_notes(p.namespace.as_deref())?
            .search(TextSearchRequest {
                query: p.query.clone(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.clone()],
                    ..TextFilter::default()
                }),
                top_k: candidates,
                snippet_chars: 200,
            })
            .await?;

        // Vector search if embedding model is configured
        let vector_hits = if self.runtime.config().embedding_model.is_some() {
            let vec = self.runtime.embed(&p.query).await?;
            self.runtime
                .vectors(p.namespace.as_deref())?
                .search(VectorSearchRequest {
                    query_embedding: vec,
                    top_k: candidates,
                    namespace: Some(ns.clone()),
                    kind: Some(SubstrateKind::Note),
                })
                .await?
        } else {
            vec![]
        };

        // Pre-filter candidates to memory kind before RRF fusion so non-memory
        // notes do not consume ranked-slot budget (ADR-036 §6).
        let note_store = self.runtime.notes(p.namespace.as_deref())?;
        let now_micros = chrono::Utc::now().timestamp_micros();

        let mut memory_ids: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        let candidate_ids: Vec<Uuid> = {
            let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
            let mut ids: Vec<Uuid> = Vec::new();
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
        let mut notes_by_id: HashMap<Uuid, khive_storage::note::Note> = HashMap::new();
        let batch = note_store.get_notes_batch(&candidate_ids).await?;
        for note in batch {
            if note.deleted_at.is_none() && note.kind == "memory" {
                memory_ids.insert(note.id);
                notes_by_id.insert(note.id, note);
            }
        }

        // RRF fusion (raw f64) — only over memory-kind candidates.
        let mut buckets: HashMap<Uuid, f64> = HashMap::new();
        for (i, hit) in text_hits.into_iter().enumerate() {
            if memory_ids.contains(&hit.subject_id) {
                let rank = (i + 1) as f64;
                *buckets.entry(hit.subject_id).or_default() += 1.0 / (RRF_K + rank);
            }
        }
        for (i, hit) in vector_hits.into_iter().enumerate() {
            if memory_ids.contains(&hit.subject_id) {
                let rank = (i + 1) as f64;
                *buckets.entry(hit.subject_id).or_default() += 1.0 / (RRF_K + rank);
            }
        }

        if buckets.is_empty() {
            return to_json(&Vec::<Value>::new());
        }

        let mut ranked: Vec<(Uuid, f64, khive_storage::note::Note)> = Vec::new();
        for (&id, &rrf) in &buckets {
            let note = match notes_by_id.remove(&id) {
                Some(n) => n,
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
            if note.salience < cfg.min_salience {
                continue;
            }

            let age_micros = (now_micros - note.created_at).max(0) as f64;
            let age_days = age_micros / (1_000_000.0 * 86_400.0);
            let (final_score, _breakdown) =
                compute_score(&cfg, rrf, note.salience, note.decay_factor, age_days);

            if final_score < cfg.min_score {
                continue;
            }
            ranked.push((id, final_score, note));
        }

        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(limit as usize);

        let results: Vec<Value> = ranked
            .into_iter()
            .map(|(id, score, note)| {
                json!({
                    "note_id": id.to_string(),
                    "score": score,
                    "content": note.content,
                    "salience": note.salience,
                    "decay_factor": note.decay_factor,
                    "memory_type": note.properties.as_ref()
                        .and_then(|p| p.get("memory_type"))
                        .and_then(|v| v.as_str()),
                    "created_at": note.created_at,
                })
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
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        struct CandidatesParams {
            query: String,
            namespace: Option<String>,
            limit: Option<u32>,
            config: Option<RecallConfig>,
        }
        let p: CandidatesParams = deser(params)?;
        let cfg = p.config.unwrap_or_default();
        let limit = p.limit.unwrap_or(10).min(100);
        let candidates = limit.saturating_mul(cfg.candidate_multiplier).max(40);
        let ns = self.runtime.ns(p.namespace.as_deref()).to_string();

        let text_hits = self
            .runtime
            .text_for_notes(p.namespace.as_deref())?
            .search(TextSearchRequest {
                query: p.query.clone(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.clone()],
                    ..TextFilter::default()
                }),
                top_k: candidates,
                snippet_chars: 200,
            })
            .await?;

        let vector_hits = if self.runtime.config().embedding_model.is_some() {
            let vec = self.runtime.embed(&p.query).await?;
            self.runtime
                .vectors(p.namespace.as_deref())?
                .search(VectorSearchRequest {
                    query_embedding: vec,
                    top_k: candidates,
                    namespace: Some(ns),
                    kind: Some(SubstrateKind::Note),
                })
                .await?
        } else {
            vec![]
        };

        to_json(&json!({
            "text_hits": text_hits.len(),
            "vector_hits": vector_hits.len(),
        }))
    }

    pub(crate) async fn handle_recall_fuse(
        &self,
        params: Value,
        _registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        // Thin wrapper exposing the RRF fusion step for diagnostics.
        // Full recall pipeline is in handle_recall; this exposes intermediate state.
        self.handle_recall(params, _registry).await
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
        let cfg = p.config.unwrap_or_default();
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
            namespace: None,
            limit: None,
            memory_type: None,
            min_score: None,
            min_salience: None,
            config: None,
        };
        let cfg = p.effective_config();
        assert!((cfg.relevance_weight - 0.70).abs() < 1e-12);
        assert!((cfg.importance_weight - 0.20).abs() < 1e-12);
        assert!((cfg.temporal_weight - 0.10).abs() < 1e-12);
    }

    #[test]
    fn effective_config_legacy_overrides() {
        let p = RecallParams {
            query: "test".to_string(),
            namespace: None,
            limit: None,
            memory_type: None,
            min_score: Some(0.5),
            min_salience: Some(0.3),
            config: None,
        };
        let cfg = p.effective_config();
        assert!((cfg.min_score - 0.5).abs() < 1e-12);
        assert!((cfg.min_salience - 0.3).abs() < 1e-12);
    }

    #[test]
    fn effective_config_explicit_config_wins() {
        let p = RecallParams {
            query: "test".to_string(),
            namespace: None,
            limit: None,
            memory_type: None,
            min_score: Some(0.1),
            min_salience: None,
            config: Some(RecallConfig {
                relevance_weight: 0.50,
                ..RecallConfig::default()
            }),
        };
        let cfg = p.effective_config();
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
