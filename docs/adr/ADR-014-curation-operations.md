# ADR-014: Curation Operations

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

A knowledge graph that only grows is incomplete. Agents need to correct names,
refine descriptions, dedupe duplicate entities, retype edges, and remove records
that no longer reflect current understanding. Without curation, a KG accumulates
noise faster than signal.

Curation operations are the runtime-level methods that transform existing records
in-place (or in semantically-aware composite operations like merge). They sit
between primitive storage operations (`upsert_entity`, `delete_edge`) and the
agent-facing verb surface (`update`, `merge`, `delete`).

The curation surface must satisfy:

1. **Patch semantics, not replace.** Updates express intent (change this field)
   rather than forcing fetch-modify-write round trips.
2. **Validation chain.** entity_type, edge endpoint legality, note kind, and
   namespace ownership all validate before persistence. Curation goes through the
   same validation as creation.
3. **Index consistency.** Updates that change indexed fields (name, description,
   content, or other embedding source text) persist the record and then immediately
   await reindexing in the same runtime operation. FTS5 is updated through the text
   store, and vector storage is updated only when an embedding model is configured.
   The shipped update path does not create an outbox or `pending_reindex` record;
   durable retry queues are deferred to a future ADR.
4. **Substrate-aware merge.** Merging two entities rewires their edges. Merging
   two notes rewires their `annotates` edges and `supersedes` chain. Edges
   themselves don't merge.
5. **Single-backend execution.** Curation runs in one backend transaction.
   Cross-backend merge is not supported; it returns an error.
6. **History-preserving where it matters.** Supersession (history-preserving)
   and merge (destructive) are different operations with different semantics.

## Decision

### Data vs view (curation guardrail)

**Data vs view (curation guardrail).** Curation verbs (`update`, `delete`, `merge`) are tools for **deliberate correction** of stored records. They are not mechanisms for hiding stale-but-historical data from query results. The "don't show stale / superseded / non-current info" pattern is always a **view-layer filter** problem — never a reason to delete, mutate, copy, or transfer data. `supersedes` (ADR-002) is the canonical mechanism for marking superseded data without losing it; queries filter superseded records at the view layer. Use curation only when a stored record is actually **wrong** (factual error, schema migration, deduplication), never to clean up "noisy" but historically valid data.

### Five curation verbs, all routed through `update` / `merge` / `delete`

The agent-facing surface from ADR-016 (Request DSL) routes curation through five
verbs:

| Verb                                                    | Substrates         | Effect                           |
| ------------------------------------------------------- | ------------------ | -------------------------------- |
| `update(kind=..., id=..., ...)`                         | entity, edge, note | Patch-style field updates        |
| `merge(kind=..., into_id=..., from_id=..., policy=...)` | entity, note       | Deduplicate two records into one |
| `delete(kind=..., id=..., hard?=...)`                   | entity, edge, note | Soft (default) or hard delete    |
| `link(source_id=..., target_id=..., relation=...)`      | edges (create)     | Create a typed edge              |
| `get/list(kind="edge", ...)`                            | edges              | Inspect existing edges           |

Edges have no `merge` operation. Two edges sharing the same
`(namespace, source, target, relation)` are deduplicated at insert time by the
unique-triple constraint (ADR-002 symmetric canonicalization + ADR-009 upsert
semantics). Edge curation is `update_edge` + `delete_edge`.

### Patch-style updates

Entity, edge, and note updates use patch semantics. Only `Some(_)` fields modify
state; absent fields are unchanged. The `description` field uses
`Option<Option<String>>` to distinguish "leave unchanged" from "clear":

```rust
pub struct EntityPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub entity_type: Option<Option<String>>,
    pub properties: Option<Value>,
    pub tags: Option<Vec<String>>,
}

pub struct EdgePatch {
    pub relation: Option<EdgeRelation>,
    pub weight: Option<f64>,
    pub properties: Option<Value>,
}

pub struct NotePatch {
    pub name: Option<Option<String>>,
    pub content: Option<String>,
    pub salience: Option<Option<f64>>,
    pub decay_factor: Option<Option<f64>>,
    pub properties: Option<Value>,
    pub kind_status: Option<String>,
}
```

