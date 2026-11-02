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

/// Most keys a `properties` object may carry.
const MAX_PROPERTY_KEYS: usize = 32;
/// Longest accepted `properties` key, in characters.
const MAX_PROPERTY_KEY_CHARS: usize = 64;
/// Largest accepted serialized `properties` object, in bytes.
const MAX_PROPERTIES_BYTES: usize = 8192;
/// Keys that have their own parameter (`memory_type`, `tags`) or are written by
/// the runtime (`attribution`), so a caller may not set them through `properties`.
const RESERVED_PROPERTY_KEYS: [&str; 3] = ["memory_type", "tags", "attribution"];

/// A key is 1-64 characters: a lowercase ASCII letter first, then lowercase
/// ASCII letters, digits or `_`.
fn is_valid_property_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    key.chars().count() <= MAX_PROPERTY_KEY_CHARS
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Validate a caller-supplied `properties` value and return its entries.
///
/// The value must be a JSON object with at most 32 keys. Each key is 1-64
/// characters (a lowercase ASCII letter, then lowercase letters, digits or `_`)
/// and is not reserved. Each value is a string, a finite number, a boolean or an
/// array of strings. The serialized object is at most 8192 bytes; an object over
/// the limit is refused, never truncated. Every refusal is an
/// `InvalidInput` that names the offending key and the rule.
fn validate_properties(raw: &Value) -> Result<serde_json::Map<String, Value>, RuntimeError> {
    let object = raw.as_object().ok_or_else(|| {
        RuntimeError::InvalidInput(
            "properties must be a JSON object whose values are strings, finite numbers, booleans or arrays of strings"
                .into(),
        )
    })?;
    if object.len() > MAX_PROPERTY_KEYS {
        return Err(RuntimeError::InvalidInput(format!(
            "properties has {} keys; at most {} keys are allowed",
            object.len(),
            MAX_PROPERTY_KEYS
        )));
    }
    for (key, value) in object {
        if !is_valid_property_key(key) {
            let mut shown: String = key.chars().take(MAX_PROPERTY_KEY_CHARS).collect();
            if key.chars().count() > MAX_PROPERTY_KEY_CHARS {
                shown.push_str("...");
            }
            return Err(RuntimeError::InvalidInput(format!(
                "properties key {:?} is invalid: a key is 1-{} characters, a lowercase ASCII letter first, then lowercase letters, digits or '_'",
                mask_for_redaction_surface(RedactionSurface::GateProbe, &shown),
                MAX_PROPERTY_KEY_CHARS
            )));
        }
        if RESERVED_PROPERTY_KEYS.contains(&key.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "properties key {key:?} is reserved: memory_type and tags have their own parameters and attribution is written by the runtime"
            )));
        }
        let refusal: Option<&str> = match value {
            Value::String(_) | Value::Bool(_) => None,
            Value::Number(n) => {
                if n.as_f64().is_some_and(f64::is_finite) {
                    None
                } else {
                    Some("a number must be finite")
                }
            }
            Value::Array(items) => {
                if items.iter().all(Value::is_string) {
                    None
                } else {
                    Some("an array may hold strings only")
                }
            }
            Value::Null => Some("null is not allowed"),
            Value::Object(_) => Some("nested objects are not allowed"),
        };
        if let Some(reason) = refusal {
            let shown_key = mask_for_redaction_surface(RedactionSurface::GateProbe, key);
            return Err(RuntimeError::InvalidInput(format!(
                "properties key {shown_key:?} has an invalid value: {reason}; a value is a string, a finite number, a boolean or an array of strings"
            )));
        }
    }
    let size = serde_json::to_string(raw)
        .map_err(|e| {
            RuntimeError::InvalidInput(format!("properties could not be serialized: {e}"))
        })?
        .len();
    if size > MAX_PROPERTIES_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "properties serialize to {} bytes; the limit is {} bytes",
            size, MAX_PROPERTIES_BYTES
        )));
    }
    Ok(object.clone())
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

        let extra_properties = match p.properties.as_ref() {
            Some(raw) => Some(validate_properties(raw)?),
            None => None,
        };

        // Explicit namespace wins. Otherwise both memory types are written to the caller's
        // actor namespace. An anonymous caller (no actor configured, id `local`) has no actor
        // namespace of its own: with no namespace argument its write stays in the token's
        // namespace. Every anonymous write is flagged `attribution: "unresolved"` instead of
        // being refused, whether or not it names a namespace: the flag follows the caller.
        let unresolved_attribution = token.actor().is_anonymous();
        // Direct-call defense in depth mirrors dispatch's Rule-3 namespace escape.
        let write_token_owned: Option<NamespaceToken> = if let Some(ns_str) = p.namespace.as_deref()
        {
            let ns = Namespace::parse(ns_str).map_err(|e| {
                RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))
            })?;
            Some(token.with_namespace(ns))
        } else if unresolved_attribution {
            None
        } else {
            let actor_id = token.actor().id.as_str();
            let ns = Namespace::parse(actor_id).map_err(|e| {
                RuntimeError::InvalidInput(format!(
                    "actor id {actor_id:?} is not a valid namespace: {e}"
                ))
            })?;
            Some(token.with_namespace(ns))
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
        // Validated keys never collide with `memory_type`, `tags` or `attribution`.
        if let Some(extra) = extra_properties {
            for (key, value) in extra {
                props[key.as_str()] = value;
            }
        }
        if unresolved_attribution {
            props["attribution"] = json!("unresolved");
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
        if unresolved_attribution {
            response["attribution"] = json!("unresolved");
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
