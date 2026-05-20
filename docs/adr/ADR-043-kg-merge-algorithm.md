# ADR-043: KG Merge Algorithm — Three-Way Merge with Conflict Detection

**Status**: proposed\
**Date**: 2026-05-19\
**Authors**: Ocean, lambda:khive

## Context

ADR-015 specified the `merge_branch` MCP tool signature and named the five conflict types
(added-added, modified-modified, deleted-modified, edge-endpoint conflict, semantic contradiction).
ADR-017 specified the `GraphDiff` format and its conflict marker shape. ADR-042 introduced the
`MergeEngine` trait as the extension point.

What is still unspecified:

1. **LCA algorithm**: how does the system find the common ancestor of two branches efficiently?
2. **Diff-to-merge mapping**: given two diffs (`base→ours` and `base→theirs`), what is the exact
   algorithm for producing a merged diff or a conflict set?
3. **Conflict resolution strategies beyond "manual"**: the `strategy` parameter on `merge_branch`
   accepts `"auto"`, `"ours"`, and `"theirs"`, but the per-field rules for `"auto"` are not
   defined in detail.
4. **Property-level merge**: when both branches modify the same entity but different property keys,
   should the merge take both? When do they conflict?
5. **The `khive-merge` crate structure**: what functions and types does it expose?

This ADR fills these gaps. It is the implementation contract for `khive-merge`, the v0.5 crate
that implements `MergeEngine` for `khive-vcs`.

### Relationship to issues

This ADR addresses GitHub issue #1 (KG merge: combine two graph states with conflict detection).

### Relationship to existing ADRs

This ADR does not change any decision in ADR-015 or ADR-017. It adds precision to the merge
semantics described in both.

## Decision

### 1. Three-way merge inputs

The merge function takes three `KgArchive` instances:

```rust
pub fn three_way_merge(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
    strategy: MergeStrategy,
) -> MergeResult;

pub enum MergeStrategy {
    /// Compute diffs and auto-merge where safe. Report conflicts otherwise.
    Auto,
    /// Prefer `ours` on all conflicts. No conflicts reported; always produces a clean merge.
    Ours,
    /// Prefer `theirs` on all conflicts. No conflicts reported; always produces a clean merge.
    Theirs,
}

pub enum MergeResult {
    /// All changes merged without conflict. `merged` is the resulting archive.
    Clean { merged: KgArchive },
    /// One or more conflicts detected. No merged archive produced.
    /// The caller must resolve conflicts and call again (or use `Ours`/`Theirs` strategy).
    Conflicts { conflicts: Vec<MergeConflict> },
}
```

`base` is the least common ancestor of `ours` and `theirs` in the snapshot history. See §2 for
the LCA algorithm. The caller (in `khive-vcs`) is responsible for loading the three archives from
the database and passing them here.

### 2. Least common ancestor algorithm

**Decision: iterative walk of the parent chain using a bitset of visited snapshot IDs. O(D) where
D is the depth of the branch histories, with an early-exit when the LCA is found.**

```rust
pub fn find_lca(
    db: &dyn SnapshotReader,
    ours_id: &SnapshotId,
    theirs_id: &SnapshotId,
) -> Result<Option<SnapshotId>, VcsError>;
```

Algorithm:

1. Walk the `ours` parent chain, collecting all ancestor snapshot IDs into a `HashSet<SnapshotId>`.
   Stop at the genesis (no parent). This requires at most D reads from `kg_snapshots` where D is
   the depth of `ours`'s history.
2. Walk the `theirs` parent chain. The first ancestor ID that appears in the `ours` ancestors set
   is the LCA.
3. If no common ancestor exists (the two histories are completely disjoint — e.g., two independent
   genesis commits), return `None`. The merge proceeds with an empty base (`base = empty archive`).

**Complexity**: O(D_ours) for step 1 (HashSet build), O(D_theirs) for step 2 (linear scan until
hit). Total O(D_ours + D_theirs) snapshot metadata reads, each an indexed primary-key lookup on
`kg_snapshots.id` (the `idx_snapshots_parent` index from ADR-042 accelerates the parent walk).

