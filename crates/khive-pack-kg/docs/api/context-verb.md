# The `context` verb

Technical reference for `handle_context` (`handlers/context.rs`) — entity-anchored graph
context assembly (ADR-089): resolving anchors, expanding neighbors, and packing the result
within a byte budget.

## `relations_all_symmetric`

Mirrors `normalize_symmetric_direction` in `khive-runtime/src/operations.rs` (private to that
crate) — kept in lockstep because `neighbors_with_query` forces `Direction::Both` under this
exact condition regardless of the direction actually requested, and the handler must know
that happened to tag direction correctly instead of issuing a second, redundant call.

## `fetch_directed_neighbors`

Fetches up to `fanout` neighbors of `node_id`, each tagged with its actual direction relative
to `node_id` — it can't just trust a `direction` field on a plain `NeighborHit` because
`neighbors_with_query_directed` only ever tags hits `Out`/`In` (`Both` never appears in a
`DirectedNeighborHit`).

## `assemble_within_budget`

A deterministic-order budget walk: it appends anchor entity records and their neighbor
records (each already produced in final display order) until the next record's compact-JSON
Unicode-scalar length would push the running total past `budget`. Returns (assembled anchors,
truncated, dropped anchors, dropped neighbors). A budget exactly equal to the cumulative size
does NOT truncate — the stop condition is "would push the running total PAST budget", so a
record landing exactly on the boundary still fits.

## `handle_context` stage notes

- **Directed-neighbor fetch**: a single UNION ALL query for both directions (ADR-089
  context-verb optimization) instead of two separate direction-scoped calls — halves the
  storage neighbor SELECT count for this branch. The op already returns hits in global
  weight-descending, node_id-ascending order truncated to `fanout`, so no local
  re-sort/truncate is needed.
- **Stage 1 (anchor resolution)**: `entity_ids` is an explicit entity-anchor contract
  (ADR-089 §1: "honored in full"). `resolve_uuid_async` accepts any syntactically valid
  UUID without checking substrate or existence, so a random UUID, a note UUID, or an edge
  UUID would otherwise resolve here and then silently vanish from the response in Stage 4's
  lenient "missing entity" fallback. The handler fails loudly instead: one batch existence
  check names every offending id.
- **Query-anchor overfetch**: fetches a larger candidate window than `limit` so that anchors
  which collapse into `entity_ids` duplicates don't under-fill the query leg — ADR-089 §1
  promises search "fills up to `limit` additional anchors" after explicit ids, which requires
  looking past the first `limit` hits when some of them overlap explicit anchors. Bounded by
  a documented cap so a pathological overlap can't turn into an unbounded search.
- **Stage 2 (expansion), hop-1 stratum**: one stratum across all hop-1 parents under an
  anchor, sorted by weight desc, then neighbor id, then parent id (the last key only
  arbitrates true ties — same neighbor, same weight, different parent — so the "first
  discovering parent" is deterministic).
- **Stage 4 (assembly)**: explicit `entity_ids` anchors are already verified to exist in
  Stage 1; the Stage 4 existence check only guards the residual race of an anchor deleted
  concurrently between resolution and this fetch, or a neighbor entity that vanished the same
  way. Neighbors get the same lenient "missing node reads as absent" convention
  `neighbors_with_query` already applies (it returns an empty Vec rather than erroring on a
  nonexistent `node_id`) — they never enter the budget accounting.
