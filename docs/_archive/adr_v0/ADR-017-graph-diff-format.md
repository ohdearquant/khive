# ADR-017: Graph Diff Format

**Status**: planned (implementation deferred to v0.4+)\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

ADR-010 establishes "GitHub for knowledge graphs" as the strategic direction. The first primitive
required is a **KG diff** — a representation of the change between two graph states (entities added,
removed, or modified; edges added, removed, or modified; individual properties set or unset).
Without a well-defined diff format, none of the higher-level versioning primitives (commit, branch,
merge, PR) can be built.

This ADR specifies the diff format. It answers:

- What atomic operations exist?
- What does the JSON for each operation look like?
- How is entity identity preserved across renames and merges?
- Are operations ordered or a commutative set?
- Can a diff be reversed?
- How are merge conflicts represented?
- What MCP tools expose the diff system to agents?
- How does the diff format relate to ADR-015 (versioning snapshots)?

### Prior art surveyed

**RFC 6902 JSON Patch** — well-known, tooling-rich, but maps awkwardly to property graphs. Its
`path` pointers (`/entities/abc123/properties/year`) are fragile: if the parent entity is removed,
the path silently targets nothing. The format has no concept of edge identity or entity merge.

**W3C LD Patch / RDF Delta** — triple-centric: every operation is an `(s, p, o)` assertion or
retraction. This works for RDF but is too fine-grained for a property graph. Representing "entity
name changed from X to Y" requires multiple triple operations across system predicates, destroying
readability. Also requires URI-based identity — incompatible with khive's UUID model.

**CRDT-shaped graph deltas (Automerge / Yjs lineage)** — CRDTs guarantee automatic merge with no
conflicts, at the cost of semantic safety. ADR-010 explicitly rejects CRDTs for KG merge: "silently
wrong is worse than failing visibly." Two agents adding contradictory `extends` edges must surface a
conflict, not silently keep both. CRDT technique is useful as inspiration for op commutativity
analysis, but not as the merge strategy.

**Property-graph diff tools (Neo4j APOC `apoc.diff.graphs`)** — produces an `added`/`removed`/
`changed` summary at the entity level, but does not expose a serializable op sequence suitable for
network transport, apply, or conflict analysis. Not composable as a first-class type.

**Conclusion**: a domain-specific op format designed for khive's entity/edge model provides better
readability and semantic precision than any of the above, at the cost of novelty. The cost is
acceptable given the closed vocabularies in ADR-001 and ADR-002, which bound the complexity of both
the format and the conflict space.

### Relationship to the portability format

`portability.rs` defines `KgArchive` — a full snapshot of all entities and edges in a namespace. A
diff is the minimal op sequence that transforms one `KgArchive` into another. The diff format is a
DELTA on top of the snapshot format; it does not replace it.

### Relationship to ADR-015 (versioning)

ADR-015 picks **full-snapshot storage** for commits, with diffs computed on demand. `GraphDiff` is
therefore a transient computation between two `KgArchive` instances — it is the value returned by
the `diff` MCP tool, not a persistent on-disk artifact. The diff format defined here would also work
as on-disk commit storage if ADR-015 ever switches to delta storage, but that is not the v0.1 plan.

---

## Decision

### 1. Operation taxonomy — 9 canonical op kinds

A diff is a **list of operations**. The canonical operation set covers three domains:

#### 1.1 Entity operations

| Op kind         | Description                                                   |
| --------------- | ------------------------------------------------------------- |
| `entity_add`    | Add a new entity (all fields)                                 |
| `entity_remove` | Remove an entity by id                                        |
| `entity_modify` | Change name, description, tags, or kind on an existing entity |

#### 1.2 Edge operations

Edge identity in the diff uses the **composite key** `(source_id, target_id, relation)`. This
matches the portability format, which has no separate edge id field.

| Op kind       | Description                              |
| ------------- | ---------------------------------------- |
| `edge_add`    | Add a new directed edge                  |
| `edge_remove` | Remove an existing edge by composite key |
| `edge_modify` | Change the weight of an existing edge    |

