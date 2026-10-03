//! Message-note endpoint scoping for generic graph reads.

use khive_runtime::{
    KhiveRuntime, MailboxView, NamespaceToken, Resolved, RuntimeError, VerbRegistry,
};
use khive_storage::types::NeighborHit;
use uuid::Uuid;

/// Keep the existing graph reference scope while withholding message-prefix
/// candidates that are outside the caller's mailbox.
pub(super) async fn resolve_mailbox_graph_id(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    view: &MailboxView,
    reference: &str,
) -> Result<Uuid, RuntimeError> {
    let is_prefix = Uuid::parse_str(reference).is_err()
        && reference.len() >= 8
        && reference.chars().all(|ch| ch.is_ascii_hexdigit());
    let result = super::common::resolve_uuid_async(reference, runtime, token).await;
    if !is_prefix {
        return result;
    }
    let guidance = || {
        RuntimeError::InvalidInput(
            "this message prefix requires a full UUID; use comm.inbox to find messages".into(),
        )
    };
    match result {
        Ok(id) => {
            if message_prefix_candidate_permitted(runtime, registry, token, view, id).await? {
                Ok(id)
            } else {
                Err(guidance())
            }
        }
        Err(error @ RuntimeError::AmbiguousPrefix { .. }) => {
            if let RuntimeError::AmbiguousPrefix { matches, .. } = &error {
                for id in matches {
                    if !message_prefix_candidate_permitted(runtime, registry, token, view, *id)
                        .await?
                    {
                        return Err(guidance());
                    }
                }
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

async fn message_prefix_candidate_permitted(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    view: &MailboxView,
    id: Uuid,
) -> Result<bool, RuntimeError> {
    match registry
        .resolve_kg_read_by_id(runtime, token, id, true)
        .await?
    {
        Some(Resolved::Note(note)) => return Ok(view.permits_message_note(token, &note)),
        Some(_) => return Ok(true),
        None => {}
    }
    if let Some(edge) = runtime.get_edge_including_deleted(token, id).await? {
        return Ok(
            message_endpoint_permitted(runtime, registry, token, view, edge.source_id).await?
                && message_endpoint_permitted(runtime, registry, token, view, edge.target_id)
                    .await?,
        );
    }
    Ok(true)
}

/// Resolve through the existing KG read inventory while retaining the caller
/// token. Tombstones are checked too because an adjacency may outlive its note.
/// Ordinary non-message endpoints keep their existing graph-read behavior.
pub(super) async fn message_endpoint_permitted(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    view: &MailboxView,
    id: Uuid,
) -> Result<bool, RuntimeError> {
    Ok(
        match registry
            .resolve_kg_read_by_id(runtime, token, id, true)
            .await?
        {
            Some(Resolved::Note(note)) => view.permits_message_note(token, &note),
            _ => true,
        },
    )
}

/// Enrichment selects entities before notes and supplies their immutable kind.
/// Reuse that read for non-message hits; absent metadata and message hits need
/// the owning note backend, including tombstones left behind an adjacency.
/// As with graph projection hydration, concurrent record replacement is not
/// made atomic with the adjacency read.
pub(super) async fn message_neighbor_permitted(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    view: &MailboxView,
    hit: &NeighborHit,
) -> Result<bool, RuntimeError> {
    if hit.kind.as_deref().is_some_and(|kind| kind != "message") {
        return Ok(true);
    }
    let note_runtime = registry.kg_note_read_runtime_for_kind(runtime, "message");
    Ok(note_runtime
        .notes(token)?
        .get_note_including_deleted(hit.node_id)
        .await?
        .is_none_or(|note| view.permits_message_note(token, &note)))
}
