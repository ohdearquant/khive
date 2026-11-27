use crate::atomic_plan::{AffectedRowGuard, PlanStatement, PostCommitEffect, UpdatePlan};
use crate::atomic_runner::{AtomicOpFailure, AtomicOpPlan};
use crate::runtime::EntityOwnerHookLookup;
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};
use khive_changeset::{UpdateOp, UpdatePatch};
use khive_storage::entity::EntityAlias;
use khive_storage::{Entity, SqlStatement, SqlValue};

#[derive(Clone, Debug)]
pub(super) struct PreparedEntityUpdate {
    snapshot: Entity,
    patch: crate::curation::EntityPatch,
    /// The entity's whole alias set after the update, every origin, sorted;
    /// `None` when the change leaves the aliases alone (ADR-221 section 2).
    aliases: Option<Vec<EntityAlias>>,
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
    let aliases = patch.aliases.clone().map(sorted);
    if let Some(set) = &aliases {
        for held in set {
            crate::validate_entity_alias(&held.alias)?;
        }
    }
    let name = patch.name.as_deref().unwrap_or(&current.name);
    check_alias_names(&current, name, aliases.as_deref()).map_err(RuntimeError::InvalidInput)?;
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
        aliases,
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
        if let Some(set) = &self.aliases {
            entity.aliases.clone_from(set);
        }
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

/// A plan that writes nothing and holds only while `row` is still the live row
/// it was read as: same id, `updated_at`, `deleted_at` and version.
pub(crate) fn unchanged_entity_assertion_plan(row: &Entity, label: &str) -> AtomicOpPlan {
    let assertion = SqlStatement {
        sql: include_str!("../../sql/canonical_entity_noop_assertion.sql").into(),
        params: vec![
            SqlValue::Text(row.id.to_string()),
            SqlValue::Integer(row.updated_at),
            row.deleted_at
                .map(SqlValue::Integer)
                .unwrap_or(SqlValue::Null),
            SqlValue::Integer(row.version),
        ],
        label: Some(label.into()),
    };
    AtomicOpPlan::Update(Box::new(UpdatePlan {
        target_id: row.id,
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
    }))
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
    let before = live.clone();
    let (patched, reindex, mut changed_fields) = apply_canonical_patch(&before, &prepared.patch)
        .map_err(|error| failure(error.to_string()))?;
    live = patched;
    if let Some(set) = &prepared.aliases {
        live.aliases.clone_from(set);
    }
    // Checked again on the writer's row: the name or the aliases may have
    // moved since preparation where the preimage does not capture them.
    check_alias_names(&before, &live.name, prepared.aliases.as_deref()).map_err(failure)?;
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
        return Ok(unchanged_entity_assertion_plan(
            &before,
            "canonical-entity-noop-assertion",
        ));
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
    // ADR-221 section 2: a change that carries the alias set writes exactly
    // that set with the origins it names; the change set is a trusted
    // carrier for origin, as an archive is.
    let aliases_changed = !same_aliases(&live.aliases, &before.aliases);
    if aliases_changed {
        statements.push(PlanStatement {
            statement: khive_db::stores::entity::entity_aliases_delete_all_statement(
                &live.namespace,
                id,
            ),
            guard: None,
        });
        statements.extend(live.aliases.iter().map(|held| PlanStatement {
            statement: khive_db::stores::entity::entity_alias_insert_statement(
                &live.namespace,
                id,
                &held.alias,
                held.origin,
                live.updated_at,
            ),
            guard: None,
        }));
        changed_fields.push("aliases");
    }
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
        // Aliases are part of the entity's text and vector bodies.
        post_commit: if reindex || aliases_changed {
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
        && same_aliases(&patched.aliases, &row.aliases)
}

fn sorted(mut aliases: Vec<EntityAlias>) -> Vec<EntityAlias> {
    aliases.sort();
    aliases
}

fn same_aliases(a: &[EntityAlias], b: &[EntityAlias]) -> bool {
    sorted(a.to_vec()) == sorted(b.to_vec())
}

/// An alias never equals its entity's name (ADR-221 section 2). With the
/// alias set in the change, no alias of it may equal `name`, the name after
/// the update. Without it the apply writes no alias row, so a rename onto an
/// alias `row` holds is refused: deleting that row would be a change the
/// preimage does not record, and the inverse could not put it back.
fn check_alias_names(
    row: &Entity,
    name: &str,
    aliases: Option<&[EntityAlias]>,
) -> Result<(), String> {
    match aliases {
        Some(set) if set.iter().any(|held| held.alias == name) => Err(format!(
            "alias {name:?} of entity {} equals the entity's name",
            row.id
        )),
        None if name != row.name && row.aliases.iter().any(|held| held.alias == name) => {
            Err(format!(
                "renaming entity {} onto its alias {name:?} needs the alias set in the change",
                row.id
            ))
        }
        _ => Ok(()),
    }
}

fn failure(message: String) -> AtomicOpFailure {
    AtomicOpFailure::SqlError {
        statement_label: Some("canonical-entity-update".into()),
        message,
    }
}
