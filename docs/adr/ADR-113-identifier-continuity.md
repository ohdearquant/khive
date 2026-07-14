# ADR-113: Identifier Continuity — Merged-Entity Redirect Resolution and Split Endpoint-Move

**Status**: Proposed

**Date**: 2026-07-14

## Context

Two curation operations change which entity a UUID denotes, and both currently break
identifier continuity for callers or stored references that hold an older UUID.

### Merge tombstones the source entity, but no reader follows the redirect

`merge(into_id = D, from_id = C)` (ADR-014 curation) does not hard-delete `C`. The transaction
rewires every edge incident to `C` onto `D`, then tombstones `C` with a durable redirect: a
single `UPDATE` sets `C.deleted_at`, `C.merged_into = D`, and `C.merge_event_id`. The
`(old → kept)` mapping is therefore already persisted on the row itself, and additionally as an
`EntityMerged` event and the synchronous `MergeSummary`.

No read path consumes that pointer:

- `get(C)` loads `FROM entities WHERE id = ? AND deleted_at IS NULL`, so a merged `C` returns
  not-found — never `D`, never the tombstone.
- `resolve(C)` treats a full UUID as passthrough (ADR-007) and returns `C` verbatim, with no
  liveness check and no redirect follow — a dangling identifier.

So any holder of `C` — a UUID copied into a note or memory body, an external reference, a cached
id from before the merge — loses the entity, even though the substrate knows exactly where it
went. This is the merge-dangling class.

### Split re-mints edge identifiers and orphans edge annotations

A split — keep entity `C`'s UUID, mint a new entity `D` of a different kind, and move a subset of
`C`'s edges to `D` — has no id-preserving edge-move on the public surface. `update` patches an
edge's relation, weight, and properties only, never its endpoints. The only public path to move
an edge is delete-then-`link`, which mints a new edge id.

Re-minting an edge id silently orphans any note that annotates that edge. Judgment-bearing edges
carry an `annotates` note targeting the edge id (ADR-055 epistemic edges), so a re-minted edge
leaves its annotation pointing at a deleted edge. The id-preserving endpoint `UPDATE` that would
avoid this already exists inside the merge transaction
(`UPDATE graph_edges SET source_id = ?, target_id = ?`), but it is internal to merge and is never
re-validated against endpoint rules.

### Why a redirect, not a rewrite

The `(old → kept)` pointer is durable and cheap to follow, and following it at read time is a
view-layer decision that leaves stored data untouched (the currency rule, ADR-014). The
alternative — rewriting every stored reference to `C` on merge — would require a global scan and
would mutate history to fix what a query returns, which the substrate explicitly forbids. Opaque
UUID mentions inside free-text bodies are the one class a pointer cannot cover; those are handled
by a separate pre-mutation body-scan-and-rewrite procedure at the operating-rule layer and are
out of scope here.

## Decision

### (a) Transitive redirect resolution at the by-id read entry points

`get` and `resolve` resolve a UUID through `merged_into` before returning:

1. Load the row by id including tombstoned rows (drop the `deleted_at IS NULL` filter for the
   redirect probe only).
2. If the row is tombstoned with `merged_into = K`, chase to `K`. Repeat transitively — a kept
   entity may itself be merged later (`C → D → E`).
3. Return the first live (`deleted_at IS NULL`, `merged_into IS NULL`) entity reached.

**Cycle guard (required).** The chase carries a visited-set and a bounded hop limit. On a
revisited id or an exceeded limit, it fails loud with a distinct error (`redirect cycle detected`
/ `redirect chain too long`) rather than looping. A cycle is a data-integrity fault; the resolver
must surface it, not hang.

**Fork — how the caller learns the id moved.** Recommended: an **explicit redirect marker**.
`get(C)` returns the live kept entity, and the response carries `redirected_from: [C, …]` (the
chain of ids traversed). `resolve(C)` returns `Resolved { id: K, redirected_from: [C, …] }`. The
alternative — a **transparent return** that hands back `K` with no signal — is simpler but
silently changes what the caller believes exists: a caller that asked for `C` and gets an entity
named after `D`, with no indication the id moved, cannot distinguish "my id was stale" from "the
substrate is confused." The explicit marker is a small additive response field and keeps identity
changes caller-visible. Recommend explicit; final call is the reviewer's.