#### 1.3 Property operations (within an entity)

These express surgical changes to a single key within `entity.properties`, enabling diffs to be more
granular than wholesale property replacement.

| Op kind          | Description                              |
| ---------------- | ---------------------------------------- |
| `property_set`   | Set a single property key to a new value |
| `property_unset` | Remove a single property key             |

#### 1.4 Entity merge (cross-cutting)

| Op kind        | Description                                                             |
| -------------- | ----------------------------------------------------------------------- |
| `entity_merge` | Record that `from_id` was merged into `into_id`, including the strategy |

`entity_merge` is a first-class op, not syntactic sugar for a `remove + rewire` sequence. This
preserves intent and allows `apply_diff` to delegate to the runtime's `merge_entity` (ADR-014),
which handles edge rewiring, property merge, and index cleanup atomically.

### 2. JSON shapes for each operation

```json
{ "op": "entity_add",
  "entity": {
    "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "kind": "concept",
    "name": "FlashAttention",
    "description": "IO-aware exact attention algorithm",
    "properties": { "domain": "attention", "year": "2022" },
    "tags": ["attention", "cuda"]
  }
}

{ "op": "entity_remove",
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}

{ "op": "entity_modify",
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "patch": {
    "name": "FlashAttention",
    "description": "IO-aware exact attention (revised)",
    "tags": ["attention", "cuda", "triton"],
    "kind": null
  }
}
```

For `entity_modify.patch`: absent fields are unchanged; `null` value is forbidden for `name` and
`kind` (they are required); `null` for `description` means "clear description". This mirrors
ADR-014's `EntityPatch` semantics and the `Option<Option<String>>` Rust pattern.

```json
{ "op": "edge_add",
  "source": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "target": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
  "relation": "introduced_by",
  "weight": 0.9
}

{ "op": "edge_remove",
  "source": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "target": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
  "relation": "introduced_by"
}

{ "op": "edge_modify",
  "source": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "target": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
  "relation": "introduced_by",
  "weight": 1.0
}
```

```json
{ "op": "property_set",
  "entity_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "key": "year",
  "value": "2022"
}

{ "op": "property_unset",
  "entity_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "key": "draft_note"
}
```

```json
{
  "op": "entity_merge",
  "into_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "from_id": "dead0000-beef-cafe-0000-000000000001",
  "strategy": "prefer_into"
}
```

#### 2.1 Top-level diff envelope

A diff is always wrapped in an envelope:

```json
{
  "format": "khive-diff",
  "version": "0.1",
  "from_namespace": "experiment",
  "to_namespace": "main",
  "computed_at": "2026-05-15T12:00:00Z",
  "ops": [
    { "op": "entity_add", ... },
    { "op": "edge_add", ... }
  ]
}
```

`from_namespace` and `to_namespace` are informational. The diff does not embed a snapshot reference;
when used with ADR-015 snapshots, the snapshot ids are stored at the commit layer.

#### 2.2 Validation rules at serialization and apply time

- `op` must be one of the 9 canonical values. Unknown ops → `InvalidInput`.
- `edge_add.relation` and `edge_modify.relation` must be one of the 13 canonical relations
  (ADR-002). Unknown relations → `InvalidInput`.
- `entity_add.kind` must be one of the 6 canonical entity kinds (ADR-001). Unknown kinds →
  `InvalidInput`.
- `entity_modify.patch` with no fields present is a no-op warning, not an error.

### 3. Identity preservation — dedicated `entity_merge` op

When two entities are found to be the same concept (e.g., an agent added "FlashAttention" and
another added "Flash Attention"), identity consolidation is expressed as a single `entity_merge` op:

```json
{
  "op": "entity_merge",
  "into_id": "a1b2c3d4-...",
  "from_id": "dead0000-...",
  "strategy": "prefer_into"
}
```

`apply_diff` delegates this to `KhiveRuntime::merge_entity` (ADR-014), which:

