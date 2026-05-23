# ADR-086: Cross-Backend Edge Representation — `target_backend` Column, Node Locator, Link Mechanics

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-002 (Edge Ontology — unchanged), ADR-022 (Schema Migrations), ADR-031
(Pack-Extensible Edge Endpoints), ADR-079 (Pack-Scoped Backends — defines backends)\
**Part of**: ADR-080 (SubstrateCoordinator umbrella)

## Context

ADR-079 introduced multiple `StorageBackend`s per khive deployment (e.g., `main.db` for hot data

- `lore.db` for cold corpus). Once a graph can span multiple physical SQLite files, edges that
  cross backends need a representation. The umbrella ADR-080 frames the problem; this ADR makes
  three tightly-coupled commitments that form the **cross-backend edge data model**:

1. How a cross-backend edge is stored on disk (the schema)
2. How the coordinator resolves a bare UUID to its hosting backend (the locator)
3. How `link(source_uuid, target_uuid, relation, weight)` writes a cross-backend edge

These three decisions are inseparable — the locator's purpose only makes sense in the context of
the column, and link()'s logic depends on both. They cluster as one ADR.

## Decision

### D2 — Edges store `target_backend` on the source's backend

The `graph_edges` table gains a new nullable column:

```sql
ALTER TABLE graph_edges ADD COLUMN target_backend TEXT NULL;
CREATE INDEX idx_graph_edges_target_backend
    ON graph_edges(target_backend)
    WHERE target_backend IS NOT NULL;
```

Semantics:

- `target_backend IS NULL` → target lives on the same backend as the source (the default for
  single-backend deployments; the value for every existing row at migration time).
- `target_backend = "<name>"` → target lives on the named backend (must match a declared
  `[[backends.name]]` from `khive.toml` per ADR-079).
- The edge row **always lives on the source's backend**. The target's backend is referenced by
  name; the target's backend does not receive a mirror row.

The existing unique constraint
`UNIQUE(namespace, source_id, target_id, relation)` is preserved — uniqueness is per
(source-namespace, source, target, relation), independent of where target lives. An edge from X
to Y with relation R is a single logical edge regardless of physical location.