**Scope.** This decision covers the by-id READ entry points (`get`, `resolve`) — the exact surface
the dangling class touches, since a holder of an old id re-enters through them. Multi-record reads
(`list`, `neighbors`, `traverse`) already exclude tombstoned rows and see only the rewired-to-`D`
edges, so they carry no dangling reference. By-id WRITE paths that target a merged id (`link`,
`update`, `delete` on `C`) are deliberately left to a follow-up: redirecting a mutation interacts
with caller intent and is not required to close the read-dangling class. No schema migration is
needed — the pointer and provenance columns already exist.

### (b) An id-preserving edge endpoint-move primitive

Expose the id-preserving endpoint move that merge already performs, as a scoped runtime operation
(and a curation verb if a caller needs it directly):

`move_edge_endpoint(edge_id, new_source_id?, new_target_id?)`:

- Updates `source_id` and/or `target_id` in place, **preserving `edge_id`**, so any `annotates`
  note targeting the edge remains valid.
- Applies the same symmetric-relation canonicalization (`source_uuid < target_uuid`) and
  natural-key conflict handling that the merge transaction uses.
- **Re-validates the resulting `(source_kind, relation, target_kind)` against the ADR-002 base
  contract plus pack endpoint rules.** This is the safety addition over the raw internal `UPDATE`:
  merge preserves kind, so it never re-validates, but a split moves an endpoint onto a
  **different-kind** entity, which can produce an illegal endpoint pair. The move rejects an
  endpoint change that would violate the endpoint contract rather than writing an invalid edge.

A split is then composed from existing primitives plus this one: `create(D)` (fresh entity, new
UUID; `C` keeps its UUID untouched — a split never tombstones `C`), then `move_edge_endpoint` for
each edge whose semantics belong to `D`. Edge ids and their annotations survive throughout.

**Fork — primitive vs atomic annotates-rewire.** The alternative to exposing the primitive is a
higher-level split op that keeps delete-then-`link` for the moved edges but, in the same
transaction, re-points every orphaned `annotates` note's target from the old edge id to the new
one. Both preserve annotation targeting; they differ on whether edge ids survive. Re-minting ids
and chasing every annotation is more surface — it mutates note targets and must find all
annotators — and more to get wrong; preserving the edge id touches nothing downstream. Recommend
exposing the primitive; final call is the reviewer's.

## Rejected alternatives

- **Hard-delete on merge and rewrite all references.** Loses merge provenance, needs a global
  reference scan, and mutates stored data to fix a query result — a currency-rule violation
  (ADR-014).
- **Transparent redirect with no marker.** Silently changes which entity a caller's id denotes;
  the caller cannot tell a stale id from a substrate error. Retained as the non-recommended side
  of fork (a).
- **Split by delete-and-recreate only.** Re-mints edge ids and orphans `annotates` notes on those
  edges. Retained as the non-recommended side of fork (b).

## Consequences

### Positive

- Closes the merge-dangling class with no migration: the redirect pointer is already persisted;
  only the read path changes.
- Split preserves edge ids, so edge annotations survive a split with no note mutation.
- The `(old → kept)` mapping is materializable for auditing:
  `SELECT id AS old, merged_into AS kept FROM entities WHERE merged_into IS NOT NULL`.

### Negative

- `get` and `resolve` gain a bounded redirect chase (extra row loads, capped by the hop limit).
- The response shape of `get`/`resolve` grows an optional `redirected_from` field (additive;
  recommended fork).
- A new curation primitive expands the write surface and its endpoint-validation obligations.

### Neutral

- Pairs with an operating-rule procedure that owns body-text rewrite of raw-UUID mentions (the
  class a pointer cannot cover); this ADR provides the substrate mechanics that procedure depends
  on.

## Not covered (deliberate scope exclusions)

- Rewrite of opaque UUID mentions inside note, memory, or other free-text bodies — owned by the
  operating-rule layer as a pre-mutation body scan and rewrite.
- Redirect of by-id WRITE operations (`link` / `update` / `delete`) that target a merged id —
  named as a follow-up; it interacts with caller intent and is not needed to close the
  read-dangling class.
- The public verb composition of a split operation — this ADR specifies the substrate primitives;
  how a split verb sequences them is a consumer concern.

## References

- ADR-002 — edge ontology and the endpoint contract the move re-validates.
- ADR-007 — namespace and identifier resolution (full-UUID passthrough).
- ADR-014 — curation operations (merge) and the data-vs-view currency rule.
- ADR-055 — epistemic edges and edge annotation, the practice a re-minted edge id would orphan.
