use crate::atomic_plan::{AffectedRowGuard, PlanStatement, PostCommitEffect, UpdatePlan};
use crate::atomic_runner::{AtomicOpFailure, AtomicOpPlan};
use crate::runtime::EntityOwnerHookLookup;
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};
use khive_changeset::{UpdateOp, UpdatePatch};
use khive_storage::{Entity, SqlStatement, SqlValue};

#[derive(Clone, Debug)]
pub(super) struct PreparedEntityUpdate {
    snapshot: Entity,
    patch: crate::curation::EntityPatch,
    owner_lookup: EntityOwnerHookLookup,
    prepared_with_owner: bool,
}

pub(super) async fn prepare(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    current: Entity,
    op: &UpdateOp,
) -> RuntimeResult<PreparedEntityUpdate> {
    let UpdatePatch::Entity(patch) = op.patch() else {
        return Err(RuntimeError::InvalidInput(
            "expected canonical entity update".into(),
        ));
    };
    let patch = crate::curation::EntityPatch {
        name: patch.name.clone(),
        description: patch.description.clone(),
        properties: patch
            .properties
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| RuntimeError::InvalidInput("invalid canonical properties".into()))?,
        tags: patch.tags.clone(),
        entity_type: patch.entity_type.clone(),
    };
    let owner_lookup = runtime.canonical_entity_owner_lookup();
    let prepared_with_owner = owner_lookup.has_hook(&current.kind);
    let patch = runtime
        .validate_canonical_entity_patch(token, &current, patch)
        .await?;
    Ok(PreparedEntityUpdate {
        snapshot: current,
        patch,
        owner_lookup,
        prepared_with_owner,
    })
}

impl PreparedEntityUpdate {
    pub(super) fn projected(&self, now: i64) -> RuntimeResult<Entity> {
        let mut entity = self.snapshot.clone();
        let (_, changed_fields) =
            KhiveRuntime::apply_entity_patch_fields(&mut entity, self.patch.clone(), &[], true);
        if changed_fields.is_empty() {
            // An unchanged patch lands as a no-op (see `materialize`), so later
            // operations in the unit see the same revision.
            return Ok(entity);
        }
        entity.updated_at = now.max(
            entity
                .updated_at
                .checked_add(1)
                .ok_or_else(|| RuntimeError::InvalidInput("entity revision exhausted".into()))?,
        );
        entity.version = entity
            .version
            .checked_add(1)
            .ok_or_else(|| RuntimeError::InvalidInput("entity version exhausted".into()))?;
        Ok(entity)
    }
}

pub(super) fn materialize(
    token: &NamespaceToken,
    mut live: Entity,
    prepared: &PreparedEntityUpdate,
    now: i64,
) -> Result<AtomicOpPlan, AtomicOpFailure> {
    let id = live.id;
    let has_owner = prepared.owner_lookup.has_hook(&live.kind);
    if has_owner != prepared.prepared_with_owner
        || has_owner
            && (live.updated_at != prepared.snapshot.updated_at
                || live.deleted_at != prepared.snapshot.deleted_at
                || live.version != prepared.snapshot.version)
    {
        return Err(AtomicOpFailure::OwnerSnapshotConflict {
            target_id: id,
            kind: live.kind,
        });
    }
    let previous_updated_at = live.updated_at;
    let previous_deleted_at = live.deleted_at;
    let previous_version = live.version;
    let (reindex, changed_fields) =
        KhiveRuntime::apply_entity_patch_fields(&mut live, prepared.patch.clone(), &[], true);
    crate::secret_gate::reject_reserved_secret_gate_property(live.properties.as_ref())
        .map_err(|error| failure(error.to_string()))?;
    if changed_fields.is_empty() {
        // Live `update` leaves an unchanged entity untouched: no write, no event,
        // no new revision. A canonical update whose values already hold lands the
        // same way, so replaying it changes nothing.
        let assertion = SqlStatement {
            sql: include_str!("../../sql/canonical_entity_noop_assertion.sql").into(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Integer(previous_updated_at),
                previous_deleted_at
                    .map(SqlValue::Integer)
                    .unwrap_or(SqlValue::Null),
                SqlValue::Integer(previous_version),
            ],
            label: Some("canonical-entity-noop-assertion".into()),
        };
        return Ok(AtomicOpPlan::Update(Box::new(UpdatePlan {
            target_id: id,
            statements: vec![PlanStatement {
                statement: assertion,
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
        })));
    }
    live.updated_at = now.max(
        previous_updated_at
            .checked_add(1)
            .ok_or_else(|| failure("entity revision exhausted".into()))?,
    );
    let mut statements = vec![PlanStatement {
        statement: khive_db::stores::entity::entity_replace_if_unchanged_statement(
            &live,
            previous_updated_at,
            previous_deleted_at,
        ),
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
    statements.extend(
        crate::atomic_prepare::event_append_statements(
            token,
            &live.namespace,
            "update",
            khive_types::EventKind::EntityUpdated,
            khive_types::SubstrateKind::Entity,
            id,
            serde_json::json!({"id":id,"namespace":live.namespace,"changed_fields":changed_fields}),
        )
        .map_err(|error| failure(error.to_string()))?,
    );
    Ok(AtomicOpPlan::Update(Box::new(UpdatePlan {
        target_id: id,
        statements,
        post_commit: if reindex {
            PostCommitEffect::ReindexEntity { entity_id: id }
        } else {
            PostCommitEffect::None
        },
        edge_natural_key: None,
        idempotent_noop: false,
        entity_guard: None,
        note_guard: None,
        note_vector_purge: None,
        note_embedding_inheritance: None,
        graph_effects: Vec::new(),
    })))
}

fn failure(message: String) -> AtomicOpFailure {
    AtomicOpFailure::SqlError {
        statement_label: Some("canonical-entity-update".into()),
        message,
    }
}
