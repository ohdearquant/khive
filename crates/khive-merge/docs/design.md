# khive-merge Design

**Status**: forward-deployed v2 infrastructure (not yet wired into production packs)

The v1 VCS surface (snapshot/branch/push/pull via `khive-vcs`) governs the current production
path. This crate implements the semantic merge layer that will be promoted when the VCS
integration layer is extended to expose snapshot ancestry for LCA walks.

## ADR Compliance

### ADR-010: KG Versioning

- The v1 merge path is a line-merge on sorted NDJSON, implemented in `khive-vcs`.
- `khive-merge` provides the v2 semantic merge that understands entity identity, field-level
  conflicts, edge weights, and dangling-edge semantics.
- Promotion to a workspace member depends on the VCS layer exposing snapshot ancestry via a
  `SnapshotReader`-compatible API.

### ADR-020: VCS Integration Layer

- `khive-merge` is not yet registered in any pack; the VCS integration surface must be extended
  before the `ThreeWayMergeEngine` can be wired into production.

### ADR-043: Three-Way Merge Conflict Taxonomy

- The `MergeConflict` enum (in `types.rs`) enumerates the six conflict kinds defined for
  the three-way merge: `NameConflict`, `KindConflict`, `ModifyDelete`, `PropertyMismatch`,
  `EdgeModifyDelete`, `DanglingEdge`.
- Property merge rules (per-key, both-set-same → keep, both-set-different → conflict, one-side →
  take that side) are implemented in `entity::merge_properties`.
- Auto-merge for duplicate-UUID additions uses scalars-ours-wins + tags-union logic.

### ADR-048: Edge Identity

- `edge_id` must be stable across merge/diff cycles; merged edges must not receive a fresh UUID
  when the originating branch's edge is identifiable.
- `diff_local::diff_edges` retains full `ExportedEdge` values (not just weights) in
  `EdgeChange::Added` entries specifically to preserve `edge_id`.
- `edge::build_edge` accepts `existing_id: Option<Uuid>` and calls `Uuid::new_v4()` only as a
  fallback when no originating edge is known.

## Scope

`khive-merge` implements a three-way semantic merge for `KgArchive` snapshots. It is
distinct from the v1 line-merge on sorted NDJSON: this crate understands
entity identity, field-level conflicts, edge weights, and dangling-edge semantics.

## Module Map

| Module | Role |
|--------|------|
| `types` | Public types: `SnapshotMergeStrategy`, `MergeConflict`, `MergeResult`, `MergeEngine` trait |
| `merge` | Top-level `three_way_merge()` + `ThreeWayMergeEngine` impl |
| `lca` | Lowest-common-ancestor walk over a `SnapshotReader` |
| `diff_local` | Private: entity and edge diff between base and branch |
| `entity` | Private: entity categorization and field-level conflict analysis |
| `edge` | Private: edge categorization and dangling-edge validation |
| `strategy` | Private: last-write-wins shortcuts (`Ours`/`Theirs`) |

## Key Invariants

1. **Namespace isolation**: `base.namespace == ours.namespace == theirs.namespace`. Violated → `VcsError::Internal`.
2. **Finite weights**: all edge weights must satisfy `f64::is_finite()`. NaN/Infinity → `VcsError::Internal`.
3. **Deterministic output**: entities sorted by UUID, edges sorted by (source, target, relation). Repeated calls with equal inputs produce identical output.
4. **Edge identity**: `edge_id` is preserved from the originating branch across merge/diff cycles.

## Failure Modes

- **Namespace mismatch**: cross-namespace merge rejected before any diff is computed.
- **Non-finite weight**: rejected at the input boundary; no silent coercion.
- **Conflict**: returned as `MergeResult::Conflicts` — not an error; caller decides resolution strategy.
- **Dangling edge**: merged edge references an entity not in the merged set → reported as `MergeConflict::DanglingEdge`.

## Consistency Notes

- `diff_local` intentionally does not implement the full bidirectional `GraphDiff` format
  that a standalone diff crate would expose; it only produces the categorized change sets
  needed by the merge algorithm. When a `khive-diff` crate ships, this module can be
  replaced by a dep on that crate.
- The `MergeConflict` and related types are defined in `khive-merge` rather than `khive-vcs`
  because the VCS crate ships only the v1 surface. They will move to a shared crate when
  v2 promotion occurs.

## Verification

```bash
# From the workspace root (or from crates/khive-merge directly):
cd crates/khive-merge
cargo check --manifest-path Cargo.toml
cargo clippy --manifest-path Cargo.toml -- -D warnings
cargo test --manifest-path Cargo.toml
cargo fmt --manifest-path Cargo.toml -- --check
```

All four must pass before promoting this crate to the workspace member list.
