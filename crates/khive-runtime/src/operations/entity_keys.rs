//! Caller keys on entities (ADR-221 section 1).
//!
//! A key names one live entity per `(namespace, kind, key)`. A keyed create
//! writes when the key is free, answers the holder when the payload is the
//! same, and refuses with `key_conflict` when it differs. Deleting an entity
//! releases its keys; restoring it takes them all back or refuses.

use std::collections::BTreeSet;

use super::entity_create::EntityCreateWrite;
use super::*;
use khive_storage::EntityKeyHolder;

/// The refusal text for a key over 512 UTF-8 bytes or holding U+0000.
pub(crate) const ENTITY_KEY_REFUSAL: &str =
    "key must be at most 512 UTF-8 bytes and must not contain U+0000";

/// Claims lost to a concurrent writer before the create gives up. Each lost
/// claim re-reads the holder, so a second loss needs the key to be released
/// and taken again between two reads.
const KEYED_CREATE_ATTEMPTS: usize = 3;

/// Outcome of [`KhiveRuntime::create_keyed_entity_with_post_commit_report`].
#[derive(Debug)]
pub enum KeyedEntityCreate {
    /// No live entity held the key: this call wrote `entity` and claimed the
    /// key for it in the same transaction.
    Created {
        entity: Entity,
        embedding: crate::retrieval::EmbeddingTruncationReport,
        degradations: Vec<PostCommitDegradation>,
    },
    /// The live holder of the key carries the same payload. Nothing was
    /// written.
    Replayed { entity: Entity },
    /// A live entity with a different payload holds the key. Nothing was
    /// written; the holder is unchanged.
    Conflict { existing_id: Uuid },
}

/// Validate a caller key and scan its exact bytes with the secret gate.
///
/// Runs before any holder lookup, so a refused key never reaches a read.
pub(crate) fn validate_entity_key(key: &str) -> RuntimeResult<()> {
    crate::note_validation::validate_key(key, ENTITY_KEY_REFUSAL)?;
    crate::secret_gate::check_at(key, "entity", "key")
}

/// The caller-visible part of a properties value: the runtime-owned
/// top-level keys removed, and `None` for an object left empty.
fn caller_properties(properties: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let value = properties?;
    let serde_json::Value::Object(map) = value else {
        return Some(value.clone());
    };
    let reserved = crate::secret_gate::RESERVED_PROPERTY_KEYS;
    let kept: serde_json::Map<String, serde_json::Value> = map
        .iter()
        .filter(|(key, _)| !reserved.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (!kept.is_empty()).then_some(serde_json::Value::Object(kept))
}

/// Whether `holder` carries the payload a keyed create asked for: name,
/// description, `entity_type`, typed-equal properties and tags as a set.
///
/// Both sides' properties are read through [`caller_properties`]: the
/// runtime-owned top-level keys are left out, because no caller can send
/// them, and absent properties and `{}` compare equal on either side, so a
/// holder created with `{}` and later stamped replays a request with `{}` or
/// with none.
fn same_entity_payload(
    holder: &Entity,
    entity_type: Option<&str>,
    name: &str,
    description: Option<&str>,
    properties: Option<&serde_json::Value>,
    tags: &[String],
) -> bool {
    if holder.name != name
        || holder.description.as_deref() != description
        || holder.entity_type.as_deref() != entity_type
    {
        return false;
    }
    if caller_properties(holder.properties.as_ref()) != caller_properties(properties) {
        return false;
    }
    let held: BTreeSet<&str> = holder.tags.iter().map(String::as_str).collect();
    let asked: BTreeSet<&str> = tags.iter().map(String::as_str).collect();
    held == asked
}

/// `key_conflict` for a keyed create whose key a different payload holds.
///
/// `existing_id` is named only when the caller may list the holder; the
/// caller decides that and passes `None` otherwise.
pub fn entity_key_conflict(key: &str, existing_id: Option<Uuid>) -> RuntimeError {
    let mut details = vec![
        ("reason", "key_conflict".to_string()),
        ("key", key.to_owned()),
    ];
    if let Some(existing_id) = existing_id {
        details.push(("existing_id", existing_id.to_string()));
    }
    KhiveError::conflict(format!(
        "key_conflict: key {key:?} is already held by a live entity with a different payload"
    ))
    .with_details(khive_types::Details::new_owned(details))
    .into()
}

/// `restore_key_conflict` naming every key another live entity took while
/// entity `id` was deleted. The entity stays deleted.
///
/// A holder's id is named, in the message and in `taken_keys`, only for a
/// kind `disclose` admits, as `key_conflict` names `existing_id` only where
/// the caller may list the holder. The kind and key are always named.
fn restore_entity_key_conflict(
    id: Uuid,
    taken: &[EntityKeyHolder],
    disclose: &(dyn Fn(&str) -> bool + Sync),
) -> RuntimeError {
    let named: Vec<String> = taken
        .iter()
        .map(|taken| {
            if disclose(&taken.kind) {
                format!("{}:{:?} held by {}", taken.kind, taken.key, taken.holder_id)
            } else {
                format!("{}:{:?}", taken.kind, taken.key)
            }
        })
        .collect();
    let listed: Vec<serde_json::Value> = taken
        .iter()
        .map(|taken| {
            let mut entry = serde_json::json!({"kind": taken.kind, "key": taken.key});
            if disclose(&taken.kind) {
                entry["existing_id"] = serde_json::json!(taken.holder_id.to_string());
            }
            entry
        })
        .collect();
    KhiveError::conflict(format!(
        "restore_key_conflict: entity {id} stays deleted; keys taken by live entities: {}",
        named.join(", ")
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "restore_key_conflict".to_string()),
        ("taken_keys", serde_json::Value::Array(listed).to_string()),
    ]))
    .into()
}