For `properties`, shipped update semantics are a top-level merge with patch values
winning at patched keys. Top-level keys absent from the patch are preserved; if a
patched key contains a nested object, that nested value replaces the previous value
at the same key rather than recursively merging. To remove a property key, the
caller must pass a complete `properties` JSON object reflecting the desired
post-state (or use a future `unset` mechanism). For `tags`, semantics are
**replace**: `Some(vec)` sets tags to exactly `vec`.

The MCP JSON wire layer translates `null` → `Some(None)` (clear) and absent key →
`None` (leave alone). Tests in `khive-mcp` cover the boundary mapping.

### Validation chain on updates

`update_entity` runs through the same validation chain as `create_entity`:

```text
1. Fetch existing record from storage.
2. Verify namespace ownership (NamespaceToken comparison, ADR-007).
3. Apply patch fields.
4. If entity_type changed:    normalize via EntityTypeRegistry (ADR-001).
5. If kind changed:           reject — kind is immutable on entities.
6. Persist via upsert_entity.
7. If indexed fields changed: re-index FTS5 + vector store.
```

`update_edge` validates:

```text
1. Fetch existing edge from storage.
2. Verify namespace ownership.
3. Apply patch fields.
4. If relation changed: validate new (source_kind, relation, target_kind) endpoint
   triple against ADR-002 base rules + pack-registered rules (ADR-017, Pack Standard `const EDGE_RULES`).
5. Persist via upsert_edge with DO UPDATE semantics (ADR-009).
```

`update_note` validates:

```text
1. Fetch existing note from storage.
2. Verify namespace ownership.
3. Apply patch fields.
4. If kind_status changed: validate transition against NoteKindSpec lifecycle
   (ADR-004, e.g., GTD's inbox→next→active→done).
5. If kind changed: reject — kind is immutable on notes.
6. Persist via upsert_note.
7. If content changed: re-index FTS5 + vector store.
```

Validation failures return descriptive errors with the offending field, expected
values, and the rejection reason. Storage is never partially mutated on
validation failure — the runtime validates before any storage call.

### Index consistency

When `update_entity` or `update_note` changes indexed fields (name, description,
content), the runtime persists the record and then immediately re-indexes FTS5
and vector storage in the same awaited runtime operation. The reindex uses the
authorized namespace token for the record path to prevent cross-namespace pollution.
Property-only or tag-only updates skip reindexing because v1 does not index
properties or tags as FTS body content.

### `EntityDedupMergePolicy`: the entity merge strategy

Entity merge has its own policy enum, deliberately named `EntityDedupMergePolicy`
to avoid collision with KG versioning's `SnapshotMergeStrategy` (ADR-010):

```rust
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntityDedupMergePolicy {
    /// Default. `into` values win on conflict. Tags are unioned. Properties from
    /// `from` fill in keys that `into` doesn't have.
    #[default]
    PreferInto,
    /// `from` values win on conflict.
    PreferFrom,
    /// Deep-merge: object properties merge recursively. Scalar conflicts go to `into`.
    Union,
}
```

The two `MergeStrategy` names previously used (curation merge and snapshot merge)
now live in different namespaces with explicit names. No naming overload between
substrate dedup and graph versioning.

### `content_strategy`: description merge is independent of `policy` (amendment, #778/#814)

`policy` (`EntityDedupMergePolicy`) governs `name`, `properties`, and `tags`.
Description merge is governed by its own parameter, `content_strategy`
(`ContentMergeStrategy`, defined in ADR-039 for note merge and shared here):

```rust
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentMergeStrategy {
    /// Default. Concatenates `into`'s and `from`'s descriptions with a
    /// `\n\n---\n\n` separator. Lossless — the only default that never
    /// discards content.
    #[default]
    Append,
    /// Keeps `into`'s description unchanged; `from`'s description is discarded.
    PreferInto,
    /// Replaces `into`'s description with `from`'s description.
    PreferFrom,
}
```