Migration is purely additive (nullable column + WHERE-indexed partial index); applies via
ADR-022's `VersionedMigration` mechanism. Single-backend deployments inherit `target_backend IS
NULL` on every row and observe no behavioral change.

#### Why not RuVector's pure-locator approach

RuVector edges store only `(from_id, to_id)`; cross-shard locality is resolved at runtime by the
coordinator's node-to-shard map. We considered the same approach and rejected it. The decision
hinges on khive's operational shape, which differs from RuVector's:

| Property                                                          | Pure-locator            | `target_backend` column |
| ----------------------------------------------------------------- | ----------------------- | ----------------------- |
| Cold-start cross-backend traversal                                | needs locator warmup    | works immediately       |
| Isolated-backend introspection ("what edges leave this backend?") | impossible              | SQL query               |
| Observability metric (main→lore edge count)                       | requires full edge scan | SQL aggregate           |
| Storage cost                                                      | -1 column               | +1 nullable column      |
| Edge-stale risk if target moves                                   | none                    | bounded — see below     |

RuVector's shards are designed for graph operations (the locator is hot, fully populated, and
expected). khive's backends are **intentional isolation boundaries** — cross-backend edges are
exceptional, the locator (D3) is sparse and cold by default, and operators care about which
backend hosts which nodes for backup/restore reasons. Persisting locality on the edge row gives
properties the locator alone cannot: edge-driven introspection of a backend's outbound
references without coordinator involvement, and one-query observability of cross-backend edge
counts.

Edge-stale risk: a target_backend value becomes stale only if the target node migrates to
another backend, which is not an operation khive supports in v1 (no auto-partitioning;
operators move nodes only via explicit admin commands that update or rewrite cross-backend edge
rows). The risk is bounded to operator-initiated actions and is documented as such.

#### Why not oxigraph's named-graph approach

oxigraph stores `graph_name` on every quad (S, P, O, GraphName). That places logical isolation
inside one physical store. khive uses **both** layers:

- `namespace` column on every row — the equivalent of oxigraph's named graph; logical isolation
  within a backend.
- Multiple `[[backends]]` entries — physical isolation across SQLite files.

oxigraph's single-store-with-named-graphs and khive's per-backend-isolation are not
substitutes; they're two layers serving different concerns. The intentional-isolation model
(operator-declared backends) needs the physical layer that named-graphs alone cannot provide.

### D3 — Node locator is an in-memory lazy cache

The kernel coordinator (see ADR-080 umbrella) owns:

```text
Arc<DashMap<Uuid, BackendName>>
```

Semantics:

- **Populated on write**: every `create` of an entity or note records its UUID → owning-backend
  mapping in the cache.
- **Lazy on read**: a `locate(uuid)` call hits the cache first; on miss, the coordinator issues
  parallel reads to all backends and caches the first hit (or returns `None` if no backend
  contains the UUID).
- **Not persisted**: the cache is in-memory only. Across process restarts the cache is empty
  and warms lazily as queries arrive.

#### Why not persist the locator

Persistence would require a separate `node_locations` table on each backend with cross-backend
write coordination — every write to backend A would also write to a global index. The
complexity outweighs the benefit because:

1. The `target_backend` column (D2) is the persistent layer for **edge-driven** locality
   questions. The locator is only consulted for operations that take a bare UUID without edge
   context — `get(uuid)`, `update_entity(uuid, ...)`, `hard_delete_entity(uuid)`.
2. Substrate-kind search fans out to all backends anyway (ADR-087) so it does not consult the
   locator.
3. Cold-start traversal that does need locator data populates it as it walks — the first
   `traverse(root, depth=3)` after restart warms the working set.

Memory budget: 16 bytes (UUID) + ~16 bytes (backend name) ≈ 32 bytes per entry. 1M cached
entries ≈ 32 MB. Bounded by working set; an admin command may clear the cache if memory
pressure arises.

#### Cache invalidation rules

- **Hard-delete of node X**: invalidate `locator[X]` immediately; coordinator then walks all
  backends to remove incoming cross-backend edges (delegated to ADR-088's cascade logic).
- **Soft-delete**: no locator change — the node remains locatable; query layers filter by
  `deleted_at IS NULL`.
- **Hard-delete batch**: coordinator invalidates locator entries in bulk before issuing
  per-backend deletes.
- **Process restart**: cache empties; lazy repopulation as queries arrive.

#### Open question — eviction policy

The cache is currently unbounded. For deployments with > 10M nodes, memory pressure matters.
Likely resolution: bounded LRU with operator-configurable cap (default ~1M entries). Defer to a
follow-up ADR if a deployment hits this in practice.

### D10 — `link()` is coordinator-driven

The caller of `link(source_uuid, target_uuid, relation, weight)` passes UUIDs only — no
backend hints. The kernel coordinator:

1. Resolves source's backend via `locate(source_uuid)`. On miss, parallel-fetch fallback.
   Failure → `UnknownNode(source_uuid)`.
2. Resolves target's backend via `locate(target_uuid)`. Same fallback. Failure →
   `UnknownNode(target_uuid)`.
3. Validates the (source_kind, relation, target_kind) tuple against ADR-002's base ontology
   and ADR-031's pack-extensible rules. Violation → `EdgeRuleViolation`.
4. Writes the edge on **source's backend**:
   - Same backend: `target_backend = NULL` (the local-edge default).
   - Different backends: `target_backend = "<target_backend_name>"`.
5. The target's backend is not touched at write time. Cross-backend edges become visible to
   the target's neighbors-incoming query only via coordinator-driven fan-out (see ADR-088).
6. Increments the cross-backend edge counter (see ADR-080 umbrella) if cross-backend.

The unique constraint and pack-extensible endpoint rules remain authoritative — D10 changes the
backend resolution path, not the validation contract.

#### Reserved error variant

`CrossBackendDisallowed(relation)` exists in the coordinator's error enum to allow future
tightening if any relation must remain backend-local. No relation currently triggers this; the
variant is a forward-compatibility hook, not an active rule. If implementation drops this
reservation, an explicit ADR amendment will reinstate it.

## Storage layer surface

Two additive type evolutions:

```rust
// khive-storage/src/types.rs
pub struct Edge {
    pub id: LinkId,
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub relation: EdgeRelation,
    pub weight: f64,
    pub created_at: DateTime<Utc>,
    pub metadata: Option<serde_json::Value>,
    pub target_backend: Option<String>,    // NEW
}

