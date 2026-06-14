//! Handler for `memory.remember`.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, NamespaceToken, RuntimeError};
use khive_storage::types::{Direction, NeighborQuery};
use khive_storage::EdgeRelation;

use crate::ann;
use crate::MemoryPack;

use super::common::{
    deser, to_json, validate_memory_type, RememberParams, DEFAULT_DECAY_EPISODIC,
    DEFAULT_DECAY_SEMANTIC, DEFAULT_SALIENCE_EPISODIC, DEFAULT_SALIENCE_SEMANTIC,
};

impl MemoryPack {
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

        let salience = match p.salience {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "salience must be in [0, 1], got {v}"
                )));
            }
            Some(v) => v,
            // episodic: lower default — session events decay quickly and should not
            // crowd out timeless semantic memories in recall ranking.
            // semantic: higher default — durable facts warrant stronger base weight.
            None => match memory_type {
                "semantic" => DEFAULT_SALIENCE_SEMANTIC,
                _ => DEFAULT_SALIENCE_EPISODIC,
            },
        };
        let decay_factor = match p.decay_factor {
            Some(v) if !v.is_finite() || v < 0.0 => {
                return Err(RuntimeError::InvalidInput(format!(
                    "decay_factor must be a finite number >= 0, got {v}"
                )));
            }
            Some(v) => v,
            // episodic: ~35-day half-life — short-lived session context ages out fast.
            // semantic: ~139-day half-life — durable facts stay relevant much longer.
            None => match memory_type {
                "semantic" => DEFAULT_DECAY_SEMANTIC,
                _ => DEFAULT_DECAY_EPISODIC,
            },
        };

        let mut props = json!({ "memory_type": memory_type });
        if let Some(tags) = &p.tags {
            if !tags.is_empty() {
                props["tags"] = json!(tags);
            }
        }

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

        if let Some(model_name) = p.embedding_model.as_deref() {
            self.runtime.resolve_embedding_model(Some(model_name))?;
        }

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

        {
            let ns = token.namespace().as_str().to_owned();
            ann::invalidate_namespace(&self.runtime, &self.ann, &ns).await;
            let affected_models: Vec<String> = match p.embedding_model.as_deref() {
                Some(model) => vec![model.to_owned()],
                None => self.runtime.registered_embedding_model_names(),
            };
            for model in affected_models {
                ann::ensure_ann_background(&self.runtime, token, &self.ann, &model).await;
            }
        }

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
            "id": note.id.to_string(),
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
}