`content_strategy` is selected directly — it is **not** derived from `policy`.
An explicit `content_strategy=prefer_from` takes effect even when `policy`
stays at its default `prefer_into`, and vice versa: the two parameters are
independently settable, matching the note-merge design in ADR-039.

### `merge_entity` semantics

`merge_entity(namespace, into_id, from_id, policy, content_strategy, dry_run)`:

```text
1. Resolve namespace via NamespaceToken (ADR-007).
2. Open BEGIN IMMEDIATE transaction on the backend.
3. Read both entities; verify both alive (deleted_at IS NULL) and same namespace.
4. Verify both on the same backend (cross-backend → CoordinatorError::Unsupported).
5. Collect all edges incident to from_id (outbound + inbound, deduped by edge id).
6. For each edge:
   a. Rewire endpoints: source_id == from_id → into_id; target_id == from_id → into_id.
   b. If rewire produces a self-loop: drop the edge.
   c. Otherwise: upsert with DO UPDATE semantics (ADR-009) on the unique triple
      (namespace, source, target, relation).
7. Merge entity fields:
   - name:        per `policy`
   - description: per `content_strategy` (independent of `policy` — see above)
   - properties:  per `policy` (deep-merge for Union; key-add for PreferInto;
                  full-override for PreferFrom)
   - tags:        always unioned
   - entity_type: per policy (validated via EntityTypeRegistry)
8. Upsert merged into entity. Re-index FTS5 + vector store.
9. Tombstone from entity: set deleted_at = now(), merged_into = into_id,
   merge_event_id = (emit MergeEvent id). Remove from FTS5 + vector store.
   The entity row is NOT hard-deleted; it carries merged_into provenance.
10. Commit transaction.
11. Emit MergeEvent for audit trail (entity_merged event with into_id, from_id,
    edges_rewired count, policy, content_strategy). Skipped when `dry_run` is true.
```

`dry_run=true` short-circuits all writes in step 6 (edge rewire), step 8 (upsert +
reindex), step 9 (tombstone), and step 11 (event emission) — it is a read-only
preview. `edges_rewired` in the returned summary is still accurate: it counts the
edges that _would_ be rewired, computed without writing.

Returns:

```rust
pub struct MergeSummary {
    pub kept_id: Uuid,
    pub removed_id: Uuid,
    pub edges_rewired: usize,
    pub properties_merged: usize,
    pub tags_unioned: usize,
    pub content_appended: bool,
    pub dry_run: bool,
}
```

Merge operations keep their separate transaction boundary: SQL/FTS changes happen
inside the backend transaction, while vector re-insert for the retained record is
awaited after the SQL transaction commits because embedding generation is async and
cannot run inside `BEGIN IMMEDIATE`. The shipped code does not persist an outbox or
`pending_reindex` retry record; callers may retry idempotent reindex operations when
exposed, and durable retry is deferred.

### `merge_note` semantics

Note merge follows the same structure with substrate-specific rules. Notes are
tombstoned (not hard-deleted) on merge — the merged-away note remains as a
tombstone row with provenance metadata, consistent with the note retention model
(NoteStatus state machine).

```text
merge(kind="note", from_id, into_id):
  1. Load from_id and into_id under &NamespaceToken
  2. Validate same note_kind compatibility (or policy-allowed cross-kind)
  3. Apply merged content on into_id
  4. Atomically re-point edges from from_id to into_id
  5. Tombstone from_id with merged_into=into_id and merge_event_id
  6. Emit NoteMerged event
```

The tombstone model:

```rust
pub struct Tombstone {
    pub deleted_at:     Timestamp,
    pub deleted_by:     ActorRef,
    pub merged_into:    Option<Uuid>,
    pub merge_event_id: Uuid,
    pub reason:         TombstoneReason,
}
```

After a successful merge, `from_id` has `status = deleted`, `deleted_at` set to
the merge timestamp, and `properties["_merged_into"]` = `into_id`. The record
is NOT removed from storage. This preserves recovery options and audit trails
for authored content.

Different-kind merge (e.g., observation + insight) is rejected. If an agent
genuinely needs to consolidate notes of different kinds, they create a new note
of the appropriate kind and `supersedes` both.

### Hard-delete is a separate purge op

