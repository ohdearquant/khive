# The `context` verb

Technical reference for `handle_context` (`handlers/context.rs`): entity anchors, scoped
neighbor expansion, and a bounded paired graph payload. The payload follows the draft
[ADR-140](../../../../docs/adr/ADR-140-context-graph-payload.md), which remains Proposed;
architecture acceptance is required before dependent implementation merges. ADR-089 remains
Accepted and unchanged.

## Response and endpoint metadata

Every successful response includes `anchors`, `edges`, `anchor_edges`, `truncated`, and
`dropped`. `anchor_edges` is always an array, including when no edges connect selected anchors.
`edges` is an array, including for zero-hop, isolated-anchor, empty-search, and wholly
budget-cut results. `dropped.edges` is always present and equals `dropped.neighbors`;
`dropped.anchor_edges` counts excluded anchor-edge records, and `dropped.stage` remains
`"budget"`.

Each edge has exactly `source_id`, `source_name`, `target_id`, `target_name`, `relation`,
`weight`, `direction`, `hop`, and `via`. Its relation, weight, direction, hop, and via match
the corresponding nested neighbor. Flat edge order follows anchor order and each anchor's
neighbor order. Hop 1 uses the anchor as parent and `via: null`; hop 2 uses the actual `via`
record as parent. Explicit anchors are still entities; discovered endpoints and hop-2
parents can be entities, notes, or mailbox-visible messages.

Outgoing edges reproduce parent → neighbor; incoming edges reproduce neighbor → parent.
Selected symmetric hits use `"both"` and deterministic parent → neighbor endpoint order,
which carries no stored assertion direction. Endpoint names come from the existing scoped
metadata maps. An unnamed note or message has a JSON `null` name on either side; content,
subject, UUID, and empty strings are not substitutes.

`anchor_edges` contains one entry per stored edge found by the hop-1 walk whose two endpoints
are selected anchors. Each entry has exactly `source_id`, `source_name`, `target_id`,
`target_name`, `relation`, and `weight`. It deduplicates by the stored edge ID, even when both
anchors' walks find the same edge. It reuses the endpoint orientation above: incoming hits put
the neighbor before the anchor, while outgoing hits and symmetric `"both"` hits put the anchor
first. These entries come from the same filtered hop-1 results as neighbor expansion, so the
relation, direction, namespace, and mailbox permission filters are unchanged. Anchor-to-anchor
hits remain excluded from neighbor lists and hop-2 parents, and their edges appear only here.
Edges from a hop-1 neighbor to an anchor are not included.

Under ADR-140 Amendment 1, `anchor_edges` spends the budget remaining after anchors and
neighbor/edge pairs. Entries retain their stored-edge-ID order. Each entry costs the
Unicode-scalar length of its compact JSON serialization and is admitted only when both
endpoint anchors were admitted and its cost fits. Excluded entries increase
`dropped.anchor_edges` and set `truncated`; a later fitting entry may still be admitted.
This final pass preserves the nested `neighbors`, top-level `edges`, and their drop counts
for every budget. Candidate discovery remains bounded by the hop-1 fan-out of selected
anchors: at most `selected_anchor_count × fanout × visible_namespace_count` hits.

If either endpoint lacks scoped metadata at assembly time, omit the whole pair before
budget accounting. Preserve the visited decision: no rediscovery, metadata refetch, extra
graph read, fabricated endpoint, truncation flag, or dropped-count increment. Namespace and
mailbox visibility apply to the entire payload, including IDs, names, and `via`.

## `relations_all_symmetric` and `fetch_directed_neighbors`

The all-symmetric filter check mirrors runtime `normalize_symmetric_direction`; the storage
operation can normalize its query to both directions under that condition. The handler
normalizes every selected symmetric hit to `"both"`, including absent, empty, mixed, and
all-symmetric filters. It preserves each query's existing selection semantics rather than
adding reverse hits to direction-filtered queries.

The both-direction branch uses the existing directed UNION ALL query, whose results are
already ordered by descending weight then UUID and limited to the existing fanout window.
The handler does not change selection, ordering, mailbox refill, or per-node caps.

## `assemble_within_budget`

Assembly first admits a prefix of anchor entity records. It then admits a prefix of
neighbor/edge pairs for each admitted anchor. An oversized next pair stops that anchor's
neighbor pass; later admitted anchors may still contribute fitting pairs. Anchor priority
is preserved, and later pairs under the same anchor cannot skip an oversized predecessor.

Each cost is the Unicode-scalar length of compact JSON: an anchor's entity record, or the
sum of a pair's neighbor and edge records. Endpoint names and null fields are charged;
response envelopes, containers, and separators are excluded. This is a character budget,
not a UTF-8 byte count. Exact fits are admitted. The effective-budget bounds and
`budget_clamped` remain unchanged.

A budget-cut pair is emitted in neither location. Drops for budget-cut anchors include
those anchors' pairs. The assembler returns anchors, edges, truncation, dropped anchors,
and dropped pairs, plus the consumed record budget for the final anchor-edge pass; the same
pair count supplies both dropped neighbor and edge counts.
ADR-140 explicitly proposes this two-pass/per-anchor rule as a replacement for ADR-089
§Semantics item 5's generic global-stop prose upon acceptance.

## `handle_context` stage notes

- **Anchor resolution:** explicit `entity_ids` are verified as scoped live entities in one
  batch. Missing IDs and non-entity IDs fail; the graph payload does not relax that contract.
- **Query anchors:** bounded overfetch avoids under-filling when query hits overlap explicit
  anchors. Explicit anchors retain their order and query hits fill additional positions.
- **Expansion:** hop-1 precedes hop-2. Each stratum sorts by weight descending, neighbor UUID,
  and parent UUID for true ties. The first discovering parent and existing visited set own
  a node. `NotFound` after discovery retains the existing empty-expansion behavior.
- **Hydration:** existing scoped entity, note, owning-backend, and mailbox reads build the
  metadata maps once. Assembly builds both endpoint fields from those maps.
- **Assembly:** available endpoint pairs are formed and costed before budgeting. Metadata
  missing after expansion is an omission, while budget-cut pairs alone affect budget drops.

The private unit-test hydration barrier exists only under `cfg(test)`. It is keyed by a
unique fixture namespace, serialized by the test guard, and uses finite watchdogs around
actual registry context/delete calls to exercise stage-2/stage-3 races; it introduces no
production state or public runtime hook.