**Why not Brent's algorithm or Git's paint-walk?** Git uses a multi-color ancestry walk to handle
diamond merges (multiple common ancestors). The khive branch model in v0.1 is linear (a branch is
a named pointer to a single HEAD; branches share a snapshot history but do not create diamonds
through parallel commits). The simple bitset walk is correct and sufficient. The paint-walk can be
introduced if diamond merges appear in v0.2 (via namespace forks).

**Memory cost**: the `HashSet` holds O(D_ours) SnapshotIds. Each `SnapshotId` is a 70-byte string
(`"sha256:"` + 64 hex chars). At 10,000 commits in `ours`'s history: 700 KB. Acceptable.

### 3. Diff computation (base → ours, base → theirs)

The merge algorithm computes two `GraphDiff` values internally:

```rust
let diff_ours   = compute_diff(base, ours);    // defined in khive-diff (ADR-017)
let diff_theirs = compute_diff(base, theirs);
```

`compute_diff` produces the 9-op diff format from ADR-017. When `khive-diff` is not yet linked
(v0.4), `khive-merge` includes a local implementation of `compute_diff` sufficient for the merge
algorithm. This local implementation is NOT the full `khive-diff` crate; it only needs to produce
ops for the merge use case (not `diff_summary`, not `apply_diff`).

### 4. Merge algorithm — entity-level

**Step 1: categorize each entity across the three archives.**

For each UUID that appears in any of base, ours, or theirs:

| In base? | In ours? | In theirs? | Category |
|---|---|---|---|
| No | Yes | No | Added in ours only → include in merge |
| No | No | Yes | Added in theirs only → include in merge |
| No | Yes | Yes | Added in both → `duplicate_add`; see §4.1 |
| Yes | Yes | Yes, same | Unchanged by both → include as-is |
| Yes | Yes (modified) | Yes, same as base | Modified in ours only → take ours |
| Yes | Yes, same as base | Yes (modified) | Modified in theirs only → take theirs |
| Yes | Yes (modified) | Yes (modified differently) | Modified in both → `property_conflict` or auto-resolve; see §4.2 |
| Yes | No (deleted) | Yes, same as base | Deleted in ours only → delete in merge |
| Yes | Yes, same as base | No (deleted) | Deleted in theirs only → delete in merge |
| Yes | Yes (modified) | No (deleted) | `modify_delete` conflict → report |
| Yes | No (deleted) | Yes (modified) | `modify_delete` conflict → report |
| Yes | No (deleted) | No (deleted) | Deleted in both → delete in merge (no conflict) |

#### 4.1 Duplicate add (same UUID in ours and theirs, absent in base)

Both branches created an entity with the same UUID. This should be rare (UUID collisions) but must
be handled.

Auto-resolution rule: field-by-field, with `ours` winning on scalar conflicts and tags being
unioned. This matches ADR-017's `duplicate_entity_add` auto-resolution.

#### 4.2 Modified in both — field-level conflict analysis

When the same entity is modified in both branches, the merge performs field-level conflict
analysis:

```rust
pub struct EntityChange {
    pub id: Uuid,
    pub base_name: Option<String>,      ours_name: Option<String>,  theirs_name: Option<String>,
    pub base_desc: Option<String>,      ours_desc: Option<String>,  theirs_desc: Option<String>,
    pub base_tags: Vec<String>,         ours_tags: Vec<String>,     theirs_tags: Vec<String>,
    pub base_props: serde_json::Value,  ours_props: serde_json::Value, theirs_props: serde_json::Value,
}
```

Field-level auto-resolution rules (all applied before conflict reporting):

