//! Idempotent, deterministic-id entity creation over the runtime's create
//! seam (ADR-191 D3: "every write lands in the caller's namespace through
//! the runtime's create seam").
//!
//! `KhiveRuntime::create_entity` (and every other `operations.rs` create
//! helper) always assigns a fresh `Uuid::new_v4()` — there is no public,
//! non-crate-private path that both accepts a caller-supplied id AND runs
//! the full FTS+embedding indexing pipeline in one call (verified by reading
//! `operations.rs::create_entity_with_embedding_report_inner`, whose
//! embedding step calls `embed_document_with_model_outcome_for_token`, a
//! `pub(crate)` method on `KhiveRuntime` — unreachable from this crate).
//! ADR-191 D1 requires the persisted `entity.id` itself to equal the
//! deterministic UUIDv5 (A5's "id equality row by row" across two
//! independent ingests), so a caller-chosen id is not optional here.
//!
//! The seam this module uses instead is two public runtime calls, both part
//! of "the runtime" in the same sense `operations.rs`'s own internals are:
//! `KhiveRuntime::entities()` (the `EntityStore` capability trait, ADR-005)
//! for the id-carrying insert, then `KhiveRuntime::update_entity()` (the
//! same method the `update` verb dispatches to) to route the real name and
//! properties through the ordinary reindex path. `update_entity`'s reindex
//! only fires on a `name`/`description`/`entity_type` value change
//! (`curation.rs::prepare_update_entity`), so the bare row is inserted with
//! an empty name on purpose — the immediately following patch always
//! differs from `""`, so it always reindexes, and FTS+embedding parity with
//! an ordinary `create` is achieved without touching any crate-private
//! method.

use khive_runtime::{EntityPatch, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{Entity, EntityStore};
use serde_json::Value;
use uuid::Uuid;

/// Refuse web writes that would reuse a differently attributed entity.
///
/// Runtime by-ID reads remain namespace-agnostic under ADR-007 Rev 8. This
/// is an interim web mutation safeguard while namespace-aware deterministic
/// identity is pending; it does not change the UUID derivation or store policy.
/// A refusal discloses only the requested ID, never the stored attribution.
pub(crate) fn require_entity_namespace(
    token: &NamespaceToken,
    entity: &Entity,
) -> Result<(), RuntimeError> {
    if entity.namespace != token.namespace().as_str() {
        return Err(khive_types::KhiveError::not_found("web entity", entity.id).into());
    }
    Ok(())
}

async fn read_insert_winner(
    store: &dyn EntityStore,
    token: &NamespaceToken,
    id: Uuid,
) -> Result<Entity, RuntimeError> {
    let winner = store.get_entity(id).await?.ok_or_else(|| {
        RuntimeError::Internal(format!(
            "web entity {id}: insert_entity_if_absent lost the race but no row is readable"
        ))
    })?;
    require_entity_namespace(token, &winner)?;
    Ok(winner)
}

/// Fetch the entity at `id` if it already exists in the caller's namespace,
/// else insert it with `entity_kind`/`entity_type`/`name`
/// and `properties`, indexed for search exactly as an ordinary `create`
/// would be. Returns `(entity, created)`; `created = false` both when the
/// row already existed and when this call lost a race to a concurrent
/// writer creating the same deterministic id in the same namespace. A foreign
/// row from either lookup is refused before a caller can patch or link it.
pub(crate) async fn get_or_create(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    entity_kind: &str,
    entity_type: &str,
    name: &str,
    properties: Value,
) -> Result<(Entity, bool), RuntimeError> {
    let store = runtime.entities(token)?;
    if let Some(existing) = store.get_entity(id).await? {
        require_entity_namespace(token, &existing)?;
        return Ok((existing, false));
    }

    let mut bare = Entity::new(token.namespace().as_str(), entity_kind, "")
        .with_entity_type(Some(entity_type));
    bare.id = id;
    let inserted = store
        .insert_entity_if_absent(bare)
        .await
        .map_err(RuntimeError::from)?;
    if !inserted {
        let winner = read_insert_winner(store.as_ref(), token, id).await?;
        return Ok((winner, false));
    }

    let entity = runtime
        .update_entity(
            token,
            id,
            EntityPatch {
                name: Some(name.to_string()),
                properties: Some(properties),
                ..Default::default()
            },
        )
        .await?;
    Ok((entity, true))
}

/// Patch an already-existing web entity's properties (deep-merged, per
/// `EntityPatch`'s documented semantics) and/or re-type it in place. Used by
/// `refresh` (unchanged body: still a no-op — callers only invoke this when
/// something actually changed) and by `fetch`'s re-typing of an unfetched
/// `resource` to `page` once the body is known to be HTML (D3).
pub(crate) async fn patch(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    entity_type: Option<&str>,
    properties: Value,
) -> Result<Entity, RuntimeError> {
    runtime
        .update_entity(
            token,
            id,
            EntityPatch {
                entity_type: entity_type.map(|t| Some(t.to_string())),
                properties: Some(properties),
                ..Default::default()
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{Namespace, RuntimeConfig};
    use serde_json::json;

    #[tokio::test]
    async fn lost_insert_race_refuses_foreign_winner_without_mutating_it() {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: None,
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let alpha = runtime
            .authorize(Namespace::parse("alpha").unwrap())
            .unwrap();
        let beta = runtime
            .authorize(Namespace::parse("beta").unwrap())
            .unwrap();
        let store = runtime.entities(&beta).unwrap();
        let winner = Entity::new("alpha", "document", "private row title")
            .with_entity_type(Some("resource"))
            .with_properties(json!({"private": "metadata"}));
        assert!(store.get_entity(winner.id).await.unwrap().is_none());

        // Deterministically place another writer between the initial miss and
        // conditional insert, then exercise the production lost-race readback.
        assert!(runtime
            .entities(&alpha)
            .unwrap()
            .insert_entity_if_absent(winner.clone())
            .await
            .unwrap());
        let mut contender = Entity::new("beta", "document", "").with_entity_type(Some("resource"));
        contender.id = winner.id;
        assert!(!store.insert_entity_if_absent(contender).await.unwrap());
        let error = read_insert_winner(store.as_ref(), &beta, winner.id)
            .await
            .unwrap_err();
        let projected =
            khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
        assert_eq!(projected["kind"], "not_found");
        assert_eq!(
            projected["message"],
            format!("web entity not found: {}", winner.id)
        );
        assert!(projected["details"].is_null());
        assert!(projected["code"].is_null());
        let after = store.get_entity(winner.id).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&winner).unwrap()
        );

        // The identical readback remains reusable for the winning namespace.
        let same_namespace = read_insert_winner(store.as_ref(), &alpha, winner.id)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(same_namespace).unwrap(),
            serde_json::to_value(winner).unwrap()
        );
    }
}
