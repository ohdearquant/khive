# khive-merge

Three-way semantic merge for KG snapshots — entity identity, field-level conflict
detection, edge weights, and dangling-edge validation.

This is the v2 semantic merge layer, distinct from the v1 line-merge that
[`khive-vcs`](https://crates.io/crates/khive-vcs) performs today over sorted NDJSON. Where
the v1 path merges text lines, `khive-merge` understands `KgArchive` structure: it
categorizes entity and edge changes between a common ancestor and two branches, applies
field-level conflict rules, and reports conflicts as data rather than failing the merge.

## Usage

```rust
use khive_merge::{ThreeWayMergeEngine, MergeEngine, MergeResult, SnapshotMergeStrategy};
use khive_runtime::portability::KgArchive;

fn merge(base: &KgArchive, ours: &KgArchive, theirs: &KgArchive) {
    let engine = ThreeWayMergeEngine;
    match engine
        .merge_branch(base, ours, theirs, SnapshotMergeStrategy::Auto)
        .expect("namespaces match and all weights are finite")
    {
        MergeResult::Clean { merged } => {
            println!("merged cleanly: {} entities", merged.entities.len());
        }
        MergeResult::Conflicts { conflicts } => {
            println!("{} conflicts need manual resolution", conflicts.len());
        }
    }
}
```

`SnapshotMergeStrategy::Ours` / `Theirs` skip field conflict detection and apply a
last-write-wins shortcut, then report `DanglingEdge` conflicts if the shortcut output
would reference missing endpoints. `SnapshotMergeStrategy::Auto` runs the
full three-way algorithm: entity pass, edge pass, dangling-edge validation, deterministic
sort, and returns `MergeResult::Conflicts` if anything needs a human. The free function
`khive_merge::three_way_merge(base, ours, theirs, strategy)` is the same algorithm without
going through the `MergeEngine` trait object.

## Conflicts

`MergeConflict` enumerates what `Auto` can detect: `NameConflict`, `KindConflict`,
`PropertyMismatch`, `ModifyDelete` (one branch edited, the other deleted),
`DuplicateAddition` (both branches added the same UUID with different content),
`EdgeModifyDelete`, and `DanglingEdge` (a merged edge references an entity not in the
merged set). `BranchSide::{Ours, Theirs}` identifies which branch a `ModifyDelete` /
`EdgeModifyDelete` change came from.

## Invariants

- **Namespace isolation** — `base`, `ours`, and `theirs` must share one namespace, or the
  merge is rejected with `MergeError::NamespaceMismatch` before any diff is computed.
- **Finite weights only** — a non-finite edge weight (`NaN`/`Infinity`) on either branch
  aborts with `MergeError::InvalidEdgeWeight`, never silently coerced.
- **Deterministic output** — entities sort by UUID, edges by `(source, target, relation)`;
  repeated calls over equal inputs produce byte-identical `KgArchive` output.
- **Edge identity preserved** — a merged edge keeps the `edge_id` of the originating
  branch's edge rather than minting a fresh UUID.

## Where this sits

`khive-merge` implements `MergeEngine`, the trait a `khive-vcs` integration would call at
merge time once the VCS layer exposes snapshot ancestry (a `SnapshotReader`-compatible API
for the lowest-common-ancestor walk this crate's `lca` module performs). It depends on
`khive-runtime` (for `KgArchive`) and `khive-storage`.

**This crate is forward-deployed: it is excluded from the cargo workspace** (`crates/Cargo.toml`
`exclude = ["khive-merge"]`) **and not published to crates.io.** It compiles standalone via its
own `[workspace]` table (`cargo check -p khive-merge` works from its own directory), but no
production pack currently calls into it — the v1 path (`khive-vcs`, git line-merge over sorted
NDJSON) governs KG merges today. See `crates/khive-merge/docs/semantic-merge-architecture.md` for the full
promotion plan.

Relates to [ADR-010](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-010-kg-versioning.md)
(KG versioning strategy — the v1/v2 merge distinction) and
[ADR-020](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-020-git-native-kg-implementation.md)
(git-native KG implementation).

## License

Apache-2.0.