| Field | Rule |
|---|---|
| `name` | Both changed to same value → take it. Different values → `name_conflict` (always report). |
| `description` | Both changed → `ours` wins (annotation, not identity). |
| `tags` | Both changed → union of both sets. |
| `kind` | Both changed to same value → take it. Different values → `kind_conflict` (always report). |
| `properties` key K | Only ours set K → take ours. Only theirs set K → take theirs. Both set K to same value → take it. Both set K to different values → `property_mismatch` (report unless strategy = Ours/Theirs). |
| `properties` key K | Only ours deleted K (key absent) → delete K in merge. Same for theirs. Both delete K → delete in merge. |

`name_conflict` and `kind_conflict` are always reported (never auto-resolved by `Auto` strategy)
because name and kind have identity semantics.

### 5. Merge algorithm — edge-level

Edge identity uses the composite key `(source_id, target_id, relation)` (ADR-017 §3).

For each composite key appearing in any of base, ours, or theirs:

| In base? | In ours? | In theirs? | Action |
|---|---|---|---|
| No | Yes | No | Added in ours only → include |
| No | No | Yes | Added in theirs only → include |
| No | Yes (weight W1) | Yes (weight W2) | Both added same edge; W1 == W2 → include. W1 ≠ W2 → `duplicate_edge_weight`; auto-resolve: `max(W1, W2)` |
| Yes | Yes (same weight) | Yes (same weight) | Unchanged → include |
| Yes | Yes (modified weight) | Yes (same as base) | Modified in ours only → take ours |
| Yes | Yes (same as base) | Yes (modified weight) | Modified in theirs only → take theirs |
| Yes | Yes (W1) | Yes (W2, W1 ≠ W2) | Both modified weight differently → `duplicate_edge_weight`; auto-resolve: `max(W1, W2)` |
| Yes | No | Yes (same as base) | Deleted in ours → delete in merge |
| Yes | Yes (same as base) | No | Deleted in theirs → delete in merge |
| Yes | Yes (modified) | No | `modify_delete` conflict → report |
| Yes | No | Yes (modified) | `modify_delete` conflict → report |
| Yes | No | No | Deleted in both → delete in merge |

**Edge endpoint validation**: after the entity merge produces the merged entity set, validate that
every edge in the merged edge set has both endpoints present. An edge whose source or target was
deleted in the merge is a `dangling_edge` conflict and must be reported.

### 6. Conflict types and data model

```rust
/// A conflict that prevents auto-merge from completing.
pub enum MergeConflict {
    /// Two different names for the same entity.
    NameConflict {
        entity_id: Uuid,
        ours: String,
        theirs: String,
    },

    /// Incompatible `kind` values for the same entity.
    KindConflict {
        entity_id: Uuid,
        ours: String,
        theirs: String,
    },

    /// Same property key set to different values in ours and theirs.
    PropertyMismatch {
        entity_id: Uuid,
        key: String,
        ours: serde_json::Value,
        theirs: serde_json::Value,
    },

    /// One branch modified an entity; the other deleted it.
    ModifyDelete {
        entity_id: Uuid,
        modified_in: BranchSide,     // Ours or Theirs
        deleted_in: BranchSide,
    },

    /// One branch modified an edge; the other deleted it.
    EdgeModifyDelete {
        source_id: Uuid,
        target_id: Uuid,
        relation: String,
        modified_in: BranchSide,
        deleted_in: BranchSide,
    },

    /// An edge in the merged set references a deleted endpoint.
    DanglingEdge {
        source_id: Uuid,
        target_id: Uuid,
        relation: String,
        missing_endpoint: Uuid,
    },
}

pub enum BranchSide { Ours, Theirs }
```

**Semantic contradiction** (two edges that together assert a falsehood, e.g., `A extends B` and
`A extends C` where B and C are incompatible) is deferred to v0.6. The conflict taxonomy above
covers structural conflicts only. Semantic analysis requires domain reasoning that cannot be
implemented in a general-purpose merge algorithm.

### 7. Conflict resolution — manual flow

When `merge_branch` returns `MergeResult::Conflicts`, the agent workflow is:

