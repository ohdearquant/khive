use std::collections::HashSet;

use khive_runtime::{KhiveRuntime, NamespaceToken, Resolved, RuntimeError, VerbRegistry};
use khive_storage::entity::Entity;
use uuid::Uuid;

const MAX_REDIRECT_HOPS: usize = 32;

pub(super) fn reject_live_redirect(entity: &Entity) -> Result<(), RuntimeError> {
    if entity.deleted_at.is_none() {
        if let Some(next_id) = entity.merged_into {
            return Err(RuntimeError::Internal(format!(
                "live entity {} carries merged_into {next_id}",
                entity.id
            )));
        }
    }
    Ok(())
}

pub(super) async fn followed_entity(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    requested_id: Uuid,
) -> Result<Option<(Entity, Vec<Uuid>)>, RuntimeError> {
    let mut current_id = requested_id;
    let mut visited = HashSet::new();
    let mut redirected_from = Vec::new();

    loop {
        if !visited.insert(current_id) {
            return Err(RuntimeError::InvalidInput("redirect cycle detected".into()));
        }
        let Some(Resolved::Entity(entity)) = registry
            .resolve_kg_read_by_id(runtime, token, current_id, true)
            .await?
        else {
            if !redirected_from.is_empty() {
                return Err(RuntimeError::NotFound(format!(
                    "redirect target {current_id}"
                )));
            }
            return Ok(None);
        };
        reject_live_redirect(&entity)?;
        if let Some(next_id) = entity.merged_into {
            if redirected_from.len() == MAX_REDIRECT_HOPS {
                return Err(RuntimeError::InvalidInput("redirect chain too long".into()));
            }
            redirected_from.push(current_id);
            current_id = next_id;
            continue;
        }
        return Ok(entity
            .deleted_at
            .is_none()
            .then_some((entity, redirected_from)));
    }
}