1. Rewires all edges incident to `from_id` to reference `into_id`.
2. Merges properties per `strategy` (`prefer_into` | `prefer_from` | `union`).
3. Hard-deletes `from_id`.
4. Re-indexes FTS5 and the vector store.

**Why not `remove(A) + edge_rewire ops`?** A rewire-per-edge approach requires the diff to enumerate
every incident edge of the merged entity — this couples the diff to the current graph state (all
edges must be known at diff-compute time). The `entity_merge` op is intent-carrying: it says "these
are the same thing," and leaves the mechanical rewiring to the apply layer. This also means a diff
containing an `entity_merge` is readable by a human: one line vs. N edge operations.

**Why not an id-renaming pass before diff computation?** An id-rename pass would rewrite all edge
references before computing the diff, producing a clean diff with no trace of the merge. This is
attractive for simplicity but destroys provenance: you can no longer tell from the diff that a merge
happened. For an audit-trail system ("GitHub for KGs"), provenance matters.

### 4. Order semantics — sequence with deterministic application order

A diff is a **sequence** (ordered list of operations). Operations are applied in list order. This is
simpler to reason about than a commutative set and imposes no per-op commutativity requirement.

**Deterministic application order convention**: when `diff` computes a diff, it MUST emit operations
in this order:

1. `entity_add` (ensures referenced entities exist before edges reference them)
2. `entity_modify` / `property_set` / `property_unset` (modify what exists)
3. `entity_merge` (consolidate identities before rewiring)
4. `edge_add` (add edges only after all entity additions are done)
5. `edge_modify` (update edges on existing entities)
6. `edge_remove` (remove before entity removes to avoid referential-integrity checks)
7. `entity_remove` (remove entities last)

This convention ensures that a well-formed diff from `diff` applies without ordering conflicts.
Hand-authored diffs are NOT required to follow this order, but `apply_diff` may validate or reorder
them when a `reorder: true` flag is provided.

**Parallel agent edits**: two agents computing diffs independently will each produce a sequence.
Merging those sequences into a single sequence is the three-way merge problem (see §6). The sequence
model makes this straightforward: conflict detection is per-op-key (entity id + field, or edge
composite key + attribute), and non-conflicting ops can be freely interleaved.

