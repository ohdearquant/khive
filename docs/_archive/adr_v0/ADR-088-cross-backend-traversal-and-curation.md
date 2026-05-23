# ADR-088: Cross-Backend Traversal and Curation Semantics

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-014 (Curation Operations — single-backend baseline), ADR-002 (Edge
Ontology), ADR-086 (Cross-Backend Edge Representation), ADR-079 (Pack-Scoped Backends)\
**Part of**: ADR-080 (SubstrateCoordinator umbrella)

## Context

ADR-086 establishes the cross-backend edge data model (`target_backend` column + locator +
coordinator-driven `link()`). ADR-087 establishes how substrate-kind reads federate. This ADR
covers the remaining cross-backend operations — **graph traversal and entity curation**.

These two concerns cluster because they both operate over the cross-backend edge structure
ADR-086 defines:

- Traversal **reads** the edge graph (BFS following outgoing/incoming edges).
- Curation **mutates** entity rows and incident edges (`update_entity`, `merge_entity`,
  `hard_delete`).

ADR-014 specifies single-backend curation semantics. This ADR extends those semantics to the
multi-backend case where the operation may cross backend boundaries.

## Decision

### D9 — Traversal works across backends transparently

`coordinator.traverse(roots, options)` is BFS where each `neighbors()` call is the unit
operation:

