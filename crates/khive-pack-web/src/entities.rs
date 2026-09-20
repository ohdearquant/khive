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
//! method. Flagged as an open question in LEG_B_REPORT.md: whether
//! `operations.rs` should grow an explicit-id create helper so a pack never
//! needs this two-step form.

use khive_runtime::{EntityPatch, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::Entity;
use serde_json::Value;
use uuid::Uuid;

/// Fetch the entity at `id` if it already exists (by-id, namespace-agnostic
/// per ADR-007 Rev 8), else insert it with `entity_kind`/`entity_type`/`name`
/// and `properties`, indexed for search exactly as an ordinary `create`
/// would be. Returns `(entity, created)`; `created = false` both when the
/// row already existed and when this call lost a race to a concurrent
/// writer creating the same deterministic id (the winner is read back and
/// returned either way — same-id writers always converge on one row).
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
        let winner = store.get_entity(id).await?.ok_or_else(|| {
            RuntimeError::Internal(format!(
                "web entity {id}: insert_entity_if_absent lost the race but no row is readable"
            ))
        })?;
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
