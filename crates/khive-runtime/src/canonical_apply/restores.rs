use khive_changeset::{CanonicalRecordRef, DeleteOp};
use khive_db::stores::entity::{
    entity_alias_insert_statement, entity_aliases_delete_all_statement,
};
use khive_storage::entity::EntityKeyHolder;
use khive_storage::{Entity, SqlReader, SqlStatement, SqlValue, SqlWriter, StorageError};
use khive_types::{Details, KhiveError};
use uuid::Uuid;

use super::{entity_record, failure, mismatch, storage};
use crate::atomic_plan::{PlanStatement, PostCommitEffect};
use crate::atomic_runner::{AtomicOpFailure, AtomicOpPlan, EntityRestoreKeyConflict};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

struct ReleasedKeys {
    keys: Vec<String>,
    taken: Vec<EntityKeyHolder>,
}

async fn read_released_keys(
    reader: &mut (impl SqlReader + ?Sized),
    row: &Entity,
) -> Result<Option<ReleasedKeys>, StorageError> {
    // The restore trigger matches every key of this ID, not just keys with
    // the captured kind and namespace. Read and verify the complete set.
    let rows = reader
        .query_all(
            SqlStatement::new(
                "SELECT mine.namespace, mine.kind, mine.key, mine.released_at, \
                        held.entity_id AS holder_id \
                 FROM entity_keys AS mine \
                 LEFT JOIN entity_keys AS held \
                   ON held.namespace = mine.namespace AND held.kind = mine.kind \
                  AND held.key = mine.key AND held.released_at IS NULL \
                  AND held.entity_id <> mine.entity_id \
                 WHERE mine.entity_id = ?1 ORDER BY mine.kind, mine.key",
                vec![SqlValue::Text(row.id.to_string())],
            )
            .labelled("canonical-restore-keys"),
        )
        .await?;
    let column_error =
        |error: khive_storage::types::SqlColumnError| StorageError::Internal(error.to_string());
    let mut keys = Vec::with_capacity(rows.len());
    let mut taken = Vec::new();
    for key_row in rows {
        let kind = key_row.text("kind").map_err(column_error)?;
        if kind != row.kind
            || key_row.text("namespace").map_err(column_error)? != row.namespace
            || key_row
                .opt_i64("released_at")
                .map_err(column_error)?
                .is_none()
        {
            return Ok(None);
        }
        let key = key_row.text("key").map_err(column_error)?;
        keys.push(key.to_owned());
        if let Some(holder) = key_row.opt_text("holder_id").map_err(column_error)? {
            let holder_id = Uuid::parse_str(holder).map_err(|_| {
                StorageError::Internal("canonical restore holder id is invalid".into())
            })?;
            taken.push(EntityKeyHolder {
                kind: kind.to_owned(),
                key: key.to_owned(),
                holder_id,
            });
        }
    }
    Ok(Some(ReleasedKeys { keys, taken }))
}

fn check_tombstone(token: &NamespaceToken, row: &Entity) -> Result<(), AtomicOpFailure> {
    if row.namespace != token.namespace().as_str() {
        return Err(mismatch(row.id, "namespace"));
    }
    if row.deleted_at.is_none() {
        return Err(mismatch(row.id, "deleted_at"));
    }
    if row.merged_into.is_some() {
        return Err(mismatch(row.id, "merged_into"));
    }
    if row
        .tags
        .iter()
        .any(|tag| khive_types::pack::pack_registry_tag(tag).is_some())
    {
        return Err(failure(
            "canonical restore cannot restore a pack registry row",
        ));
    }
    Ok(())
}

fn check_preimage(
    row: &Entity,
    expected: &DeleteOp,
    released_keys: Vec<String>,
) -> Result<(), AtomicOpFailure> {
    let mut actual = entity_record(row, released_keys)?;
    actual.entity.deleted_at = None;
    khive_changeset::check_delete(expected, CanonicalRecordRef::Entity(&actual))
        .map_err(|error| mismatch(row.id, &error.field_path))
}

fn admission_error(error: AtomicOpFailure) -> RuntimeError {
    match error {
        AtomicOpFailure::PreimageMismatch {
            target_id,
            field_path,
        } => KhiveError::conflict("captured preimage no longer matches")
            .with_details(Details::new_owned([
                ("id", target_id.to_string()),
                ("field", field_path),
            ]))
            .into(),
        AtomicOpFailure::SqlError { message, .. } => RuntimeError::InvalidInput(message),
        other => RuntimeError::Internal(format!("unexpected canonical restore refusal: {other:?}")),
    }
}

pub(super) async fn prepare(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    mut tombstone: Entity,
    expected: &DeleteOp,
    projected_keys: Option<&[String]>,
    now: i64,
) -> RuntimeResult<Entity> {
    if tombstone.namespace != token.namespace().as_str() {
        return Err(RuntimeError::NotFound(tombstone.id.to_string()));
    }
    check_tombstone(token, &tombstone).map_err(admission_error)?;
    let released_keys = if let Some(projected_keys) = projected_keys {
        projected_keys.to_vec()
    } else {
        let mut reader = runtime.sql().reader().await?;
        read_released_keys(reader.as_mut(), &tombstone)
            .await?
            .ok_or_else(|| admission_error(mismatch(tombstone.id, "keys")))?
            .keys
    };
    check_preimage(&tombstone, expected, released_keys).map_err(admission_error)?;
    KhiveRuntime::project_entity_restore(&mut tombstone, now)?;
    Ok(tombstone)
}

pub(super) async fn materialize(
    writer: &mut dyn SqlWriter,
    token: &NamespaceToken,
    tombstone: Entity,
    expected: &DeleteOp,
    now: i64,
) -> Result<AtomicOpPlan, AtomicOpFailure> {
    check_tombstone(token, &tombstone)?;
    let ReleasedKeys { keys, taken } = read_released_keys(writer, &tombstone)
        .await
        .map_err(storage)?
        .ok_or_else(|| mismatch(tombstone.id, "keys"))?;
    check_preimage(&tombstone, expected, keys)?;
    if !taken.is_empty() {
        return Err(AtomicOpFailure::RestoreKeyConflict(
            EntityRestoreKeyConflict {
                target_id: tombstone.id,
                taken_keys: taken,
            },
        ));
    }
    let (restored, mut plan) = KhiveRuntime::prepare_entity_restore_plan(tombstone, now)
        .map_err(|error| failure(error.to_string()))?;
    // The full preimage check matched these aliases including their origins.
    // Reinsert the stored set so duplicate aliases in the capture do not
    // generate duplicate statements.
    plan.statements.push(PlanStatement {
        statement: entity_aliases_delete_all_statement(&restored.namespace, restored.id),
        guard: None,
    });
    plan.statements
        .extend(restored.aliases.iter().map(|alias| PlanStatement {
            statement: entity_alias_insert_statement(
                &restored.namespace,
                restored.id,
                &alias.alias,
                alias.origin,
                restored.updated_at,
            ),
            guard: None,
        }));
    plan.post_commit = PostCommitEffect::ReindexEntity {
        entity_id: restored.id,
    };
    Ok(AtomicOpPlan::Update(Box::new(plan)))
}
