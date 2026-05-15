# ADR-014: KG Curation Operations — Update, Merge, Edge CRUD

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

The current OSS khive surface supports entity creation, deletion, listing, edge creation (`link`),
and graph navigation (`neighbors`, `traverse`). What's missing is the _curation_ surface — the
operations agents need to maintain a knowledge graph over time as they learn more:

- Update an entity's name, description, properties, or tags after creation.
- Update an edge's relation or weight.
- Merge two entities into one (deduplication when an agent realizes "Concept A" and "Concept A v2"
  are the same thing).
- Look up a specific edge by id, list edges by filter, delete edges.

Without these, agents can only _grow_ a KG. They can't _correct_ or _refine_ it. That's a critical
gap for the "research KG that grows with your work" use case.

Storage layer note: the `khive-storage` traits (`EntityStore`, `GraphStore`) already expose all the
primitive operations — upsert, get, delete, query, count. This ADR is about runtime composition (the
higher-level operations) and MCP exposure (the agent surface).

## Decision

Add **seven runtime operations** (in `khive-runtime`) for KG curation. These are exposed to agents
through the verb-consolidated MCP surface defined in ADR-023 (`update`, `merge`, `get`, `list`,
`delete` with `kind=` discriminant). Specifications below.

### Runtime operations (impl KhiveRuntime)

```rust
/// Patch-style update — None fields leave the existing value unchanged.
/// Any change to name or description re-indexes FTS5 and (if model configured) vector store.
pub async fn update_entity(
    &self,
    namespace: Option<&str>,
    id: Uuid,
    patch: EntityPatch,
) -> RuntimeResult<Entity>;

pub struct EntityPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,    // Some(None) clears, None leaves unchanged
    pub properties: Option<serde_json::Value>,  // wholesale replace if Some
    pub tags: Option<Vec<String>>,              // wholesale replace if Some
}

/// Merge `from_id` into `into_id`: rewires all edges incident to from_id so they
/// reference into_id, merges properties per strategy, hard-deletes from_id.
pub async fn merge_entity(
    &self,
    namespace: Option<&str>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: MergeStrategy,
) -> RuntimeResult<MergeSummary>;

pub enum MergeStrategy {
    /// Default. `into` values win on conflict. Tags are unioned. Properties from
    /// `from` are added where `into` doesn't have them.
    PreferInto,
    /// `from` values win on conflict (rare — only when caller knows from is more recent).
    PreferFrom,
    /// Deep-merge: object properties merge recursively (object union), conflicts go to `into`.
    Union,
}

pub struct MergeSummary {
    pub kept_id: Uuid,
    pub removed_id: Uuid,
    pub edges_rewired: usize,
    pub properties_merged: usize,
    pub tags_unioned: usize,
}

/// Fetch a single edge by id.
pub async fn get_edge(
    &self,
    namespace: Option<&str>,
    edge_id: Uuid,
) -> RuntimeResult<Option<Edge>>;

/// List edges in a namespace with filtering. limit defaults to 100, max 1000.
pub async fn list_edges(
    &self,
    namespace: Option<&str>,
    filter: EdgeListFilter,
    limit: u32,
) -> RuntimeResult<Vec<Edge>>;

pub struct EdgeListFilter {
    pub source_id: Option<Uuid>,
    pub target_id: Option<Uuid>,
    pub relations: Vec<String>,        // empty = any
    pub min_weight: Option<f64>,
    pub max_weight: Option<f64>,
}

/// Patch-style edge update. None fields leave existing values unchanged.
pub async fn update_edge(
    &self,
    namespace: Option<&str>,
    edge_id: Uuid,
    relation: Option<String>,
    weight: Option<f64>,
) -> RuntimeResult<Edge>;

/// Hard-delete an edge by id.
pub async fn delete_edge(
    &self,
    namespace: Option<&str>,
    edge_id: Uuid,
) -> RuntimeResult<bool>;

/// Count edges matching a filter (for pagination + UI).
pub async fn count_edges(
    &self,
    namespace: Option<&str>,
    filter: EdgeListFilter,
) -> RuntimeResult<u64>;
```

### MCP exposure

