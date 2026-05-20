use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{RuntimeError, VerbRegistry};
use khive_storage::types::{TextFilter, TextQueryMode, TextSearchRequest, VectorSearchRequest};
use khive_types::SubstrateKind;

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
}

impl MemoryPack {
    pub(crate) async fn handle_remember(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: RememberParams = deser(params)?;

        if let Some(mt) = &p.memory_type {
            validate_memory_type(mt)?;
        }

        let importance = p.importance.unwrap_or(0.5).clamp(0.0, 1.0);
        let decay_factor = p.decay_factor.unwrap_or(0.01).max(0.0);

        let mut props = serde_json::json!({});
        if let Some(mt) = &p.memory_type {
            props["memory_type"] = json!(mt);
        }
        if let Some(sid) = &p.source_id {
            props["source_id"] = json!(sid);
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

        let limit = p.limit.unwrap_or(10).min(100);
        let min_score = p.min_score.unwrap_or(0.0);
        let candidates = limit.saturating_mul(20).max(40);
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

        // RRF fusion (raw f64)
        let mut buckets: HashMap<Uuid, f64> = HashMap::new();
        for (i, hit) in text_hits.into_iter().enumerate() {
            let rank = (i + 1) as f64;
            *buckets.entry(hit.subject_id).or_default() += 1.0 / (RRF_K + rank);
        }
        for (i, hit) in vector_hits.into_iter().enumerate() {
            let rank = (i + 1) as f64;
            *buckets.entry(hit.subject_id).or_default() += 1.0 / (RRF_K + rank);
        }

        if buckets.is_empty() {
            return to_json(&Vec::<Value>::new());
        }

        let note_store = self.runtime.notes(p.namespace.as_deref())?;
        let now_micros = chrono::Utc::now().timestamp_micros();

        let mut ranked: Vec<(Uuid, f64, khive_storage::note::Note)> = Vec::new();
        for (&id, &rrf) in &buckets {
            let note = match note_store.get_note(id).await? {
                Some(n) if n.deleted_at.is_none() => n,
                _ => continue,
            };
            if note.kind != "memory" {
                continue;
            }
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
            if let Some(min_sal) = p.min_salience {
                if note.salience < min_sal {
                    continue;
                }
            }

            let age_micros = (now_micros - note.created_at).max(0) as f64;
            let age_days = age_micros / (1_000_000.0 * 86_400.0);
            let effective_importance = note.salience * (-note.decay_factor * age_days).exp();
            let temporal = (-age_days / 30.0).exp();
            let final_score = rrf * 0.70 + effective_importance * 0.20 + temporal * 0.10;

            if final_score < min_score {
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
}