1. Inspect each conflict object. Each carries the entity/edge ID and both conflicting values.
2. For each conflict:
   - Use `update(kind="entity", id=..., ...)` to set the desired value.
   - Use `delete(kind="entity", id=...)` if the entity should be deleted.
   - Use `delete(kind="edge", ...)` or `link(...)` to resolve edge conflicts.
3. After all conflicts are resolved (all conflicting fields manually set to a consistent state),
   call `merge_branch` again with `force=true`. With `force=true`, the merge skips conflict
   detection and snapshots the current working state as the merge commit.

This is the "agent applies judgment" model from ADR-015 §D.3. The system provides structure; the
agent decides what is semantically correct.

### 8. Last-write-wins shortcut strategy

When `strategy = Ours` or `strategy = Theirs`, the merge skips conflict detection and applies a
simple field-selection rule: for every field that differs between the two modified states, always
take the field from the specified side. This is deterministic and always produces a `Clean` result.

Last-write-wins is appropriate for:

- Automated pipeline agents that cannot pause for human review.
- Merging machine-generated annotations where semantic correctness is less critical than merge
  speed.
- Testing and CI environments where deterministic merge results are required.

It is NOT appropriate for research KGs where a name conflict or property mismatch represents a
genuine disagreement about facts.

### 9. Property-level merge details

When both branches modified the same entity but different property keys, there is no conflict:

```
base.properties  = {domain: "attention", year: "2022"}
ours.properties  = {domain: "attention", year: "2022", type: "algorithm"}  // added "type"
theirs.properties= {domain: "attention", year: "2022", status: "published"} // added "status"
merged.properties= {domain: "attention", year: "2022", type: "algorithm", status: "published"}
```

When both branches modified the same key to different values:

```
base.properties  = {year: "2022"}
ours.properties  = {year: "2023"}      // changed
theirs.properties= {year: "2022-09"}   // changed differently
// → PropertyMismatch {entity_id, key: "year", ours: "2023", theirs: "2022-09"}
```

When both branches deleted the same key: no conflict, key is absent in the merge.

When one branch added a key the other deleted:

```
base.properties  = {}
ours.properties  = {draft: "true"}     // added
theirs.properties= {}                  // did not add (or deleted)
// → ours wins (added vs. absent = no conflict)
```

**No nested property merge**: properties are `serde_json::Value`, which can be objects. The merge
does NOT recursively walk nested objects. If both branches modified the same property key, the
entire value at that key is compared. If they differ, it is a `PropertyMismatch` even if the
nested objects only differ on one sub-key. This is correct for v0.1; deep-merge is a v0.2
extension.

### 10. `khive-merge` crate structure

```
crates/khive-merge/
├── Cargo.toml
└── src/
    ├── lib.rs         — re-exports; implements MergeEngine from khive-vcs
    ├── lca.rs         — find_lca(): snapshot ancestry walk
    ├── diff_local.rs  — minimal compute_diff() (entity+edge level, no property ops)
    ├── entity.rs      — categorize_entities() + field-level conflict analysis
    ├── edge.rs        — categorize_edges() + dangling edge validation
    ├── conflict.rs    — MergeConflict enum + BranchSide
    ├── strategy.rs    — apply_ours(), apply_theirs() (last-write-wins shortcuts)
    └── merge.rs       — three_way_merge() top-level function
```

`khive-merge` depends on: `khive-types`, `khive-storage` (for `SnapshotReader`), `khive-vcs`
(for `SnapshotId`, `KgArchive`, `MergeEngine` trait). It does NOT depend on `khive-diff` in v0.5;
`diff_local.rs` is a private implementation that only serves the merge use case.

When `khive-diff` ships in v0.4, a follow-up ADR can decide whether `khive-merge` should replace
`diff_local.rs` with a dependency on `khive-diff::compute_diff` or keep its private copy. The
private copy is simpler and avoids a circular-dependency risk.

### 11. Integration with `merge_branch` MCP tool

The `khive-vcs::merge_branch` operation:

