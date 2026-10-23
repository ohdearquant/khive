//! Handler for `memory.remember`.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::keyed_memory::{
    create_keyed_memory_with_receipt_and_report, validate_memory_key, KeyedMemorySpec,
};
use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{micros_to_iso, Namespace, NamespaceToken, RuntimeError};
use khive_storage::types::{Direction, NeighborQuery};
use khive_storage::EdgeRelation;
use khive_types::{Details, KhiveError};

use crate::ann;
use crate::MemoryPack;

use super::common::{
    deser, to_json, validate_memory_type, RememberParams, DEFAULT_DECAY_EPISODIC,
    DEFAULT_DECAY_SEMANTIC, DEFAULT_SALIENCE_EPISODIC, DEFAULT_SALIENCE_SEMANTIC,
};

fn receipt_issuance_error(error: RuntimeError, memory_id: Uuid, replayed: bool) -> RuntimeError {
    let reason = match error.refusal_source() {
        RuntimeError::Khive(error) => {
            match error.details().and_then(|details| details.get("reason")) {
                Some("visibility_key_unavailable") => "visibility_key_unavailable",
                Some("visibility_nonce_unavailable") => "visibility_nonce_unavailable",
                Some("receipt_store_unavailable") => "receipt_store_unavailable",
                _ => "visibility_receipt_unavailable",
            }
        }
        _ => "visibility_receipt_unavailable",
    };
    KhiveError::unavailable("freshness_unmet: visibility receipt could not be issued")
        .with_details(Details::new_owned([
            ("reason", reason.to_owned()),
            ("memory_id", memory_id.to_string()),
            (
                "receipt_phase",
                if replayed {
                    "exact_replay"
                } else {
                    "post_commit"
                }
                .to_owned(),
            ),
        ]))
        .into()
}