Hard-delete is NOT the merge default. `merge` produces tombstones. An explicit
administrative `purge(id=..., kind=..., policy=...)` op handles true hard-delete
and is governed separately by ADR-018 (authorization gate). To permanently
remove a tombstoned note after merge, the caller explicitly calls
`delete(kind="note", id=from_id, hard=true)`.

### Soft vs hard delete

Default delete is **soft** for entities and notes (sets `deleted_at` timestamp).
Hard delete is opt-in via `hard: true`:

```text
delete(kind="entity", id="...", hard=true)
delete(kind="note",   id="...", hard=true)
delete(kind="edge",   id="...")                    # edges are always hard
```

Soft delete:

- Sets `deleted_at = now()` on the record.
- Removes from FTS5 (search results don't include soft-deleted records).
- Removes from vector store (vector search doesn't return soft-deleted records).
- Leaves edges in place; edge queries filter by alive endpoints in the runtime.

Hard delete:

- Removes the row from storage.
- Cascades to incident edges (edges become orphans if endpoints are hard-deleted;
  the runtime filters orphans on read).
- Removes from FTS5 and vector store.
- Cross-backend cascade follows ADR-009's `_cross_backend_wal` design.

Edges have no soft-delete state. `delete(kind="edge")` is always hard. Edges
don't carry identity worth recovering — if an edge was wrong, recreate it.

### Cross-backend constraints

Per ADR-009 (S20), cross-backend `merge_entity` and `merge_note` return
`CoordinatorError::CrossBackendUnsupported` with the constraint message. Both
records must reside on the same backend. This applies in v1 and v2.

If the caller needs to consolidate entities that span backends, the workflow is:

1. Copy one entity to the other's backend (manual or via future relocation ADR).
2. Run `merge_entity` on the single backend.

Edge curation (`get_edge`, `update_edge`, `delete_edge`) supports cross-backend
edges through the SubstrateCoordinator (ADR-003, ADR-029). An edge's
namespace and endpoints uniquely identify it; the coordinator resolves which
backend owns the edge before dispatching.

### Edge CRUD

Edge curation is symmetric to entity curation, minus the merge operation:

```rust
async fn get_edge(&self, namespace: &str, edge_id: Uuid) -> RuntimeResult<Option<Edge>>;

async fn list_edges(
    &self, namespace: &str, filter: EdgeListFilter, limit: u32,
) -> RuntimeResult<Vec<Edge>>;

async fn update_edge(
    &self, namespace: &str, edge_id: Uuid, patch: EdgePatch,
) -> RuntimeResult<Edge>;

async fn delete_edge(&self, namespace: &str, edge_id: Uuid) -> RuntimeResult<bool>;

async fn count_edges(
    &self, namespace: &str, filter: EdgeListFilter,
) -> RuntimeResult<u64>;
```

`EdgeListFilter` supports the common cases: `source_id`, `target_id`, relations
(as `Vec<EdgeRelation>`), weight range. The filter is index-friendly (source +
relation, target + relation are both indexed in `graph_edges`).

### Audit trail via events

Every curation operation emits an event to `EventStore`:

```text
update_entity → entity_updated  { id, namespace, changed_fields }
update_edge   → edge_updated    { id, namespace, changed_fields }
update_note   → note_updated    { id, namespace, changed_fields }
merge_entity  → entity_merged   { into_id, from_id, policy, edges_rewired }
merge_note    → note_merged     { into_id, from_id, policy, edges_rewired }
delete_entity → entity_deleted  { id, namespace, hard }
delete_edge   → edge_deleted    { id, namespace }
delete_note   → note_deleted    { id, namespace, hard }
```

Events carry the actor (from `NamespaceToken.principal_id`) and the timestamp.
This provides an audit trail without requiring callers to manage history
explicitly. Storage of events is governed by ADR-038.

### Property unset and edge property updates

`v1` does not support `unset(key)` as a first-class operation. To remove a key
from `properties`, the caller passes the desired post-state object. A future
extension may add explicit unset semantics if a concrete use case justifies it.

`update_edge` patches `relation`, `weight`, and `properties`. The original ADR-014
restricted edge updates to relation+weight; this rewrite expands to
properties for symmetry with entities. Edge properties remain free-form JSON.

## Rationale

### Why patch semantics?

A patch ("change this field") expresses intent directly. Replace semantics
("here's the new full state") force the agent to fetch first, modify
client-side, then send the full record back — costing a round trip and risking
lost updates if the agent's snapshot is stale. Patch lets the agent reason about
changes rather than full states.

### Why rename `MergeStrategy` to `EntityDedupMergePolicy`?

Two distinct operations use "merge" in khive: substrate-level entity/note
deduplication, and graph-versioning snapshot merge. Both had types named
`MergeStrategy` in the codebase. Renaming to `EntityDedupMergePolicy` (this ADR)
and `SnapshotMergeStrategy` (ADR-010) eliminates the collision. The name reflects
intent: this policy controls how duplicate entities are deduped.

### Why no edge merge?

Edges aren't dedup candidates the way entities are. Two edges that share
`(namespace, source, target, relation)` are inherently the same edge — the
unique triple constraint enforces this at the storage layer. Edge weights,
properties, and metadata don't carry the kind of identity that warrants merging.
If a caller wants to consolidate two edges, they update one and delete the
other; or rely on `upsert_edge` deduplication.

### Why one-backend-only merge?

Cross-backend merge requires coordinated updates to entities, edges, FTS5
indexes, and vector indexes across multiple SQLite files — none of which can
participate in a single atomic transaction. Without atomicity, partial failure
leaves the KG in an inconsistent state that's hard to recover from. The
constraint is documented and surfaced as a typed error so callers know how to
work around it.

### Why audit trail via events?

Curation operations are state changes that humans and other agents need to be
able to inspect. Putting the audit trail in `EventStore` reuses the existing
append-only substrate and keeps the curation API simple. Callers that don't
need the audit can ignore the event stream; callers that do need it (e.g.,
replay, anomaly detection) query `EventStore` directly.

### Why soft delete by default?

In a research KG, most "deletes" are mistakes the agent wants to undo later
("oh, this entity was actually right after all"). Soft delete preserves
recoverability at low cost (one timestamp column, one filter clause on read).
Hard delete is opt-in for the cases where the caller knows the record should
not be recoverable.

Edges are exempt — they don't carry identity worth recovering. An edge gone
wrong is recreated, not undeleted.

### Why kind is immutable on update?

Changing an entity's kind from `concept` to `document` would change the legal
edge endpoint set (ADR-002, ADR-017 Pack Standard `EDGE_RULES`), invalidate `entity_type` (which is
kind-scoped per ADR-001), and require revalidating every incident edge.
Effectively it's a delete + recreate. Forcing the agent to do it explicitly
keeps the curation API honest.

Same logic for note kind. To "change" a note's kind, supersede it with a new
note of the right kind.

## Alternatives Considered

| Alternative                                                          | Why rejected                                                                         |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| Wholesale PUT (full replace) on `update_entity`                      | Forces fetch-modify-write; lost-update races. Patch wins.                            |
| Auto-merge via embedding similarity                                  | Wrong calls are unfixable; ambiguous policy. Curation must be explicit.              |
| `set_property(key, value)` / `unset_property(key)` as separate verbs | Surface bloat; wholesale `properties` patch covers the common case.                  |
| Soft-delete for edges                                                | Edges don't carry recoverable identity. Symmetry not worth the query complexity.     |
| Same name (`MergeStrategy`) for entity merge and snapshot merge      | Collision causes confusion; rename to intent-specific names.                         |
| Cross-backend `merge_entity` with eventual consistency               | No atomicity; partial failure modes are hard to reason about. Return error in v1+v2. |
| Mutable kind                                                         | Equivalent to delete + recreate; better expressed as such.                           |
| Curation operations bypass validation                                | Reintroduces the failure modes ADR-001/002 fix. Curation must validate.              |
| No audit trail                                                       | Operations on shared state need history. Events are the right substrate.             |

## Consequences

### Positive

- Patch semantics match agent intent; no fetch-modify-write round trips required.
- Validation chain ensures `entity_type`, edge endpoints, and note kinds are
  always consistent after curation.
- Index consistency: FTS5 re-indexed in-transaction; vector reindex awaited after the SQL
  transaction (no durable outbox/`pending_reindex` in shipped code; durable retry
  deferred to a future ADR).
- Naming clarity: `EntityDedupMergePolicy` vs `SnapshotMergeStrategy`.
- Single-backend merge constraint is surfaced as a typed error; callers know
  exactly what's unsupported.
- Audit trail via `EventStore` reuses existing substrate.
- Soft delete + hard delete distinction matches recoverability needs.

### Negative

- Curation requires a `NamespaceToken` per call. Same overhead as any
  namespace-scoped operation.
  Mitigated: the token is pass-by-ref; cost is negligible.
- Cross-backend merge unsupported. Callers must relocate before merging.
  Mitigated: documented constraint; relocation is a future ADR.
- Vector re-indexing happens post-transaction (because embedding is async). If
  it fails, the entity is correct but the vector index is stale.
  Mitigated: failure is logged; idempotent retry mechanism via `reindex_entity`.
- No `unset_property` verb in v1. Removing keys requires full `properties`
  replacement.
  Mitigated: rare in practice; can extend later if needed.

### Neutral

- Edge soft-delete is not provided — edges are always hard-deleted.
- Note merge requires same-kind. Different-kind merging is expressed via
  supersession.
- The verb-dispatch surface (ADR-016) routes `update/merge/delete` through the
  `kind` discriminant; this ADR specifies semantics per kind.

## Implementation

- `crates/khive-runtime/src/curation.rs`: `update_entity`, `merge_entity`,
  `update_edge`, `merge_note` (new), `update_note` (new).
- `crates/khive-runtime/src/operations.rs`: `get_edge`, `list_edges`,
  `delete_edge`, `count_edges`, `delete_entity`, `delete_note`.
- `crates/khive-runtime/src/audit.rs`: event emission helpers for each curation
  operation.
- `EntityDedupMergePolicy` type lives in `khive-runtime` (curation policy, not a
  substrate type).
- Cross-backend merge guard: SubstrateCoordinator checks that
  `into_id` and `from_id` resolve to the same backend; returns
  `CoordinatorError::CrossBackendUnsupported` otherwise.

### Public curation DSL uses `kind`, not `substrate`

The public curation DSL uses `kind` as the discriminator across `create`,
`list`, `search`, `update`, `delete`, and `merge`. `substrate` is the
lower-level resolved storage family (entity / note / event / pack-owned backend)
and is internal to the registry — never a public DSL parameter.

```text
merge(kind="note",    from_id=..., into_id=...)
merge(kind="entity",  from_id=..., into_id=...)
merge(kind="concept", from_id=..., into_id=...)  # granular kind; resolves to entity substrate
merge(kind="task",    from_id=..., into_id=...)  # granular kind; resolves to note substrate
```

The registry maps each granular `kind` to its storage substrate and owning pack
internally via `KindResolution { substrate, granular_kind, owner }`. Callers
never specify `substrate` directly.

## References

- ADR-001: Entity Kind Taxonomy — entity_type validation via EntityTypeRegistry.
- ADR-002: Edge Ontology — 15 base relations, endpoint legality. (Amended by ADR-055: current total is 17.)
- ADR-004: Substrate Observables — Note kind_status lifecycle validation.
- ADR-005: Storage Capability Traits — `EntityStore`, `GraphStore`, `NoteStore`,
  `EventStore`, `VectorStore`, `TextSearch` are the primitives curation composes.
- ADR-007: Namespace — `NamespaceToken` for namespace enforcement.
- ADR-009: Backend Architecture — `upsert_edge` DO UPDATE semantics; cross-backend
  cascade constraints.
- ADR-010: KG Versioning — `SnapshotMergeStrategy` (the other "merge" in khive).
- ADR-013: Note Kind Taxonomy — supersession via edge for cross-kind transitions.
- ADR-016: Request DSL — verb surface that exposes curation to agents.
- ADR-038: Events — event substrate for audit trail.