1. Resolves `theirs` to a `SnapshotId` (branch HEAD or direct snapshot ID).
2. Loads the current branch HEAD as `ours_id`.
3. Calls `find_lca(db, &ours_id, &theirs_id)` → `base_id`.
4. Loads three `KgArchive` instances from `kg_snapshot_archives` (base, ours, theirs).
5. Calls `merge_engine.merge_branch(base, ours, theirs, strategy)`.
6. If `MergeResult::Clean`: restore the merged archive to the live namespace, call `commit`, return
   `{status: "clean", snapshot_id, ...}`.
7. If `MergeResult::Conflicts` and `force = false`: return `{status: "conflicts", conflicts: [...]}`.
8. If `force = true`: skip step 5–6. Snapshot the current working state directly (the agent has
   manually resolved conflicts). Return `{status: "clean", snapshot_id, ...}`.

### 12. Test plan

**Unit tests** (all in `khive-merge/src/`):

- `lca.rs`: LCA on linear history (trivial), on diverged branches with a common ancestor, on two
  disjoint histories (expect `None`).
- `entity.rs`: each of the 12 categorization rows in §4 produces the expected action/conflict.
- `entity.rs`: field-level merge for all `EntityChange` field combinations from §4.2.
- `edge.rs`: each of the 12 edge categorization rows in §5.
- `edge.rs`: dangling edge detection after entity deletion in merge.
- `conflict.rs`: conflict types serialize/deserialize correctly.
- `strategy.rs`: `Ours` strategy always produces `Clean`; `Theirs` always produces `Clean`.

**Integration tests** (in `khive-merge/tests/`):

- Three-archive merge with no conflicts: result equals expected merged archive.
- Three-archive merge with `PropertyMismatch`: returns correct conflict; resolving it and calling
  `three_way_merge` with `Ours` produces a clean result.
- `ModifyDelete` conflict: both variants (ours deletes / theirs deletes).
- `DanglingEdge` conflict: one branch adds an edge to an entity the other branch deletes.
- Full `merge_branch` MCP call via integration test against an in-memory runtime.

**Coverage target**: 90% line coverage on `khive-merge`.

## Rationale

### Why not CRDT-based automatic merge?

ADR-010 explicitly rejected CRDTs. The core problem is semantic contradiction: two independently
correct `extends` edges that together produce a falsehood cannot be detected by a CRDT. A CRDT
silently accepts both; the merge algorithm here detects and surfaces the contradiction
(`PropertyMismatch`, `NameConflict`, or future `SemanticContradiction`). For a research KG, silent
corruption is worse than a paused merge.

### Why is LCA a simple bitset walk and not Git's paint algorithm?

Git's paint algorithm handles the case where two branches have multiple common ancestors (merges
of merges, diamond histories). The khive v0.1 branch model creates diamond histories only if a
user explicitly merges a branch into two different branches and then merges those again. This is
rare in the solo-researcher-or-small-team use case. The simple walk is correct for the common case
and O(D) per branch. The paint algorithm can be introduced if diamond histories become common in
v0.2.

### Why is property-level merge flat (not recursive)?

Nested JSON object merge has unbounded complexity: two branches that each modify different keys at
depth 3 of a nested object require walking the entire subtree to determine whether there is a
conflict. For v0.1, properties are treated as atomic values (if the key matches, compare the whole
value). This is consistent with how `EntityPatch` works in ADR-014 (properties are wholesale
replaced, not incrementally patched). Recursive property merge is a v0.2 extension if agents need
it.

### Why is `khive-merge` a separate crate from `khive-vcs`?

The merge algorithm has different phasing than the basic VCS operations. `khive-vcs` ships in v0.4
(with `commit`, `branch`, `checkout`, `log`). `khive-merge` ships in v0.5. The `MergeEngine` trait
in `khive-vcs` decouples them: `khive-vcs` does not have a hard compile dependency on `khive-merge`.
If they were in the same crate, v0.4 users would either get an incomplete merge implementation or
have to wait for v0.5 to use `commit`/`branch`. Separate crates with a trait boundary lets each
phase ship independently.

