# Three-Way Merge Orchestration

`three_way_merge` coordinates validation, entity and edge reconciliation, dangling-reference checks, and deterministic output construction. `ThreeWayMergeEngine` exposes the same algorithm through the `MergeEngine` trait.

## `three_way_merge`

The function accepts a common `base` archive, the local `ours` archive, the remote `theirs` archive, and a `SnapshotMergeStrategy`. All inputs are borrowed and the result owns either a merged archive or its conflicts.

Every strategy first validates four input invariants:

1. all archives have the same namespace;
2. every edge weight is finite;
3. entity IDs and edge IDs are unique within each archive; and
4. semantic edge keys `(source, target, relation)` are unique within each archive.

Namespace, weight, duplicate-entity, and duplicate-edge-key failures have dedicated `MergeError` variants. A duplicate edge ID or an edge relation that cannot be reconstructed is reported as `MergeError::Internal`.

## Automatic strategy

`SnapshotMergeStrategy::Auto` follows this order:

1. classify and merge entities;
2. classify and merge edges;
3. validate all merged edge endpoints against the merged entity IDs;
4. return `MergeResult::Conflicts` when any entity, edge, or dangling conflict exists; otherwise
5. build and deterministically sort a `MergeResult::Clean` archive.

The conflict result does not include a provisional archive. The lower-level entity merge does retain an ours-side fallback for some conflicted entities, but the top-level function returns only the accumulated conflicts whenever any remain.

## Ours and theirs strategies

`Ours` and `Theirs` are last-write-wins shortcuts. They select the preferred branch's versions, retain additions unique to the other branch, sort and timestamp the result, and still run dangling-edge validation. A shortcut therefore returns `Conflicts`, rather than incorrectly returning `Clean`, if its composed archive references a missing entity.

`apply_theirs` is defined by swapping the branch arguments to `apply_ours`, which keeps the two shortcut policies symmetric.

## Deterministic output

Clean entities are sorted by UUID. Edges are sorted by source UUID, target UUID, relation string, and edge UUID. The output timestamp is the later of `ours.exported_at` and `theirs.exported_at`, so equal inputs do not acquire a wall-clock-dependent value.

The clean archive takes its format, version, and namespace from `ours` after validation has established namespace agreement.

## `ThreeWayMergeEngine`

`ThreeWayMergeEngine` is a stateless `MergeEngine` implementation whose `merge_branch` method delegates directly to `three_way_merge`. It is intended to replace the VCS layer's no-op engine when semantic merge is wired into production.
