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

/// Outcome of [`KhiveRuntime::reload_keyed_entity`].
#[derive(Debug)]
pub enum KeyedEntityReload {
    /// The holder now carries the request's payload.
    Updated {
        entity: Entity,
        report: crate::curation::EntityUpdateReport,
    },
    /// The holder already carried the request's payload. Nothing was written.
    Replayed { entity: Entity },
    /// The key no longer names the entity the caller was cleared to update:
    /// it was released, or another entity holds it now. Nothing was written.
    Moved,
}

/// A fresh id for the row a keyed bulk create will write (ADR-221 section 1).
///
/// The bulk create names the id before it builds the row, so the update gate
/// can judge a later item of the same request against that row. Only
/// [`PendingEntityId::mint`] makes one and building the plan spends it, so one
/// id names one new row.
#[derive(Debug)]
pub struct PendingEntityId(pub(super) Uuid);

impl PendingEntityId {
    /// A new random id.
    pub fn mint() -> Self {
        Self(Uuid::new_v4())
    }

    /// The id the row will carry.
    pub fn id(&self) -> Uuid {
        self.0
    }
}

/// A plan that writes nothing and holds only while `holder` is still the
/// live row it was read as: same id, `updated_at`, `deleted_at` and version.
///
/// A keyed bulk create that answers an item as an identical replay of a
/// stored holder joins this to its unit (ADR-221 section 1), so the replay
/// is judged inside the unit's transaction: a holder that changed after it
/// was read rolls the unit back and the request is judged again.
pub fn keyed_replay_assertion_plan(holder: &Entity) -> AtomicOpPlan {
    crate::canonical_apply::unchanged_entity_assertion_plan(holder, "keyed-replay-assertion")
}

/// Whether `holder` carries the payload `spec` asks for, as a keyed create
/// compares them.
pub fn entity_carries_create_payload(holder: &Entity, spec: &EntityCreateSpec) -> bool {
    same_entity_payload(
        holder,
        spec.entity_type.as_deref(),
        &spec.name,
        spec.description.as_deref(),
        spec.properties.as_ref(),
        &spec.tags,
        &spec_aliases(spec),
    )
}

/// The caller aliases a row written from `spec` holds: the requested ones
/// other than its name.
fn spec_aliases(spec: &EntityCreateSpec) -> BTreeSet<&str> {
    spec.aliases
        .iter()
        .map(String::as_str)
        .filter(|alias| *alias != spec.name)
        .collect()
}

/// Whether two keyed create payloads are the same, as
/// [`entity_carries_create_payload`] compares a holder with a request.
pub fn same_create_payload(first: &EntityCreateSpec, second: &EntityCreateSpec) -> bool {
    let mut held =
        Entity::new("", &first.kind, &first.name).with_entity_type(first.entity_type.as_deref());
    held.description = first.description.clone();
    held.properties = first.properties.clone();
    held.tags = first.tags.clone();
    held.aliases = spec_aliases(first)
        .into_iter()
        .map(|alias| khive_storage::entity::EntityAlias {
            alias: alias.to_owned(),
            origin: khive_storage::entity::EntityAliasOrigin::Caller,
        })
        .collect();
    entity_carries_create_payload(&held, second)
}