### Why does `force=true` on `merge_branch` skip conflict detection?

When the agent has manually resolved all conflicts by calling `update`/`delete` on the live
namespace, calling `merge_branch` again with `force=true` snapshots the current state. Repeating
conflict detection at this point would require the system to re-identify which entities were
conflicted and verify they are now consistent — this is complex and fragile. The simpler contract
is: the agent resolves conflicts by editing the live state, then tells the system "I am done; commit
this." The system trusts the agent. This is correct for the "agent provides semantic judgment"
model.

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
|---|---|---|---|
| CRDT-based automatic merge | No conflicts to report; always produces a result | Silently wrong on semantic contradictions; ADR-010 rejected this | Safety requirement overrides convenience |
| Git paint-walk LCA | Handles diamond merge histories | Significantly more complex; not needed for v0.1 branch model | Premature generalization |
| Recursive property merge | Fine-grained conflict detection in nested JSON | Unbounded complexity; inconsistent with ADR-014's wholesale property replace semantics | Not worth the complexity |
| Single `khive-vcs` crate containing merge | Fewer crates | v0.4 users blocked on merge implementation before they can use `commit`/`branch` | Phasing requires independence |
| Auto-resolve all conflicts with last-write-wins by default | Fewer merge failures for agents | Silently corrupts research KG; violates "agent provides judgment" principle | Research KG quality requirement |
| Report semantic contradictions (e.g., conflicting edge relations) | More complete conflict detection | Requires domain reasoning; cannot be implemented in a general merge algorithm | Defer to v0.6 semantic analysis |

## Consequences

### Positive

- The merge algorithm is fully deterministic: the same three archives always produce the same
  `MergeResult`.
- Conflict types are structured Rust enums, not text strings — agents can programmatically inspect
  and respond to specific conflict types.
- The `MergeStrategy::Ours/Theirs` shortcut enables automated pipelines to merge without pausing.
- Property-level merge handles the common case (two branches modify different keys on the same
  entity) without a conflict — only genuine disagreements surface.
- `khive-merge` ships independently of `khive-diff` (local diff_local.rs handles the merge case).

### Negative

- Semantic contradictions (two edges that together assert a falsehood) are not detected by this
  algorithm. This is a known limitation; deferred to v0.6.
- The manual resolution flow (inspect conflicts → edit live state → `force=true` merge) requires
  multiple round-trips. This is intentional but may feel verbose for simple conflicts.
- Property merge is flat, not recursive. A single property key that holds a large nested object
  will conflict if any sub-key differs, even if the conflict is trivial.
- `diff_local.rs` in `khive-merge` is a private reimplementation of entity/edge diffing. If
  `khive-diff` ships with a different algorithm, there may be subtle discrepancies. Mitigated: when
  `khive-diff` ships, `diff_local.rs` can be replaced by a dependency on it in a follow-up PR.

### Neutral

- The LCA walk reads O(D) snapshot metadata rows (no archive blobs). With the primary key index on
  `kg_snapshots.id`, each read is sub-millisecond. A branch with 1,000 commits takes approximately
  1 second for the LCA walk — acceptable for a merge operation.
- The three-archive load (base, ours, theirs) reads three archive blobs from
  `kg_snapshot_archives`. At 60 MB per archive, this is 180 MB peak memory for the merge
  computation. Document as the v0.1 memory constraint.

## Open Questions

1. **Semantic contradiction detection**: ADR-015 listed `SemanticContradiction` as a future conflict
   type (two edges that together assert a falsehood). What is the right model? Rule-based (e.g.,
   "an entity cannot have both `extends X` and `supersedes X`")? Embedding-based (flag entity pairs
   whose property vectors are too close)? Domain-specific? This is a v0.6 research question.

