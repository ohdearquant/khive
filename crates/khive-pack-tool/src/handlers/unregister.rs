//! Retirement of one registry row and its namespace-local authorization rows.

use super::*;

fn missing() -> RuntimeError {
    RuntimeError::NotFound("tool is not registered in the write namespace".into())
}

fn owned_registry(entity: Entity, token: &NamespaceToken) -> Result<Entity, RuntimeError> {
    if entity.namespace != token.namespace().as_str()
        || entity.kind != REGISTRY_ENTITY_KIND
        || !entity
            .tags
            .iter()
            .any(|tag| pack_registry_tag(tag) == Some(REGISTRY_TAG))
    {
        return Err(missing());
    }
    Ok(entity)
}

async fn resolve_owned(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    reference: &str,
) -> Result<Entity, RuntimeError> {
    let own = token.with_namespace(token.namespace().clone());
    if let Ok(id) = Uuid::parse_str(reference) {
        if let Some(entity) = rt.entities(&own)?.get_entity_including_deleted(id).await? {
            return owned_registry(entity, &own);
        }
    } else if reference.len() >= 8 && reference.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        if let Some(id) = rt.resolve_prefix(&own, reference).await? {
            return owned_registry(rt.get_entity(&own, id).await?, &own);
        }
    }
    if let Some(entity) = find_by_name(rt, &own, reference).await? {
        return owned_registry(entity, &own);
    }
    if let Some(id) = same_name_tombstone(
        rt,
        &own,
        reference,
        REGISTRY_ENTITY_KIND,
        None,
        REGISTRY_TAG,
    )
    .await?
    {
        let entity = rt
            .entities(&own)?
            .get_entity_including_deleted(id)
            .await?
            .ok_or_else(missing)?;
        return owned_registry(entity, &own);
    }
    Err(missing())
}

pub(crate) async fn unregister(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let object = params
        .as_object()
        .ok_or_else(|| RuntimeError::InvalidInput("parameters must be an object".into()))?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "tool" | "namespace"))
    {
        return Err(RuntimeError::InvalidInput(
            "tool.unregister accepts only tool and namespace".into(),
        ));
    }
    let reference = req_str(&params, "tool")?;
    opt_str(&params, "namespace")?;
    let entity = resolve_owned(rt, token, &reference).await?;
    if entity.deleted_at.is_some() {
        return Ok(json!({
            "id":entity.id, "full_id":entity.id, "retired":false,
            "policies_retired":0, "grants_revoked":0, "post_commit_degradations":[],
        }));
    }
    let retired_at = now_micros().max(entity.updated_at.checked_add(1).ok_or_else(|| {
        RuntimeError::InvalidInput("registry retirement timestamp is exhausted".into())
    })?);
    let companions = policy::registry_retirement_statements(
        token.namespace().as_str(),
        entity.id,
        &entity.name,
        retired_at,
        &actor_label(token),
    );
    let ([policies_retired, grants_revoked], degradations) = rt
        .retire_tool_registry_entity_with_post_commit_report(token, &entity, retired_at, companions)
        .await?;
    Ok(json!({
        "id":entity.id, "full_id":entity.id, "retired":true,
        "policies_retired":policies_retired, "grants_revoked":grants_revoked,
        "post_commit_degradations":degradations,
    }))
}
