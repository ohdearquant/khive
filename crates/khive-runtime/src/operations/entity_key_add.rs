//! Adding a caller key to a live entity (ADR-221 Amendment 1).
//!
//! A key added here is held exactly as one a keyed create claimed: unique per
//! `(namespace, kind, key)` over held rows, under the entity's own base kind,
//! released by a delete, taken back by a restore and moved by a merge. A
//! pack that writes names as keys on entities it already holds joins
//! [`entity_key_add_plans`] to its own atomic unit; [`KhiveRuntime::add_entity_key`]
//! commits one add by itself.

use khive_db::stores::entity::entity_key_claim_statement;

use super::*;

/// Label of the statement that holds an add to an entity that is live, not
/// merged away and of the kind the key is added under.
pub const ENTITY_KEY_ADD_LIVE_LABEL: &str = "entity-key-add-live";

/// Label of the statement that holds an add only while the entity holds the
/// key once the claim has run. It fails when another live entity holds it.
pub const ENTITY_KEY_ADD_HELD_LABEL: &str = "entity-key-add-held";

/// Units lost to a concurrent writer before an add gives up. Each lost unit
/// reads the entity and the key's holder again.
const ENTITY_KEY_ADD_ATTEMPTS: usize = 3;

/// Outcome of [`KhiveRuntime::add_entity_key`].
#[derive(Debug)]
pub enum EntityKeyAdd {
    /// The key was free: this call claimed it for `entity`.
    Added { entity: Entity },
    /// `entity` already held the key. Nothing was written.
    Held { entity: Entity },
    /// Another live entity holds the key. Nothing was written.
    Conflict { existing_id: Uuid },
}

fn assertion_plan(target_id: Uuid, statement: SqlStatement) -> AtomicOpPlan {
    AtomicOpPlan::Update(Box::new(UpdatePlan {
        target_id,
        statements: vec![PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        post_commit: PostCommitEffect::None,
        edge_natural_key: None,
        idempotent_noop: true,
        entity_guard: None,
        note_guard: None,
        note_vector_purge: None,
        note_embedding_inheritance: None,
        graph_effects: Vec::new(),
    }))
}

/// The plans that add caller key `key` to entity `entity_id`, of base kind
/// `kind` in `namespace`, claimed at `at` (ADR-221 Amendment 1).
///
/// The key is validated and secret-gated before anything else, as a keyed
/// create's is. Joined to an atomic unit in the order returned, the plans
/// hold only while the entity is live, not merged away and of kind `kind`
/// ([`ENTITY_KEY_ADD_LIVE_LABEL`]); they claim the key when no live entity
/// holds it under `kind`; and they hold only while the entity then holds it
/// ([`ENTITY_KEY_ADD_HELD_LABEL`]). A key the entity already holds passes
/// and writes nothing, so adding it twice leaves one row. A key another live
/// entity holds fails the last guard and rolls the whole unit back.
pub fn entity_key_add_plans(
    namespace: &str,
    entity_id: Uuid,
    kind: &str,
    key: &str,
    at: i64,
) -> RuntimeResult<Vec<AtomicOpPlan>> {
    validate_entity_key(key)?;
    let live = SqlStatement {
        sql: "SELECT 1 FROM entities WHERE id = ?1 AND namespace = ?2 AND kind = ?3 \
              AND deleted_at IS NULL AND merged_into IS NULL"
            .into(),
        params: vec![
            SqlValue::Text(entity_id.to_string()),
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(kind.to_owned()),
        ],
        label: Some(ENTITY_KEY_ADD_LIVE_LABEL.into()),
    };
    let claim = AtomicOpPlan::Update(Box::new(UpdatePlan {
        target_id: entity_id,
        statements: vec![PlanStatement {
            statement: entity_key_claim_statement(namespace, kind, key, entity_id, at),
            guard: None,
        }],
        post_commit: PostCommitEffect::None,
        edge_natural_key: None,
        idempotent_noop: false,
        entity_guard: None,
        note_guard: None,
        note_vector_purge: None,
        note_embedding_inheritance: None,
        graph_effects: Vec::new(),
    }));
    let held = SqlStatement {
        sql: "SELECT 1 FROM entity_keys WHERE namespace = ?1 AND kind = ?2 AND key = ?3 \
              AND entity_id = ?4 AND released_at IS NULL"
            .into(),
        params: vec![
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(kind.to_owned()),
            SqlValue::Text(key.to_owned()),
            SqlValue::Text(entity_id.to_string()),
        ],
        label: Some(ENTITY_KEY_ADD_HELD_LABEL.into()),
    };
    Ok(vec![
        assertion_plan(entity_id, live),
        claim,
        assertion_plan(entity_id, held),
    ])
}

/// `entity_deleted` for an add to an entity that a delete removed.
fn entity_deleted(id: Uuid) -> RuntimeError {
    KhiveError::conflict(format!(
        "entity_deleted: entity {id} is deleted, so no key can be added to it"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "entity_deleted".to_string()),
        ("id", id.to_string()),
    ]))
    .into()
}