2. **Merge of soft-deleted entities**: if base has entity E (live), ours has E (soft-deleted), and
   theirs has E (modified), the current categorization treats soft-delete as "still present" (since
   soft-deleted entities are excluded from the canonical hash). Should soft-deleted entities be
   excluded from snapshot archives entirely? v0.1 recommendation: yes — only live entities are
   snapshotted. This is consistent with the hash algorithm in ADR-042 §1. Document explicitly.

3. **Performance at 100K entities**: the current algorithm is O(N) in entity count and O(E) in
   edge count for the categorization pass. At 100K entities and 500K edges, this is CPU-bounded at
   approximately 2–5 seconds. Is this acceptable for v0.5? If not, the categorization pass can be
   parallelized trivially (entity categorization is embarrassingly parallel by entity ID partition).

4. **`force=true` safety**: currently `force=true` on `merge_branch` skips conflict detection and
   commits whatever is in the live namespace. This means an agent that calls `force=true` without
   having resolved conflicts will commit a conflicted state. Should there be a "pending conflict set"
   stored in `kg_vcs_state` that must be explicitly cleared before `force=true` is accepted? This
   would prevent accidental `force=true` abuse. Decision deferred — the v0.1 contract is
   "trust the agent."

5. **Merge commit message**: the auto-generated merge commit message is
   `"Merge branch '{theirs_name}' into '{ours_name}'"`. Should it include a conflict summary
   (e.g., "resolved 3 conflicts")? Useful for `log` output readability. Add in v0.5 when `log`
   supports richer commit metadata.

## Implementation Plan

### Phase 1 — Types and LCA (v0.5 prep)

- Scaffold `crates/khive-merge/` with `types.rs`, `conflict.rs`, `lca.rs`.
- Implement `find_lca` with full unit test coverage.
- Implement `MergeEngine` on `ThreeWayMergeEngine` (stub returning `NotImplemented` for all
  three-archive calls until entity/edge logic ships).

### Phase 2 — Entity merge (v0.5)

- Implement `categorize_entities` and field-level conflict analysis in `entity.rs`.
- Unit-test all 12 categorization rows and all field combination rules.

### Phase 3 — Edge merge and dangling validation (v0.5)

- Implement `categorize_edges` and `dangling_edge_check` in `edge.rs`.
- Unit-test all 12 edge categorization rows and dangling edge detection.

### Phase 4 — Top-level merge and strategy (v0.5)

- Implement `three_way_merge` in `merge.rs` composing entity + edge passes.
- Implement `apply_ours` / `apply_theirs` in `strategy.rs`.
- Integration test: end-to-end with in-memory `KhiveRuntime`.
- Achieve 90% line coverage.

### Phase 5 — Wire into `merge_branch` MCP tool (v0.5)

- Register `ThreeWayMergeEngine` in `khive-vcs` startup.
- Replace `NoOpMergeEngine` on startup when `khive-merge` feature is active.
- Integration test: full `merge_branch` MCP call path.

## References

- ADR-010: KG Versioning Direction (strategic context)
- ADR-015: KG Versioning Model (`merge_branch` tool signature; five conflict types named)
- ADR-017: Graph Diff Format (diff format; ADR-017 §6 defines `ConflictDiff` shape)
- ADR-042: KG Versioning Implementation (`MergeEngine` trait; `find_lca` interface; `SnapshotId`)
- ADR-014: Curation Operations (`update`, `delete`, `merge` — tools agents use to resolve conflicts)
- ADR-002: Closed Edge Ontology (13 canonical relations; edge categorization is bounded by this)
- ADR-001: Entity Kind Taxonomy (6 entity kinds; `KindConflict` is bounded by this)
- `crates/khive-runtime/src/portability.rs` — `KgArchive` type used as merge input
- Git documentation: three-way merge algorithm; LCA paint walk (inspiration, not imitation)
- Myers diff algorithm — inspiration for `diff_local.rs` entity categorization (not implemented
  here; a simpler O(N) set-difference is sufficient for entity lists)