1. Locate the current node's backend via the locator (ADR-086 D3) or parallel-fetch fallback.
2. Read outgoing edges from that backend, including their `target_backend` field.
3. For each neighbor:
   - `target_backend = NULL` (or matches source's backend) → neighbor lives on the same backend
     → resolve locally.
   - `target_backend = "<name>"` → coordinator fetches the neighbor node from the named backend.
4. Continue BFS with the resolved neighbors.

Pack handlers do not see the boundary. They call `coordinator.traverse(...)` and receive a
unified `Vec<PathNode>` where each node knows the backend it came from (for observability;
ignored by simple consumers).

#### Incoming-direction neighbors require full fan-out

`neighbors(uuid, direction=Out)` reads outgoing edges from one backend (source's). Cross-backend
targets are dereferenced via `target_backend`. Cost: O(1) backend touch for the source + at
most one extra touch per cross-backend target.

`neighbors(uuid, direction=In)` is asymmetric. Incoming edges to `uuid` may originate from any
backend — there is no single "incoming side" backend the way there is a single source backend.
The coordinator must **fan out across all backends** and union the matching rows where
`target_id = uuid AND (target_backend = "<uuid's backend>" OR (target_backend IS NULL AND we
queried uuid's own backend))`.

This is a known cost of the source-side edge storage choice (ADR-086 D2). The alternative —
mirror cross-backend edges on both backends — was rejected in ADR-086 §C as worse on every
other axis.

#### Performance characteristics

- Same-backend hop: identical to current single-backend traversal (one local `neighbors` call).
- Cross-backend hop (outgoing): one extra in-process backend touch, no network — backends are
  `Arc<StorageBackend>` siblings.
- Locator misses on the first cross-backend hop trigger parallel-fetch once; cached thereafter
  (ADR-086 D3).
- Incoming-direction queries are O(N backends) per node; bounded by `max_depth` and pruned by
  visited-set deduplication.

### D11 — Curation semantics for cross-backend operations

**`update_entity(uuid, patch)`** — works across backends.

1. Coordinator locates the entity's backend via locator (or parallel-fetch fallback).
2. Routes the patch to that backend's `KhiveRuntime`.
3. Single-row write inside the entity's backend's SQLite transaction.

Cross-backend update is identical to local update from the caller's perspective. No coordinator
transaction; the entity's backend's WAL is the atomic boundary. Caller's only observable
difference from single-backend: a locator miss may add latency on the first call.

**`merge_entity(into_id, from_id)`** — errors when cross-backend.

- Same backend: standard merge per ADR-014 (move rows + merge incident edges, delete from_id).
- Different backends: `CrossBackendMergeUnsupported { into_backend, from_backend }`.

Merge requires moving entity rows between tables and re-pointing every incident edge. A
cross-backend merge would need a 2PC protocol or a coordinator compensation log — both are scope
for a future ADR. Operators with the use case can manually export `from_id`, delete it, and
re-import on `into_id`'s backend.

**`hard_delete_entity(uuid)`** — coordinator cascades incoming cross-backend edges.

1. Coordinator invalidates `locator[uuid]` (ADR-086 D3).
2. The entity's backend executes its local hard-delete:
   - Delete entity row.
   - Cascade: delete outgoing edges (`source_id = uuid`).
3. Coordinator iterates over the cross-backend edge counter (ADR-086) to identify backends
   with edges into `uuid`.
4. For each such backend B: coordinator issues a delete on B's edges where
   `target_id = uuid AND target_backend = "<uuid's backend>"`.
5. Decrement cross-backend edge counters accordingly.

The cascade is **non-atomic across backends**. Each backend's delete is its own SQLite
transaction. If a backend is unreachable mid-cascade (per ADR-089), the cascade may complete
partially:

- The entity's row is gone (step 2 succeeded).
- Some other backend's incoming edges may remain, now dangling.
- Dangling edges are filtered at query time (neighbors resolution drops edges whose target
  cannot be fetched) and can be cleaned by a future `kkernel db cleanup` admin command.

This is the trade-off: at-most-one-backend's atomicity per step, in exchange for no cross-backend
2PC complexity. Operators wanting stricter guarantees should keep dependent data on the same
backend.

## Operation matrix — cross-backend semantics for every verb

This table is the **normative summary** of cross-backend behavior for the substrate verbs. It
references the decisions in ADR-086, ADR-087, ADR-088, ADR-089. Granular-kind verbs
(`assign`, `complete`, `recall`, `remember`, etc.) stay pack-local per ADR-079 D6 and do not
appear here.

| Operation                      | Same-backend                      | Cross-backend                                                                            | Reference            |
| ------------------------------ | --------------------------------- | ---------------------------------------------------------------------------------------- | -------------------- |
| `create(kind=entity, ...)`     | local write                       | N/A — pack determines backend                                                            | ADR-079 D6           |
| `create(kind=note, ...)`       | local write                       | N/A — pack determines backend                                                            | ADR-079 D6           |
| `link(a, b, rel)`              | local edge                        | edge on a's backend with `target_backend = b's backend`                                  | ADR-086 D10          |
| `get(uuid)`                    | local read                        | local read after locator hit                                                             | ADR-086 D3           |
| `update_entity(uuid, patch)`   | local write                       | local write after locator hit                                                            | D11 (this ADR)       |
| `merge_entity(a, b)`           | local merge per ADR-014           | **error** `CrossBackendMergeUnsupported`                                                 | D11 (this ADR)       |
| `delete(uuid, soft)`           | local mark                        | local mark after locator hit                                                             | locator stays valid  |
| `delete(uuid, hard)`           | local delete + local edge cascade | local delete + **coordinator cascades incoming cross-backend edges across all backends** | D11 (this ADR)       |
| `neighbors(uuid, Out)`         | local query                       | local edges + cross-backend target resolution                                            | D9 (this ADR)        |
| `neighbors(uuid, In)`          | local query                       | **fan-out across all backends**                                                          | D9 (this ADR)        |
| `traverse(roots, depth)`       | local BFS                         | BFS following local + cross-backend edges                                                | D9 (this ADR)        |
| `search(kind=note)`            | local search                      | fan-out + unweighted RRF                                                                 | ADR-087 D4           |
| `search(kind=task)` (granular) | local                             | N/A — task is pack-owned, single backend                                                 | ADR-026 / ADR-079 D6 |

## Single-backend default behavior

For a deployment with one `[[backends.main]]` entry:

- D9 traversal: all edges have `target_backend = NULL`, all neighbors resolve locally;
  identical to pre-ADR-088 single-backend behavior.
- D11 `update_entity`: locator lookup hits one backend; identical.
- D11 `merge_entity`: same-backend case always; no cross-backend error ever raised.
- D11 `hard_delete_entity`: cross-backend cascade step iterates zero times; identical.

Coordinator is zero-cost on single-backend deployments.

## Alternatives considered

### A. Disallow cross-backend traversal

`traverse()` errors when an edge has `target_backend != NULL`. Caller must manually orchestrate.
Rejected: defeats the unified-graph model. Cross-backend traversal is the most common reason to
have a coordinator in the first place.

### B. Mirror writes so neighbors-In is local

Store cross-backend edges on both source and target backends so incoming queries don't need
fan-out. Rejected: doubles cross-backend edge storage; introduces non-atomic two-backend writes
on link(); cascade on delete is more complex. The neighbors-In fan-out is the correct cost.

### C. Support cross-backend merge via 2PC

Implement a real two-phase commit for `merge_entity` across backends. Rejected for v1:
substantial complexity (write-ahead log, compensation, partial-failure recovery) for a rarely
needed operation. Operator-side export/import handles the use case.

### D. Coordinator-level transaction log for cascade idempotency

Persist a log of cross-backend cascade operations so they can be replayed on failure. Rejected
for v1: most operators will tolerate dangling edges (cheap to clean up via admin command); the
log adds write-amplification on every hard_delete. Revisit if operational evidence shows the
issue.

## Consequences

### Positive

- `traverse(root, depth)` returns a unified graph view across backends
- `update_entity` works regardless of where the entity lives
- Pack handlers stay single-backend — the coordinator owns boundary crossing
- Cascade is best-effort but predictable; dangling edges are filtered at query time
- Operators with consistency-critical workloads can keep dependent data on one backend

### Negative

- **Cross-backend hard_delete is not atomic** — partial failure leaves dangling edges.
  Mitigation: filtering at query time + admin cleanup command (future).
- **Cross-backend merge is unsupported in v1.** Workaround: manual export/import.
- **Incoming neighbors are O(N backends)** per node. Bounded by backend count and visited-set
  pruning.

### Neutral

- ADR-014's single-backend curation semantics are preserved (D11 explicitly defers to
  ADR-014 when same-backend)
- ADR-002's 13 relations are unchanged
- Edge cascade logic adds counter-driven incoming-edge discovery — fast for the common case of
  zero cross-backend edges

## Open Questions

1. **Cascade idempotency on partial failure** — should a future ADR specify a retryable cascade
   protocol (e.g., per-backend "pending delete" markers)? Defer until operational evidence
   shows dangling edges are a real problem.
2. **Operation matrix as standalone reference** — should this matrix live in its own appendix
   document so all four split ADRs can cite it without duplicating, or stay normative in this
   ADR? v1: keep here as normative; cite from other split ADRs.
3. **`CrossBackendMergeUnsupported` upgrade path** — when (if ever) does khive add cross-backend
   merge? Tied to coordinator-level transaction support; out of scope for v1.

## References

- ADR-002 — Edge ontology (unchanged)
- ADR-014 — Single-backend curation operations (this ADR extends to multi-backend)
- ADR-019 — Substrate vs granular kinds
- ADR-026 — GTD pack (task is granular, single-backend)
- ADR-036 — Memory pack (memory is granular, single-backend)
- ADR-079 — Backends declared here
- ADR-080 — Umbrella
- ADR-086 — Edge representation + locator + link mechanics (this ADR builds on)
- ADR-087 — Substrate-kind search (separate read path)
- ADR-089 — Partition tolerance (affects cascade behavior under backend unavailability)
- RuVector `distributed/coordinator.rs` — pattern for shard-style BFS