The runtime operations above are surfaced to agents via the verb-consolidated tools in ADR-023:

| Runtime op      | MCP verb invocation                                |
| --------------- | -------------------------------------------------- |
| `update_entity` | `update(kind="entity", id, patch...)`              |
| `merge_entity`  | `merge(kind="entity", into_id, from_id, strategy)` |
| `get_edge`      | `get(kind="edge", id)`                             |
| `list_edges`    | `list(kind="edge", filter...)`                     |
| `update_edge`   | `update(kind="edge", id, relation?, weight?)`      |
| `delete_edge`   | `delete(kind="edge", id)`                          |

`count_edges` is library-only — agents call `list(kind="edge")` with a high `limit` if they need
counts. Re-evaluate if a real agent workflow needs it.

## Semantics

### Patch vs replace

`update_entity` and `update_edge` are **patch**: only fields present in the request modify state.
Absent fields are unchanged.

For `description` specifically: the Rust type is `Option<Option<String>>`. `None` (outer) = leave
alone; `Some(None)` = clear; `Some(Some(s))` = set to `s`. The MCP JSON schema models this as: omit
the key = leave alone, `null` = clear, string = set.

For `properties` and `tags`: wholesale replace if present. To merge properties incrementally, the
agent fetches first, modifies, then sends the full new value. This keeps the API simple; "set one
property without touching the others" is rare in practice for an agent that already has the entity
in context.

### Merge entity — what gets merged

`merge_entity(into, from, strategy)`:

1. Fetch both entities. If either is missing, error.
2. Query all edges incident to `from_id` (as source or target).
3. For each edge, rewrite to reference `into_id` instead of `from_id`. If the rewrite would create a
   self-loop (e.g., A `extends` B; merge B into A → A `extends` A), drop that edge.
4. Compute merged properties per `strategy`. Compute unioned tags. Compute the merged `name` and
   `description` per strategy: `PreferInto` keeps `into`'s; `PreferFrom` takes `from`'s; `Union`
   keeps `into`'s name but appends `from`'s description if `into`'s is empty.
5. Upsert the updated `into` entity. Re-index FTS5 + vector store.
6. Hard-delete `from`. Remove `from` from FTS5 and vector store.
7. Return `MergeSummary` with counts.

The operation is **not** transactional across `from` and `into` in v0.1 — if the process dies
between steps 3 and 7, the KG has rewired edges but both entities still exist. Future versions can
wrap in a transaction; v0.1 documents this and is idempotent enough that re-running the merge with
the same args succeeds (the rewires are no-ops, the delete of `from` succeeds if it still exists).

### Auto-indexing on update

When `update_entity` changes `name` or `description` (in any way), the runtime must:

- Re-upsert the entity's `TextDocument` into the FTS5 index with the new body.
- If `embedding_model` is configured, re-embed the new body and replace the entity's vector in the
  vector store.

If `name` and `description` are unchanged (only properties or tags changed), skip the re-indexing —
properties and tags aren't part of the indexed body in v0.1.

### Deletion and indexes

Current `delete_entity` is soft-delete by default. v0.1 keeps soft-deleted entities in FTS5 + vector
store. Their queries don't filter them out yet — this means soft-deleted entities can still appear
in hybrid_search. **This is a known v0.1 limitation** to be fixed in a follow-up; documented here so
it's not a surprise. For v0.1, prefer hard-delete (`hard: true`) when removing entities permanently.

`delete_edge` is always hard — edges don't have a soft-delete state.

## Rationale

### Why patch semantics

PATCH is the natural shape for "make this change to this thing". Replacing the entire entity on
every update forces the agent to fetch first — which costs a round-trip and risks lost updates if
the agent's snapshot is stale. PATCH lets the agent express _intent_ directly.

### Why merge_entity at the runtime layer (not storage)

Merging is composition: it touches multiple stores (entities, edges, FTS5, vectors) and applies
semantic rules (rewiring, conflict resolution). That's a runtime concern. The storage layer stays
primitive — it just knows how to upsert, get, delete.

### Why `get(kind="edge")` and `list(kind="edge")` are MCP-exposed

