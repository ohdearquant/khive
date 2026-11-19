//! Message-note endpoint scoping for generic graph reads.

use std::collections::HashMap;

use khive_runtime::{
    KhiveRuntime, MailboxView, NamespaceToken, Resolved, RuntimeError, VerbRegistry,
};
use khive_storage::types::{Edge, NeighborHit};
use uuid::Uuid;

/// Keep the existing graph reference scope while withholding message-prefix
/// candidates that are outside the caller's mailbox. A prefix whose only
/// matches are withheld is answered like a prefix that matches nothing, and an
/// ambiguity error lists only records the caller may read.
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
    match result {
        Ok(id) => {
            if message_prefix_candidate_permitted(runtime, registry, token, view, id).await? {
                Ok(id)
            } else {
                Err(super::common::prefix_not_found(reference))
            }
        }
        Err(RuntimeError::AmbiguousPrefix { prefix, matches }) => {
            let (mut readable, withheld) =
                screen_prefix_candidates(runtime, registry, token, view, &matches).await?;
            if !withheld {
                return Err(RuntimeError::AmbiguousPrefix { prefix, matches });
            }
            if readable.len() < 2 {
                readable =
                    readable_prefix_matches(runtime, registry, token, view, reference).await?;
            }
            match readable.len() {
                0 => Err(super::common::prefix_not_found(reference)),
                1 => Ok(readable[0]),
                _ => Err(RuntimeError::AmbiguousPrefix {
                    prefix,
                    matches: readable,
                }),
            }
        }
        Err(error) => Err(error),
    }
}

/// Split prefix candidates into the ones the caller may read and whether any
/// candidate was withheld.
async fn screen_prefix_candidates(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    view: &MailboxView,
    candidates: &[Uuid],
) -> Result<(Vec<Uuid>, bool), RuntimeError> {
    let mut readable = Vec::with_capacity(candidates.len());
    let mut withheld = false;
    for id in candidates {
        if message_prefix_candidate_permitted(runtime, registry, token, view, *id).await? {
            readable.push(*id);
        } else {
            withheld = true;
        }
    }
    Ok((readable, withheld))
}

/// The resolver bounds its ambiguity sample (two rows per table, and it stops
/// at the first table that reaches two), so a sample holding a withheld record
/// cannot show how many readable records share the prefix. Resolve each longer
/// prefix to enumerate them, stopping once two readable records are known.
async fn readable_prefix_matches(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    view: &MailboxView,
    prefix: &str,
) -> Result<Vec<Uuid>, RuntimeError> {
    let mut readable: Vec<Uuid> = Vec::new();
    let mut pending = vec![prefix.to_string()];
    while let Some(next) = pending.pop() {
        let (candidates, ambiguous) = match runtime.resolve_prefix(token, &next).await {
            Ok(None) => (Vec::new(), false),
            Ok(Some(id)) => (vec![id], false),
            Err(RuntimeError::AmbiguousPrefix { matches, .. }) => (matches, true),
            Err(error) => return Err(error),
        };
        let (found, withheld) =
            screen_prefix_candidates(runtime, registry, token, view, &candidates).await?;
        for id in found {
            if !readable.contains(&id) {
                readable.push(id);
            }
        }
        if readable.len() >= 2 {
            break;
        }
        if ambiguous && withheld && next.len() < 32 {
            for digit in "fedcba9876543210".chars() {
                pending.push(format!("{next}{digit}"));
            }
        }
    }
    readable.sort_unstable();
    Ok(readable)
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

/// Decides, edge by edge, whether the caller may read both endpoints. Each
/// endpoint is looked up once for the life of the scope.
pub(super) struct EdgeScope<'a> {
    runtime: &'a KhiveRuntime,
    registry: &'a VerbRegistry,
    token: &'a NamespaceToken,
    view: &'a MailboxView,
    cache: HashMap<Uuid, bool>,
}

impl<'a> EdgeScope<'a> {
    pub(super) fn new(
        runtime: &'a KhiveRuntime,
        registry: &'a VerbRegistry,
        token: &'a NamespaceToken,
        view: &'a MailboxView,
    ) -> Self {
        Self {
            runtime,
            registry,
            token,
            view,
            cache: HashMap::new(),
        }
    }

    pub(super) async fn permits(&mut self, edge: &Edge) -> Result<bool, RuntimeError> {
        for id in [edge.source_id, edge.target_id] {
            let permitted = match self.cache.get(&id) {
                Some(permitted) => *permitted,
                None => {
                    let permitted = message_endpoint_permitted(
                        self.runtime,
                        self.registry,
                        self.token,
                        self.view,
                        id,
                    )
                    .await?;
                    self.cache.insert(id, permitted);
                    permitted
                }
            };
            if !permitted {
                return Ok(false);
            }
        }
        Ok(true)
    }
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