/// `entity_merged` for an add to an entity merged into `kept_id`: its keys
/// now live on the kept entity.
fn entity_merged(id: Uuid, kept_id: Uuid) -> RuntimeError {
    KhiveError::conflict(format!(
        "entity_merged: entity {id} was merged into {kept_id}, so no key can be added to it"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "entity_merged".to_string()),
        ("id", id.to_string()),
        ("merged_into", kept_id.to_string()),
    ]))
    .into()
}

impl KhiveRuntime {
    /// Add caller key `key` to live entity `id` in the caller's primary
    /// namespace, under the entity's own base kind (ADR-221 Amendment 1).
    ///
    /// The key is validated and secret-gated before any read. A free key is
    /// claimed in one transaction that also holds the entity live and of the
    /// kind it was read with. A key the entity holds answers
    /// [`EntityKeyAdd::Held`] and one another live entity holds answers
    /// [`EntityKeyAdd::Conflict`]; neither writes anything. A missing entity
    /// is `NotFound`; a deleted one is refused as `entity_deleted` and a
    /// merged-away one as `entity_merged`, naming the kept entity.
    pub async fn add_entity_key(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        key: &str,
    ) -> RuntimeResult<EntityKeyAdd> {
        validate_entity_key(key)?;
        let namespace = token.namespace().as_str();
        for _ in 0..ENTITY_KEY_ADD_ATTEMPTS {
            let entity = self
                .entities(token)?
                .get_entity_including_deleted(id)
                .await?
                .filter(|entity| entity.namespace == namespace)
                .ok_or_else(|| RuntimeError::NotFound(format!("entity {id}")))?;
            if let Some(kept_id) = entity.merged_into {
                return Err(entity_merged(id, kept_id));
            }
            if entity.deleted_at.is_some() {
                return Err(entity_deleted(id));
            }
            match self
                .entities(token)?
                .get_entity_by_key(namespace, &entity.kind, key)
                .await?
            {
                Some(holder) if holder.id == id => return Ok(EntityKeyAdd::Held { entity }),
                Some(holder) => {
                    return Ok(EntityKeyAdd::Conflict {
                        existing_id: holder.id,
                    })
                }
                None => {}
            }
            let at = Utc::now().timestamp_micros();
            let plans = entity_key_add_plans(namespace, id, &entity.kind, key, at)?;
            match run_atomic_unit(self.sql().as_ref(), plans).await {
                Ok(AtomicRunOutcome::Committed { .. }) => {
                    return Ok(EntityKeyAdd::Added { entity });
                }
                // The entity or the key's holder changed after they were
                // read: read both again.
                Ok(AtomicRunOutcome::RolledBack {
                    failure: AtomicOpFailure::GuardFailed { .. },
                    ..
                }) => continue,
                Ok(AtomicRunOutcome::RolledBack { failure, .. }) => {
                    return Err(RuntimeError::Internal(format!(
                        "entity key add rolled back: {failure:?}"
                    )));
                }
                Err(error) => return Err(RuntimeError::Storage(error.0)),
            }
        }
        Err(RuntimeError::Internal(format!(
            "entity key add: entity {id} or the holder of key {key:?} changed during \
             {ENTITY_KEY_ADD_ATTEMPTS} attempts"
        )))
    }
}

#[cfg(test)]
#[path = "entity_key_add_tests.rs"]
mod tests;