impl MemoryPack {
    pub(crate) async fn handle_remember(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: RememberParams = deser(params)?;
        if let Some(key) = p.key.as_deref() {
            validate_memory_key(key)?;
        }
        if p.content.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "content must not be empty".into(),
            ));
        }

        let memory_type = p.memory_type.as_deref().unwrap_or("episodic");
        validate_memory_type(memory_type)?;

        // Explicit namespace wins; otherwise episodic uses actor scope and semantic uses local.
        // Direct-call defense in depth mirrors dispatch's Rule-3 namespace escape.
        let write_token_owned: Option<NamespaceToken> = if let Some(ns_str) = p.namespace.as_deref()
        {
            let ns = Namespace::parse(ns_str).map_err(|e| {
                RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
            })?;
            Some(token.with_namespace(ns))
        } else if memory_type == "episodic" {
            let actor_id = token.actor().id.as_str();
            let ns = Namespace::parse(actor_id).map_err(|e| {
                RuntimeError::InvalidInput(format!(
                    "actor id {actor_id:?} is not a valid namespace: {e}"
                ))
            })?;
            Some(token.with_namespace(ns))
        } else {
            None
        };
        let write_token: &NamespaceToken = write_token_owned.as_ref().unwrap_or(token);

        let salience = match p.salience {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "salience must be in [0, 1], got {v}"
                )));
            }
            Some(v) => v,
            // Short-lived episodes start below durable semantic facts.
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
            // Episodic context decays at ~35 days; semantic facts at ~139 days.
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
                            "source_id {:?}: no record matches this prefix",
                            mask_for_redaction_surface(RedactionSurface::GateProbe, sid)
                        )));
                    }
                    Err(e) => return Err(e),
                }
            } else {
                return Err(RuntimeError::InvalidInput(format!(
                    "source_id {:?} is not a valid UUID or 8-char short ID",
                    mask_for_redaction_surface(RedactionSurface::GateProbe, sid)
                )));
            }
        }

        if let Some(model_name) = p.embedding_model.as_deref() {
            self.runtime.resolve_embedding_model(Some(model_name))?;
        }

        self.runtime.notes(write_token)?;
        let sealing = self.runtime.ensure_visibility_receipt_key_if_configured()?;

        let annotates_target = annotates.first().copied();

        let (note, keyed_edge_id, replayed, vector_fences, embedding_truncation) =
            if let Some(key) = p.key.as_deref() {
                create_keyed_memory_with_receipt_and_report(
                    &self.runtime,
                    write_token,
                    KeyedMemorySpec {
                        content: &p.content,
                        key,
                        salience,
                        decay_factor,
                        properties: props,
                        source_id: annotates_target,
                        embedding_model: p.embedding_model.as_deref(),
                    },
                )
                .await?
            } else {
                // Retain both diagnostics after the committed write so bounded
                // embedding input does not skip the ANN generation bump below.
                let (note, fences, truncation) = self
                    .runtime
                    .create_note_with_decay_for_embedding_model_with_visibility_and_report(
                        write_token,
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
                (note, None, false, fences, truncation)
            };

        if !replayed {
            // Preserve the stale graph as a fast fallback; generation is the invalidation signal.
            let affected_models: Vec<String> = match p.embedding_model.as_deref() {
                Some(model) => vec![model.to_owned()],
                None => self.runtime.registered_embedding_model_names(),
            };
            for model in affected_models {
                // Bump BEFORE warming so this write is included in the required generation floor.
                let key = ann::AnnKey::from_token(&model);
                ann::bump_generation(&self.ann, &key).await;
                ann::ensure_ann_background(&self.runtime, write_token, &self.ann, &model).await;
            }
        }

        // A replay answers from the stored memory, not from the request: the
        // annotation edge is the one the stored note still owns (whatever the
        // replay named as `source_id`), and `memory_type` is the value it was
        // written with, so a replay that sends a different type cannot echo it
        // back as if stored.
        let edge_id = if let Some(id) = keyed_edge_id {
            Some(id.to_string())
        } else if replayed || annotates_target.is_some() {
            let neighbors = self
                .runtime
                .neighbors_with_query(
                    write_token,
                    note.id,
                    NeighborQuery {
                        direction: Direction::Out,
                        relations: Some(vec![EdgeRelation::Annotates]),
                        limit: None,
                        min_weight: None,
                    },
                )
                .await;
            let neighbors = match neighbors {
                Ok(hits) => hits,
                // Prune can remove a replayed holder after its key lookup.
                Err(RuntimeError::NotFound(message))
                    if replayed && message == format!("neighbor anchor {} not found", note.id) =>
                {
                    Vec::new()
                }
                Err(error) => return Err(error),
            };
            neighbors
                .into_iter()
                .find(|hit| replayed || annotates_target == Some(hit.node_id))
                .map(|hit| hit.edge_id.to_string())
        } else {
            None
        };
        let response_memory_type: String = if replayed {
            note.properties
                .as_ref()
                .and_then(|pr| pr.get("memory_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("episodic")
                .to_owned()
        } else {
            memory_type.to_owned()
        };

        let visibility_token = if sealing {
            Some(
                self.runtime
                    .seal_visibility_receipt(&note.namespace, &vector_fences)
                    .map_err(|error| receipt_issuance_error(error, note.id, replayed))?,
            )
        } else {
            None
        };
        let mut response = json!({
            "id": note.id.to_string(),
            "kind": note.kind,
            "salience": note.salience,
            "decay_factor": note.decay_factor,
            "memory_type": response_memory_type,
            "created_at": micros_to_iso(note.created_at),
            "visibility_token": visibility_token,
        });
        if visibility_token.is_none() {
            response["visibility_token_reason"] = json!("visibility_key_unavailable");
        }
        if let Some(eid) = edge_id {
            response["edge_id"] = json!(eid);
        }
        if replayed {
            response["replayed"] = json!(true);
        }
        if embedding_truncation.any_truncated() {
            response["warnings"] =
                json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]);
        }
        to_json(&response)
    }
}

#[cfg(test)]
#[path = "remember_tests.rs"]
mod tests;
