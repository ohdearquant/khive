# Generic message-read scope

This record describes the shipped generic-read behavior from #3763 and qualifies
[ADR-007](../../../../docs/adr/ADR-007-namespace.md) and
[ADR-089](../../../../docs/adr/ADR-089-context-verb.md). Message notes keep their ordinary
stored namespace. Their read view additionally follows the caller's mailbox.

## Caller and message copies

`KhiveRuntime::authorize_mailbox_view` receives the dispatch-authorized caller token, verb
and original arguments. `get`, `search`, `list`, `neighbors` and `context` use the caller's
own view and have no cross-actor selector. Changing a namespace or knowing a full UUID
does not select another actor's messages. Delegated mailbox selection belongs to the
separately authorized comm read methods.

`MailboxView::permits_message_note` accepts non-message notes unchanged. For messages it
accepts inbound copies whose `to_actor` matches the view and outbound copies whose
`from_actor` matches it. Only the non-delegated anonymous `local` caller retains the
documented legacy unattributed local pool. Named actors do not inherit that pool.
`MailboxView::note_scope` expresses the same partition for store-side filtering.

Generic `get` returns not found for an unreadable message copy or a direct edge reference
to one. Its annotations exclude unreadable messages. Note searches apply the view even
for a broad `kind="note"` request, before returning hits. Graph reads check message
origins and endpoints before rendering them, including endpoints whose message note is
now a tombstone. `context` still requires explicit anchors to be live visible entities;
mailbox filtering does not make a note a valid explicit anchor. Entity and other non-message
records retain their existing read rules.

## Prefixes and configured backends

Full-ID KG reads can resolve through `VerbRegistry::resolve_kg_read_by_id`, which checks
the configured entity/note backend inventory with the original token. Choosing an owning
backend selects storage; it does not mint a different caller identity or grant a message
view. Single-runtime registration keeps its existing resolver behavior, and these read
helpers do not reroute mutations or pack-private records.

Short-prefix ambiguity samples are bounded. Withholding message candidates from a sample
cannot establish that the remaining candidate is globally unique. Message-related prefix
refusals require a full UUID and direct the caller to `comm.inbox` rather than publishing
unreadable candidates or claiming the filtered sample is complete. Ordinary non-message
prefix ambiguity retains its declared behavior.

## Library preconditions and snapshot limits

`VerbRegistry::neighbors_for_kg_read` and its directed/entity-kind variants verify a live
origin across the configured KG read inventory before expanding the graph. An absent
origin is not found; a full-ID origin outside the graph's visible namespaces can still
have in-scope edges. Message origin and endpoint filtering remains the calling handler's
responsibility.

The public `KhiveRuntime::neighbors_for_resolved_kg_read` and
`neighbors_for_resolved_kg_read_with_entity_kinds` helpers assume that the caller has
already verified the live origin and applied its record-kind read scope. They are not an
authorization or mailbox-filtering shortcut. Preserve the original caller token for
adjacency and enrichment. `KgNeighborRead::namespace` may select only a namespace already
in that token's visible set; an invisible selection is refused. Shared query, projection,
kind and cursor options filter that scope rather than widening it.

Origin resolution, adjacency, deletion screening, endpoint policy checks and record
hydration are separate reads. They do not promise an atomic snapshot against concurrent
record deletion or replacement. Missing entity-kind hints still require a message check
on the owning note backend; the presence of an ordinary entity-kind hint can reuse the
existing graph screen without another message lookup.

## Bounded graph scans and continuation

Limited `neighbors` reads refill independently in each visible namespace when mailbox
filtering leaves too few admitted neighbors. `context` does the same for each expanded
node. Windows double up to 10,000 candidates, then merge and deduplicate admitted rows.
Candidate work includes all visible namespaces and repeated refill windows; a small
output limit or `context` character budget is not a bound on that work.

`scan_incomplete: true` means a scan stopped at its cap with a potentially incomplete
graph view. It differs from `context`'s output-budget `truncated` flag. A limited
`neighbors` response sets `next_after` only when it has an extra admitted row beyond the
requested page. The cursor is based on the last returned admitted row, never a hidden
message. A capped scan can have `scan_incomplete: true` and `next_after: null`; do not read
that combination as proof that no later readable neighbors exist. If a cursor is present,
pass it unchanged as `after` with an explicit `limit`. `context` has no scan cursor and
repeating the same capped request does not guarantee progress.

## Coordinator integration and implementation pointers

Coordinated note search must carry the gate-authorized caller token and original arguments
through backend search and hydration. Each backend still performs its existing namespace
admission checks. The compatibility fallback refuses scoped note search when a coordinator
only implements the older namespace-based method; entity search retains that fallback.
See the [coordinator contract](../../../khive-mcp/docs/api/coordinator.md).

The relevant source seams are `khive-runtime/src/mailbox_view.rs`, `kg_read.rs`,
`operations.rs` and `pack.rs`; `khive-pack-kg/src/handlers/get.rs`, `search.rs`, `graph.rs`,
`context.rs` and `message_scope.rs`; and `kkernel/src/coordinator/dispatch.rs` and
`service.rs`. Existing regression coverage lives in `message_search_get_scope.rs`,
`message_graph_scope.rs` and `list_mailbox_scan_cursor.rs` under `khive-pack-kg/tests`.