pub struct NeighborHit {
    pub node_id: Uuid,
    pub edge_id: Uuid,
    pub target_backend: Option<String>,    // NEW
}
```

Both fields default to `None` — single-backend callers see no semantic change.

## Alternatives considered

### A. Pure-locator (RuVector style)

Drop the `target_backend` column entirely; coordinator's `DashMap` is the only source of
locality. Rejected: see "Why not RuVector's pure-locator approach" above. Operational
requirements (cold-start, observability, intentional-isolation introspection) reject this for
khive even though it works for RuVector.

### B. Named-graph only (oxigraph style)

One large `Store`; the `namespace` column carries logical isolation. Rejected by ADR-079
already — physical isolation (separate SQLite files) is the user-visible feature this ADR's
parent introduces.

### C. Two-sided storage (mirror cross-backend edges on both backends)

Store the edge on both source's and target's backends so neighbors-incoming queries can be
fully local. Rejected: doubles cross-backend edge storage; introduces a non-atomic two-backend
write on link(); raises consistency questions on delete cascade. The fan-out at neighbors-In
time (ADR-088) is acceptable cost.

### D. Edge ID encodes target backend

Make `LinkId` carry a backend prefix so target locality is in the ID. Rejected: makes IDs
opaque, breaks ID stability across backends, conflicts with UUID semantics.

## Consequences

### Positive

- Cross-backend edges have a clear, queryable representation
- Cold-start cross-backend traversal works without locator warm-up
- Per-backend isolated introspection ("what does main link to?") is a SQL query
- Single-backend deployments observe zero behavioral change (NULL on every row)
- Locator cost is bounded — operators with one backend pay nothing
- Edge cascade (ADR-088) can use the counter index for efficient incoming-edge discovery

### Negative

- Schema migration required — but additive (nullable column); ADR-022's mechanism handles it
- Edge-stale risk if a node migrates between backends — bounded by no-auto-migration policy
- Locator memory budget grows with working set — bounded but unmonitored in v1

### Neutral

- ADR-002's 13-relation ontology is unchanged — `target_backend` is row metadata, not a
  relation
- ADR-031's edge endpoint rules apply identically; D10 consults the same validator
- Single-backend deployments inherit the migration but never exercise the cross-backend path

## Open Questions

1. **Bounded vs. unbounded locator cache.** Default unbounded for v1; LRU with cap if a
   deployment exceeds ~10M cached UUIDs.
2. **Stable backend IDs vs. names.** `target_backend` references operator-defined strings.
   Renaming a backend orphans existing values. Mitigation v1: document that backend renames
   require a migration script. Future: a stable `backend_id` UUID alongside names.
3. **CrossBackendDisallowed concrete trigger.** Reserved error variant has no current rule.
   Either commit to a concrete rule or drop the variant until the design exists.

## References

- ADR-002 — Edge ontology (unchanged)
- ADR-022 — Schema migration mechanism used for the `target_backend` column add
- ADR-031 — Pack-extensible edge endpoints; consulted by D10's validation step
- ADR-079 — Backends declared and instantiated here
- ADR-080 — Umbrella ADR motivating cross-backend operations
- ADR-087 — Substrate-kind federated search (separate concern)
- ADR-088 — Traversal and curation semantics that use this ADR's column + locator
- RuVector `crates/ruvector-graph/src/edge.rs` — pure-locator alternative (rejected in §A)
- RuVector `crates/ruvector-graph/src/distributed/coordinator.rs` — coordinator shape adopted
- oxigraph `lib/oxigraph/src/sparql/dataset.rs` — named-graph alternative (rejected in §B)