impl KhiveRuntime {
    /// Create an entity under caller key `key`, unique per namespace and
    /// base kind (ADR-221 section 1).
    ///
    /// The key is validated and secret-gated before any read. A free key is
    /// claimed in the transaction that writes the entity. A held key answers
    /// [`KeyedEntityCreate::Replayed`] when the holder carries the same
    /// payload and [`KeyedEntityCreate::Conflict`] when it does not; neither
    /// writes anything.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_keyed_entity_with_post_commit_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        key: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<KeyedEntityCreate> {
        validate_entity_key(key)?;
        self.validate_entity_kind(kind)?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        let namespace = token.namespace().as_str();
        for _ in 0..KEYED_CREATE_ATTEMPTS {
            let holder = self
                .entities(token)?
                .get_entity_by_key(namespace, kind, key)
                .await?;
            if let Some(holder) = holder {
                return Ok(
                    if same_entity_payload(
                        &holder,
                        entity_type,
                        name,
                        description,
                        properties.as_ref(),
                        &tags,
                    ) {
                        KeyedEntityCreate::Replayed { entity: holder }
                    } else {
                        KeyedEntityCreate::Conflict {
                            existing_id: holder.id,
                        }
                    },
                );
            }
            match self
                .create_entity_with_embedding_report_inner(
                    token,
                    kind,
                    entity_type,
                    name,
                    description,
                    properties.clone(),
                    tags.clone(),
                    Vec::new(),
                    Some(key),
                )
                .await?
            {
                EntityCreateWrite::Written(entity, embedding, degradations) => {
                    return Ok(KeyedEntityCreate::Created {
                        entity: *entity,
                        embedding,
                        degradations,
                    });
                }
                EntityCreateWrite::KeyHeld => continue,
            }
        }
        // Every claim lost to a concurrent writer: answer from the winner.
        let holder = self
            .entities(token)?
            .get_entity_by_key(namespace, kind, key)
            .await?;
        match holder {
            Some(holder)
                if same_entity_payload(
                    &holder,
                    entity_type,
                    name,
                    description,
                    properties.as_ref(),
                    &tags,
                ) =>
            {
                Ok(KeyedEntityCreate::Replayed { entity: holder })
            }
            Some(holder) => Ok(KeyedEntityCreate::Conflict {
                existing_id: holder.id,
            }),
            None => Err(RuntimeError::Internal(format!(
                "keyed entity create: key {key:?} was claimed and released again during \
                 {KEYED_CREATE_ATTEMPTS} claims"
            ))),
        }
    }

    /// The live entity holding `key` for `kind` in the caller's primary
    /// namespace, or `None` when no live entity holds it.
    pub async fn get_entity_by_key(
        &self,
        token: &NamespaceToken,
        kind: &str,
        key: &str,
    ) -> RuntimeResult<Option<Entity>> {
        validate_entity_key(key)?;
        Ok(self
            .entities(token)?
            .get_entity_by_key(token.namespace().as_str(), kind, key)
            .await?)
    }

    /// Refuse a restore of entity `id` while another live entity holds any
    /// key `id` released. `disclose` says, per kind, whether the refusal may
    /// name the holder's id.
    pub(super) async fn refuse_restore_over_taken_keys(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        disclose: &(dyn Fn(&str) -> bool + Sync),
    ) -> RuntimeResult<()> {
        let taken = self.entities(token)?.taken_entity_keys(id).await?;
        if taken.is_empty() {
            Ok(())
        } else {
            Err(restore_entity_key_conflict(id, &taken, disclose))
        }
    }
}

#[cfg(test)]
#[path = "entity_keys_tests.rs"]
mod tests;
