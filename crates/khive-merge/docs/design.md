# khive-merge Design

**Status**: forward-deployed v2 infrastructure (not yet wired into production packs)
**Authority**: ADR-010/ADR-020 govern the current v1 VCS surface. This crate implements
the semantic merge layer that will be promoted when the VCS integration layer is extended.

## Scope

`khive-merge` implements a three-way semantic merge for `KgArchive` snapshots. It is
distinct from the v1 line-merge on sorted NDJSON (ADR-010 §merge): this crate understands
entity identity, field-level conflicts, edge weights, and dangling-edge semantics.

## Module Map

| Module | Role |
|--------|------|
| `merge_types` | Public types: `MergeStrategy`, `MergeConflict`, `MergeResult`, `MergeEngine` trait |
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