/// The caller property keys `holder` has and `wanted` leaves out: what a
/// reload removes so the holder's caller properties become `wanted`.
fn properties_to_remove(holder: &Entity, wanted: Option<&serde_json::Value>) -> Vec<String> {
    match caller_properties(holder.properties.as_ref()) {
        Some(serde_json::Value::Object(held)) => held
            .keys()
            .filter(|held_key| {
                !wanted
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|asked| asked.contains_key(*held_key))
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// The patch a reload writes: every caller field becomes the request's.
fn reload_patch(
    entity_type: Option<&str>,
    name: &str,
    description: Option<&str>,
    wanted: Option<serde_json::Value>,
    tags: &[String],
) -> crate::EntityPatch {
    crate::EntityPatch {
        name: Some(name.to_owned()),
        description: Some(description.map(str::to_owned)),
        properties: wanted,
        tags: Some(tags.to_vec()),
        entity_type: Some(entity_type.map(str::to_owned)),
    }
}

fn is_version_conflict(error: &RuntimeError) -> bool {
    matches!(
        error,
        RuntimeError::Khive(error)
            if error.details().and_then(|details| details.get("reason"))
                == Some("version_conflict")
    )
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
/// description, `entity_type`, typed-equal properties, tags as a set and
/// caller-origin aliases as a set (ADR-221 section 1). `aliases` is the
/// set the request would store, its name already left out.
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
    aliases: &BTreeSet<&str>,
) -> bool {
    if super::entity_aliases::caller_alias_set(holder) != *aliases {
        return false;
    }
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
    /// writes anything. `aliases` is part of the payload, compared as the set
    /// of the holder's caller-origin aliases.
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
        aliases: &EntityAliasChange,
    ) -> RuntimeResult<KeyedEntityCreate> {
        validate_entity_key(key)?;
        self.validate_entity_kind(kind)?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        let namespace = token.namespace().as_str();
        let wanted_aliases = aliases.wanted(name);
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
                        &wanted_aliases,
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
                    aliases,
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
                    &wanted_aliases,
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

    /// A keyed create under `on_key: "update"` whose key `holder_id` holds
    /// with a different payload (ADR-221 section 1).
    ///
    /// The holder's name, description, `entity_type`, tags and caller
    /// properties become the request's: a field the request leaves out is
    /// cleared, as the payload comparison reads it. The write is the
    /// ordinary entity update, bound to the version this call compared, so a
    /// holder that changed in between is read and compared again, and a
    /// holder carrying a runtime-owned property key is refused as that
    /// update refuses it: the key vouches for bytes this write would change.
    /// The caller has already been cleared to update `holder_id`; if the key
    /// no longer names it, nothing is written and the answer is
    /// [`KeyedEntityReload::Moved`]. The holder's caller-origin aliases
    /// become `aliases`; aliases a rename or a merge wrote stay.
    #[allow(clippy::too_many_arguments)]
    pub async fn reload_keyed_entity(
        &self,
        token: &NamespaceToken,
        kind: &str,
        key: &str,
        holder_id: Uuid,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        aliases: &EntityAliasChange,
    ) -> RuntimeResult<KeyedEntityReload> {
        validate_entity_key(key)?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        let namespace = token.namespace().as_str();
        let wanted = caller_properties(properties.as_ref());
        let wanted_aliases = aliases.wanted(name);
        for _ in 0..KEYED_CREATE_ATTEMPTS {
            let holder = self
                .entities(token)?
                .get_entity_by_key(namespace, kind, key)
                .await?;
            let Some(holder) = holder.filter(|holder| holder.id == holder_id) else {
                return Ok(KeyedEntityReload::Moved);
            };
            if same_entity_payload(
                &holder,
                entity_type,
                name,
                description,
                wanted.as_ref(),
                &tags,
                &wanted_aliases,
            ) {
                return Ok(KeyedEntityReload::Replayed { entity: holder });
            }
            let removed = properties_to_remove(&holder, wanted.as_ref());
            let removed: Vec<&str> = removed.iter().map(String::as_str).collect();
            let patch = reload_patch(entity_type, name, description, wanted.clone(), &tags);
            match self
                .update_entity_with_removals_and_index_report(
                    token,
                    holder.id,
                    patch,
                    &removed,
                    Some(holder.version),
                    aliases,
                )
                .await
            {
                Ok((entity, report)) => {
                    return Ok(KeyedEntityReload::Updated { entity, report });
                }
                Err(error) if is_version_conflict(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(RuntimeError::Internal(format!(
            "keyed entity reload: the holder of key {key:?} changed during \
             {KEYED_CREATE_ATTEMPTS} attempts"
        )))
    }

    /// The update plan for a bulk item under `on_key: "update"` whose key
    /// `holder` holds with a different payload (ADR-221 section 1).
    ///
    /// The holder's caller fields become `spec`'s, as
    /// [`Self::reload_keyed_entity`] writes them, and the plan is bound to the
    /// version `holder` carries, so a holder that changed after the caller
    /// read it rolls the unit back. A holder carrying a runtime-owned
    /// property key is refused as the ordinary update refuses it. The
    /// returned record is the projection; its version is the one read.
    pub async fn prepare_keyed_entity_reload_plan(
        &self,
        token: &NamespaceToken,
        holder: &Entity,
        spec: &EntityCreateSpec,
    ) -> RuntimeResult<(Entity, AtomicOpPlan)> {
        crate::secret_gate::reject_reserved_secret_gate_property(spec.properties.as_ref())?;
        let aliases = EntityAliasChange::replace(&spec.aliases)?;
        let wanted = caller_properties(spec.properties.as_ref());
        let removed = properties_to_remove(holder, wanted.as_ref());
        let removed: Vec<&str> = removed.iter().map(String::as_str).collect();
        let patch = reload_patch(
            spec.entity_type.as_deref(),
            &spec.name,
            spec.description.as_deref(),
            wanted,
            &spec.tags,
        );
        crate::atomic_prepare::prepare_keyed_reload_entity_record(
            self,
            token,
            holder.id,
            patch,
            &removed,
            holder.version,
            &aliases,
        )
        .await
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