Agents need to inspect specific edges (e.g., "what's the weight of this relation?") and enumerate
edges (e.g., "what does FlashAttention depend on?"). Without these, agents can only access edges via
`neighbors` or `traverse`, which return summaries. Exposing get+list directly closes the gap.

### Why no bulk create/link at MCP

A bulk tool would add complexity (partial-failure semantics, per-item error reporting) without a
clear agent win. Agents needing batch use the generic `request` verb (ADR-020), which composes
multiple ops in one call.

### Why count_edges is library-only

`list(kind="edge")` with a generous `limit` covers the common case. A separate `count` MCP tool
would force agents to choose between two tools where one suffices.

## Alternatives Considered

| Alternative                                                          | Pros                    | Cons                                                                                                         | Why rejected                  |
| -------------------------------------------------------------------- | ----------------------- | ------------------------------------------------------------------------------------------------------------ | ----------------------------- |
| Wholesale PUT on `update(kind="entity")` (full replace)              | Simpler semantics       | Forces fetch-first, lost-update races                                                                        | PATCH wins                    |
| Auto-merge entities via embedding similarity                         | Less agent work         | Wrong calls are unfixable; ambiguous on policy                                                               | Defer; orthogonal to curation |
| `set_property(key, value)` / `unset_property(key)` as separate tools | Surgical                | API bloat; wholesale replace via `update(kind="entity")` covers it                                           | Skip                          |
| Soft-delete for edges too                                            | Symmetric with entities | Edges don't have an identity worth recovering; complicates queries                                           | Skip                          |
| `merge_entity` requires transactional storage                        | Atomicity               | SQLite trait surface doesn't expose multi-step transactions cleanly; v0.1 ships idempotent non-transactional | Defer                         |

## Consequences

### Positive

- Agents can correct mistakes, dedupe, refine the KG — the core curation loop is complete.
- Edge-by-id access closes a real gap in the existing surface.
- Index consistency is maintained automatically on update (matches the existing auto-index-on-create
  behavior).

### Negative

- `merge_entity` is not transactional in v0.1 — partial failure leaves the KG inconsistent until
  re-run.
- Soft-deleted entities still appear in `search(kind="entity")` results until the follow-up filter
  work lands.
- `EntityPatch` with `Option<Option<String>>` for `description` is awkward in some serde mappings;
  the MCP layer translates JSON `null` → `Some(None)` explicitly.

### Neutral

- The 8 runtime operations route through the existing verb-consolidated MCP surface (ADR-023) — no
  growth in tool count.

## Implementation Plan

**Track A — Entity curation** (`crates/khive-runtime/src/curation.rs`):

- `update_entity`, `merge_entity`
- Routed via MCP `update(kind="entity")` and `merge(kind="entity")` per ADR-023.

**Track B — Edge CRUD** (extends `crates/khive-runtime/src/operations.rs`):

- `get_edge`, `list_edges`, `update_edge`, `delete_edge`, `count_edges`
- Routed via MCP `get(kind="edge")` / `list(kind="edge")` / `update(kind="edge")` /
  `delete(kind="edge")` per ADR-023.

Both tracks update tests and the integration test assertions.

## Open Questions

1. **Merge audit trail**: should `merge_entity` record a Note/Event with the merge history (which
   entities were merged, when, by whom)? Defer — depends on the versioning model in ADR-015.
2. **Conflict surface for `Union` strategy**: when properties conflict in the deep-merge, do we
   surface the conflicts to the caller or silently apply "into wins"? v0.1: silent; surface in v0.2
   if agents need it.
3. **Edge-property updates**: edges have a `properties` field in some schemas. v0.1 `update_edge`
   only patches `relation` and `weight`. Extend later if edge-property use cases emerge.

## References

- ADR-001: Entity Kind Taxonomy
- ADR-002: Closed Edge Ontology (relation values still restricted to the 13 canonical set;
  `update_edge` validates)
- ADR-005: Storage Capability Traits (`EntityStore`, `GraphStore` — the primitives this composes on)
- ADR-010: KG Versioning Direction (planned; curation is the precondition for versioning)
- ADR-015: KG Versioning Model (planned; builds on the operations defined here)
- ADR-023: Verb-Consolidated MCP Surface (the agent-facing names for these operations)
