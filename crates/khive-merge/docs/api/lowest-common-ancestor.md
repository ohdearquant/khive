# Snapshot Lowest Common Ancestor

The LCA subsystem finds the shared snapshot that should serve as the base of a three-way merge. Its storage-neutral reader contract keeps history traversal independently testable.

## `SnapshotReader`

Implementations return the parent snapshot ID for a supplied ID, or `None` at a history root or for an unavailable parent. The trait is `Send + Sync`, allowing production readers to be shared across runtime tasks.

## `find_lca`

If both IDs are equal, the function returns that ID immediately. Otherwise it walks the complete `ours` parent chain into a `HashSet`, then walks `theirs` from its starting ID until it reaches the first ID in that set. The first match on the theirs-side walk is the lowest shared ancestor for these single-parent histories.

The algorithm requires `O(D_ours + D_theirs)` metadata reads and `O(D_ours + D_theirs)` visited storage in the cycle-bounded worst case. Both walks maintain visited sets, so malformed cyclic histories terminate instead of looping forever.

Disjoint histories return `None`. The merge integration interprets that outcome as an empty `KgArchive` base; `find_lca` itself only reports ancestry and does not construct an archive.

Production wiring is expected to adapt `KhiveRuntime` snapshot metadata to `SnapshotReader`.