**Why not a commutative set (CRDT-style)?** Making all ops commutative requires every op to carry
enough prior state to be idempotent in any order. For `entity_add` this is fine (the entity payload
is self-contained). For `entity_remove` it means carrying the full prior entity state (converting a
slim diff to a reversible diff). For semantic safety, commutativity would also require detecting
contradictory edge assertions (op A says `FlashAttention extends SoftmaxAttention`, op B says
`FlashAttention extends LinearAttention` — both can't be true at weight 1.0). CRDT auto-merge would
silently keep both. Sequence + explicit conflict detection catches this.

### 5. Reversibility — slim diff for v0.1

A diff in v0.1 carries **intent only**: `entity_remove` stores the entity id, not the prior entity
state. `edge_remove` stores the composite key, not the full prior edge. `entity_modify` stores the
new values, not the old values.

**Consequence**: a v0.1 diff is not self-contained enough to be reversed without the base snapshot.
Given that the base snapshot (`KgArchive`) is always available in the versioning model (snapshots
are cheap and immutable per ADR-010), this is acceptable.

**Reversible diff (v0.2 target)**: extend each remove and modify op with a `prior` field:

```json
{ "op": "entity_remove",
  "id": "a1b2c3d4-...",
  "prior": { "kind": "concept", "name": "FlashAttention", ... }
}
```

This allows `apply_diff(diff.reverse(), target)` without loading the base snapshot. Deferred to v0.2
because it doubles the size of remove/modify ops and the base-snapshot model suffices for the
versioning use cases in v0.4–v0.6 of the phasing plan (ADR-010).

### 6. Conflict surface in merge output

A three-way merge of `base → ours` diff and `base → theirs` diff produces either a clean
`MergedDiff` (ops interleaved, de-duplicated) or a `ConflictDiff` containing conflict markers.
Conflicts are **structured objects**, not text sentinels.

#### 6.1 Conflict marker shape

```json
{
  "format": "khive-diff",
  "version": "0.1",
  "from_namespace": "main",
  "to_namespace": "merge-result",
  "computed_at": "2026-05-15T12:00:00Z",
  "ops": [
    { "op": "entity_add", "entity": { ... } },
    {
      "op": "conflict",
      "conflict_kind": "property_mismatch",
      "entity_id": "a1b2c3d4-...",
      "key": "year",
      "ours": "2022",
      "theirs": "2021",
      "resolution": null
    },
    {
      "op": "conflict",
      "conflict_kind": "modify_delete",
      "entity_id": "dead0000-...",
      "ours_op": { "op": "entity_modify", "id": "dead0000-...", "patch": { "name": "New Name" } },
      "theirs_op": { "op": "entity_remove", "id": "dead0000-..." },
      "resolution": null
    }
  ]
}
```

A `ConflictDiff` is a valid diff document with some ops having `"op": "conflict"`. It is NOT
applicable (applying a diff containing unresolved conflicts is an error). It is the document shown
to the human or agent resolver.

When `resolution` is populated by the resolver, the op becomes applicable:

- `resolution: "ours"` → apply `ours_op`, discard `theirs_op`
- `resolution: "theirs"` → apply `theirs_op`, discard `ours_op`
- `resolution: "custom"` + a sibling `"resolved_op"` field → apply the resolved op

#### 6.2 Conflict kinds and auto-resolution policy

| Conflict kind           | Description                                                       | Auto-resolve?                                                                     |
| ----------------------- | ----------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| `property_mismatch`     | Both diffs set the same property key to different values          | No — value semantics are unknown                                                  |
| `name_conflict`         | Both diffs set entity name to different values                    | No — names have identity semantics                                                |
| `description_conflict`  | Both diffs set description to different values                    | Yes (ours wins) — descriptions are annotations, not identifiers                   |
| `tag_conflict`          | Both diffs set tags to different lists                            | Yes (union of both sets)                                                          |
| `modify_delete`         | One diff modifies an entity the other removes                     | No — user intent is contradictory                                                 |
| `duplicate_edge_weight` | Both diffs add the same composite-key edge with different weights | Yes (max weight wins) — weight is a confidence score, higher confidence dominates |
| `dangling_edge`         | One diff removes an entity; the other adds an edge to it          | No — referential integrity violated                                               |
| `duplicate_entity_add`  | Both diffs add an entity with the same id                         | Yes (field-by-field: last-writer-wins on scalars, union on tags)                  |

The auto-resolution rules for `description_conflict`, `tag_conflict`, `duplicate_edge_weight`, and
`duplicate_entity_add` are conservative choices that minimize information loss while eliminating the
most common non-semantic conflicts that arise from concurrent agent work.

### 7. MCP tool interface

Three tools are exposed to agents. All three treat `GraphDiff` as a **serializable first-class
value** — agents can store diffs, pass them between calls, and inspect them. A diff is not a
transient computation.

#### 7.1 `diff`

Compute the diff between two named namespaces (or two named snapshots, once ADR-015 lands).

```json
{
  "name": "diff",
  "description": "Compute the diff between two KG namespaces. Returns a GraphDiff JSON document listing all added, removed, and modified entities and edges.",
  "inputSchema": {
    "type": "object",
    "required": ["from_namespace", "to_namespace"],
    "properties": {
      "from_namespace": {
        "type": "string",
        "description": "The base namespace (the 'old' state). Defaults to 'main'."
      },
      "to_namespace": {
        "type": "string",
        "description": "The target namespace (the 'new' state)."
      },
      "from_snapshot": {
        "type": "string",
        "description": "Optional snapshot id for the base (used once ADR-015 snapshots are available). Takes precedence over from_namespace if both provided."
      },
      "to_snapshot": {
        "type": "string",
        "description": "Optional snapshot id for the target."
      }
    }
  },
  "outputSchema": {
    "type": "object",
    "description": "GraphDiff document (format: khive-diff, version: 0.1)"
  }
}
```

#### 7.2 `apply_diff`

Apply a diff to a target namespace.

```json
{
  "name": "apply_diff",
  "description": "Apply a GraphDiff to a target namespace. Fails if the diff contains unresolved conflict ops. Returns an ApplySummary.",
  "inputSchema": {
    "type": "object",
    "required": ["diff", "target_namespace"],
    "properties": {
      "diff": {
        "type": "object",
        "description": "A GraphDiff document (format: khive-diff, version: 0.1)."
      },
      "target_namespace": {
        "type": "string",
        "description": "Namespace to apply the diff to."
      },
      "dry_run": {
        "type": "boolean",
        "default": false,
        "description": "If true, validate and report what would change without writing."
      }
    }
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "entities_added": { "type": "integer" },
      "entities_removed": { "type": "integer" },
      "entities_modified": { "type": "integer" },
      "entities_merged": { "type": "integer" },
      "edges_added": { "type": "integer" },
      "edges_removed": { "type": "integer" },
      "edges_modified": { "type": "integer" },
      "properties_set": { "type": "integer" },
      "properties_unset": { "type": "integer" },
      "errors": {
        "type": "array",
        "description": "Apply errors (e.g. dangling edges, unknown entity ids). Non-empty means partial apply — check carefully.",
        "items": { "type": "string" }
      }
    }
  }
}
```

#### 7.3 `diff_summary`

Human-readable summary of a diff.

```json
{
  "name": "diff_summary",
  "description": "Return a human-readable plain-text summary of what a GraphDiff does, suitable for a commit message or PR description.",
  "inputSchema": {
    "type": "object",
    "required": ["diff"],
    "properties": {
      "diff": {
        "type": "object",
        "description": "A GraphDiff document."
      }
    }
  },
  "outputSchema": {
    "type": "string",
    "description": "Plain-text summary, e.g.: '3 entities added (FlashAttention v2, Tri Dao, ...), 1 edge added (introduced_by), 0 conflicts.'"
  }
}
```

### 8. Composability with ADR-015 versioning

ADR-015 resolves the delta-vs-snapshot question: **commits are full `KgArchive` snapshots stored
with a parent pointer and a content hash. Diffs are NOT stored — they are computed on demand between
two snapshots.**

This means `GraphDiff` is a **transiently computed, serializable document**:

- `diff(from_snapshot, to_snapshot)` loads both `KgArchive` instances and runs the diff algorithm to
  produce a `GraphDiff`.
- `GraphDiff` is returned to the agent as a first-class value and can be stored externally (e.g., as
  a PR artifact), but the khive storage layer has no `kg_diffs` table.
- `apply_diff(diff, target)` writes the diff ops into the live namespace, after which the agent can
  call `commit` to snapshot the result.

**Implication for the diff envelope**: the `from_snapshot` / `to_snapshot` fields in `diff`
reference the SHA-256 snapshot ids defined in ADR-015 (§D.1). In v0.1, before `commit` is available,
callers use namespace names (`from_namespace`, `to_namespace`) and the snapshot fields are optional
/ ignored.

**The `archive_json` round-trip**: ADR-015 stores commits as `KgArchive` JSON in the
`kg_snapshots.archive_json` column. `diff` deserializes two such records and computes the diff. This
means the diff algorithm takes two `KgArchive` instances as input — the same format defined in
`portability.rs`. No new deserialization path is needed.

---

## Rationale

### Why domain-specific ops over JSON Patch (RFC 6902)?

RFC 6902 paths like `/entities/a1b2c3d4/properties/year` are fragile: if the entity is removed in
the same diff, the path silently targets nothing. JSON Patch also has no concept of edge
composite-key identity, entity kind validation, or the `entity_merge` operation. The tooling
advantage of RFC 6902 (libraries exist in every language) is offset by the impedance mismatch — a
khive-specific deserializer is needed regardless.

Domain-specific ops are more readable at the JSON level (`"op": "entity_add"` vs
`"op": "add", "path": "/entities/..."`) and map directly to the runtime operations in ADR-014.

### Why sequence over commutative set?

Pure set semantics require every op to be idempotent and order-independent. For `entity_add` this is
trivial. For `entity_remove` in a slim diff (no prior state), idempotency requires the apply layer
to silently succeed on already-absent entities — which is fine. But for semantic safety, the
critical issue is conflicting semantic ops: two agents adding contradictory `extends` edges cannot
both be silently accepted. A sequence with explicit conflict detection catches this at merge time; a
CRDT set would silently produce an inconsistent graph.

### Why slim (no prior state in removes/modifies)?

The base snapshot (`KgArchive`) is cheap to produce and immutable. Carrying prior state in every
remove/modify op doubles (or triples) the diff size and couples the diff to the snapshot it was
computed from, making it harder to compose diffs or apply them to slightly different bases. The v0.2
reversible extension is a clean upgrade path when genuinely needed.

### Why a dedicated `entity_merge` op?

`merge_entity` is a first-class curation operation in ADR-014. Representing it as a diff op: (a)
preserves intent in the diff history, (b) avoids enumerating all incident edges in the diff, (c)
delegates the mechanical work (rewiring, re-indexing) to the runtime which already does it
correctly. A `remove + rewire` representation would require the diff computation to enumerate all
incident edges at diff time — coupling the diff format to the full graph state.

### Why `(source, target, relation)` as edge identity (not a separate edge id)?

The portability format (`ExportedEdge` in `portability.rs`) does not include an edge id field.
Introducing a new edge id in the diff format would create a discrepancy: diffs reference edge ids
that the archive format doesn't expose. Using the composite key keeps the diff format compatible
with the archive format. It also matches the graph semantics: there should be at most one edge of a
given relation between any two entities (ADR-002 does not define a "multiple parallel edges with the
same relation" use case).

---

## Alternatives Considered

| Alternative                                           | Pros                                           | Cons                                                                                                                                  | Why rejected                                                              |
| ----------------------------------------------------- | ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| RFC 6902 JSON Patch                                   | Standard, tooling-rich, no new format to learn | Fragile paths, no edge composite-key identity, no entity_merge, impedance mismatch with property graph                                | Impedance mismatch cost > familiarity benefit                             |
| Triple-style `{op, subject, predicate, object}`       | Uniform, aligns with RDF lineage               | Too granular for entity-level ops; name-change = multiple triples; loses entity-level semantics                                       | Too low level for human readability                                       |
| Commutative op set (CRDT-inspired)                    | No merge conflicts in common case              | Silently wrong on semantic contradictions; requires reversibility (prior state) for all remove/modify ops; ADR-010 explicitly rejects | Semantic safety requirement overrides convenience                         |
| Reversible diff from v0.1                             | Full bidirectionality from the start           | Doubles remove/modify op size; base snapshot makes it unnecessary for v0.1 versioning                                                 | Defer — base snapshot model suffices                                      |
| `entity_merge` as `remove(from) + N edge_rewire ops`  | No new op kind                                 | Enumerates all incident edges in the diff (couples to graph state at diff time); loses provenance; N ops vs 1                         | Intent preservation and provenance win                                    |
| Embed conflict markers as git `<<<<<<` text sentinels | Familiar to developers                         | Unstructured — cannot be parsed, resolved programmatically, or inspected by agents                                                    | Structured objects are the only viable option for programmatic resolution |

---

## Consequences

### Positive

- Human-readable JSON diffs with clear semantic intent (`entity_add`, `edge_add`, etc.).
- Validates against ADR-001 (entity kinds) and ADR-002 (edge relations) at apply time — invalid
  diffs are caught before they corrupt the graph.
- `entity_merge` op composes directly with ADR-014's `merge_entity` runtime operation.
- Structured conflict markers enable programmatic conflict resolution by agents.
- Slim diff format keeps network payloads small; the base snapshot provides prior state when needed.
- Compatible with both delta-storage and snapshot-storage models for ADR-015.

### Negative

- Novel format — no off-the-shelf library. A deserializer and apply engine must be written.
- Slim diffs are not self-contained for reversal; the base snapshot is required. This is a
  documentation burden.
- `entity_merge` in a diff creates an implicit assumption that the runtime supports the full ADR-014
  merge semantics at apply time. A lightweight runtime that only does storage cannot apply diffs
  containing `entity_merge` ops.

### Neutral

- The `(source, target, relation)` composite key for edges means two diffs that both add an edge
  with the same composite key will conflict at the weight field (not at the edge level). This is
  consistent with the intended semantics but may surprise callers who expect edge-level
  deduplication to be transparent.

---

## Implementation Plan

This ADR is design-only. Implementation targets v0.4 per ADR-010's phasing plan.

**Crate**: `crates/khive-diff` (new crate, depends on `khive-types`, `khive-storage`).

**Types to define** (in `khive-diff/src/types.rs`):

- `DiffOp` — enum with 10 variants (one per of the 9 op kinds plus `Conflict`)
- `GraphDiff` — envelope with `ops: Vec<DiffOp>`
- `ConflictMarker` — embedded in `DiffOp::Conflict`
- `ApplySummary` — returned by `apply_diff`

**Functions to implement**:

- `compute_diff(base: &KgArchive, target: &KgArchive) -> GraphDiff`
- `apply_diff(diff: &GraphDiff, runtime: &KhiveRuntime, namespace: &str) -> Result<ApplySummary>`
- `merge_diffs(base_to_ours: &GraphDiff, base_to_theirs: &GraphDiff) -> MergeResult`
- `summarize_diff(diff: &GraphDiff) -> String`

**MCP tools** (in `crates/khive-mcp/src/tools/`):

- `diff.rs` — wraps `compute_diff`
- `apply_diff.rs` — wraps `apply_diff`
- `diff_summary.rs` — wraps `summarize_diff`

**Test coverage targets**:

- Roundtrip: `compute_diff(A, B)` followed by `apply_diff(result, A)` produces B (for all op kinds)
- Conflict detection: property mismatch, modify-delete, dangling edge
- Auto-resolution: tag union, description ours-wins, weight max
- Invalid op rejection: unknown op kind, unknown relation, unknown entity kind
- Worked example from §9 passes as an integration test

---

## Open Questions

1. **Edge id in the portability format**: `ExportedEdge` in `portability.rs` has no `id` field. The
   composite key `(source, target, relation)` is used for diff identity. Should ADR-015 or a
   follow-up add an optional `id` field to `ExportedEdge` so that diffs can reference stable edge
   ids across renames? (Renames are currently impossible — changing relation requires
   `edge_remove` + `edge_add`.) No change needed in v0.1, but warrants discussion before v0.5.

2. **`entity_merge` in a diff applied to a runtime without ADR-014**: if `apply_diff` is called
   against a storage-only runtime that doesn't implement `merge_entity`, the op should be expanded
   at apply time into the equivalent `entity_modify + edge_rewire + entity_remove` sequence, or the
   apply should error with a clear message. Which is correct? Recommendation: error with a clear
   message — fallback expansion hides the capability requirement.

3. **Diff composition**: can two diffs `A→B` and `B→C` be composed into `A→C` without materializing
   B? This would be valuable for squashing a chain of commits. Because ADR-015 uses snapshot storage
   (not delta storage), squashing is already O(1): just take the snapshot at C and discard B's
   snapshot. Diff composition is therefore a display concern (showing a combined changelog), not a
   storage concern. Defer to v0.5.

---

## Worked Example

**Scenario**: Building a small research KG about fast attention algorithms.

**Step 1** — Agent A creates two entities in branch `main`:

```json
{ "op": "entity_add",
  "entity": { "id": "aaaa0001-0000-0000-0000-000000000001",
               "kind": "concept", "name": "FlashAttention",
               "description": "IO-aware exact attention", "properties": {}, "tags": [] } }

{ "op": "entity_add",
  "entity": { "id": "aaaa0002-0000-0000-0000-000000000002",
               "kind": "person", "name": "Tri Dao",
               "description": null, "properties": {}, "tags": ["author"] } }
```

**Step 2** — Agent B works in branch `experiment`, creates `FlashAttention v2` and links it:

```json
{ "op": "entity_add",
  "entity": { "id": "bbbb0003-0000-0000-0000-000000000003",
               "kind": "concept", "name": "FlashAttention v2",
               "description": "Faster IO-aware attention", "properties": {}, "tags": [] } }

{ "op": "edge_add",
  "source": "bbbb0003-0000-0000-0000-000000000003",
  "target": "aaaa0001-0000-0000-0000-000000000001",
  "relation": "extends",
  "weight": 1.0 }

{ "op": "edge_add",
  "source": "bbbb0003-0000-0000-0000-000000000003",
  "target": "aaaa0002-0000-0000-0000-000000000002",
  "relation": "introduced_by",
  "weight": 0.9 }
```

**Step 3** — Compute `experiment - main` (the diff from main to experiment):

```json
{
  "format": "khive-diff",
  "version": "0.1",
  "from_namespace": "main",
  "to_namespace": "experiment",
  "computed_at": "2026-05-15T12:00:00Z",
  "ops": [
    {
      "op": "entity_add",
      "entity": {
        "id": "bbbb0003-0000-0000-0000-000000000003",
        "kind": "concept",
        "name": "FlashAttention v2",
        "description": "Faster IO-aware attention",
        "properties": {},
        "tags": []
      }
    },
    {
      "op": "edge_add",
      "source": "bbbb0003-0000-0000-0000-000000000003",
      "target": "aaaa0001-0000-0000-0000-000000000001",
      "relation": "extends",
      "weight": 1.0
    },
    {
      "op": "edge_add",
      "source": "bbbb0003-0000-0000-0000-000000000003",
      "target": "aaaa0002-0000-0000-0000-000000000002",
      "relation": "introduced_by",
      "weight": 0.9
    }
  ]
}
```

Note: entities `FlashAttention` and `Tri Dao` are already in `main`, so they do NOT appear as
`entity_add` ops. Only what is new in `experiment` relative to `main` is listed.

**Step 4** — Apply the diff to `main`:

`apply_diff(diff, target_namespace="main")` returns:

```json
{
  "entities_added": 1,
  "entities_removed": 0,
  "entities_modified": 0,
  "entities_merged": 0,
  "edges_added": 2,
  "edges_removed": 0,
  "edges_modified": 0,
  "properties_set": 0,
  "properties_unset": 0,
  "errors": []
}
```

After apply, `main` contains all three entities and both edges. The diff is three lines of readable
JSON. A human reviewer can immediately see that the change adds "FlashAttention v2" and links it to
the existing FlashAttention and Tri Dao nodes.

---

## References

- ADR-001: Entity Kind Taxonomy (6 entity kinds, validation at apply time)
- ADR-002: Closed Edge Ontology (13 canonical relations, validation at apply time)
- ADR-004: Substrate Observables (Event log as version history substrate)
- ADR-010: KG Versioning Direction (strategic context; phasing plan)
- ADR-014: Curation Operations (`merge_entity` runtime op used by `entity_merge` diff op)
- ADR-015: KG Versioning Model (planned; chose snapshot storage — diffs are transient computations,
  not stored deltas)
- `crates/khive-runtime/src/portability.rs`: `KgArchive`, `ExportedEntity`, `ExportedEdge` — the
  archive format this diff is a delta against
- RFC 6902 JSON Patch — considered and rejected (see §Alternatives)
- W3C LD Patch — considered and rejected (too triple-centric)
- Automerge / Yjs CRDT literature — CRDT commutativity rejected per ADR-010; order conventions
  borrowed
- Neo4j APOC `apoc.diff.graphs` — summary shape inspired `ApplySummary`; not used directly
