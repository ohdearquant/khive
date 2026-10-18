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

/// Apply a canonical patch to `before` under ADR-115 §4. An incoming reserved value that
/// echoes the stored one is dropped and any other incoming reserved value is refused. A
/// stored reserved value is carried only while the bytes it vouches for (name, description,
/// properties outside the reserved keys, tags) are unchanged. Canonical writes take no
/// manifest exemption, so a changed row loses the stamp and must pass the strict scans as a
/// whole.
fn apply_canonical_patch(
    before: &Entity,
    patch: &crate::curation::EntityPatch,
) -> RuntimeResult<(Entity, bool, Vec<&'static str>)> {
    let mut patch = patch.clone();
    if let Some(properties) = patch.properties.as_mut() {
        crate::secret_gate::strip_reserved_echo(properties, before.properties.as_ref())?;
    }
    let mut entity = before.clone();
    let (reindex, mut changed_fields) =
        KhiveRuntime::apply_entity_patch_fields(&mut entity, patch, &[], true);
    let stored = crate::secret_gate::reserved_properties(before.properties.as_ref());
    if !stored.is_empty() {
        let unchanged = entity.name == before.name
            && entity.description == before.description
            && entity.tags == before.tags
            && crate::secret_gate::without_reserved_properties(entity.properties.as_ref())
                == crate::secret_gate::without_reserved_properties(before.properties.as_ref());
        let properties = entity
            .properties
            .get_or_insert_with(|| serde_json::Value::Object(Default::default()))
            .as_object_mut()
            .ok_or_else(|| {
                RuntimeError::InvalidInput("canonical entity properties must be an object".into())
            })?;
        if unchanged {
            properties.extend(
                stored
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value)),
            );
        } else {
            for key in crate::secret_gate::RESERVED_PROPERTY_KEYS {
                properties.remove(key);
            }
            crate::secret_gate::check_at(&entity.name, "entity", "name")?;
            if let Some(description) = &entity.description {
                crate::secret_gate::check_at(description, "entity", "description")?;
            }
            if let Some(properties) = &entity.properties {
                crate::secret_gate::check_json_at(properties, "entity", "properties")?;
            }
            crate::secret_gate::check_tags_at(&entity.tags, "entity", "tags")?;
        }
        if entity.properties != before.properties && !changed_fields.contains(&"properties") {
            changed_fields.push("properties");
        }
    }
    Ok((entity, reindex, changed_fields))
}

impl PreparedEntityUpdate {
    pub(super) fn projected(&self, now: i64) -> RuntimeResult<Entity> {
        let (mut entity, _, _) = apply_canonical_patch(&self.snapshot, &self.patch)?;
        if same_patchable_values(&entity, &self.snapshot) {
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
    let before = live.clone();
    let (patched, reindex, changed_fields) = apply_canonical_patch(&before, &prepared.patch)
        .map_err(|error| failure(error.to_string()))?;
    live = patched;
    crate::secret_gate::check_reserved_carry_forward(
        before.properties.as_ref(),
        live.properties.as_ref(),
        live.name == before.name
            && live.description == before.description
            && live.tags == before.tags
            && crate::secret_gate::without_reserved_properties(live.properties.as_ref())
                == crate::secret_gate::without_reserved_properties(before.properties.as_ref()),
    )
    .map_err(|error| failure(error.to_string()))?;
    if same_patchable_values(&live, &before) {
        // A canonical update whose values already hold lands as a no-op: no write,
        // no event, no new revision. Its preimage still matches on replay, so this is
        // what keeps a landed change-set from writing a second time.
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

/// Whether a patch left every field it can write equal to the row it was applied to.
/// `apply_entity_patch_fields` reports the fields a patch names, not the ones whose value
/// changed, so a patch that restates current values must be detected by comparing values.
fn same_patchable_values(patched: &Entity, row: &Entity) -> bool {
    patched.name == row.name
        && patched.description == row.description
        && patched.properties == row.properties
        && patched.tags == row.tags
        && patched.entity_type == row.entity_type
}

fn failure(message: String) -> AtomicOpFailure {
    AtomicOpFailure::SqlError {
        statement_label: Some("canonical-entity-update".into()),
        message,
    }
}
