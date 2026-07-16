# Edge Identity and Merge Rules

Edge merging treats `(source, target, relation)` as semantic identity while preserving the originating edge UUID. This separates conflict detection from the durable identity that must survive repeated diff and merge cycles.

## `EdgeKey::from_edge`

`EdgeKey` clones an edge's source, target, and relation string. For symmetric relations such as `competes_with` and `composed_with`, it canonicalizes endpoints to `(min, max)` as required by ADR-002. Reversing a symmetric edge therefore produces the same hash/equality key and is detected as a duplicate.

## `diff_edges`

Each semantic key is classified as `Added`, `Deleted`, `Unchanged`, or `WeightModified`. Weight equality uses an absolute difference below `f64::EPSILON`. Added changes retain the complete `ExportedEdge`, rather than only its weight, specifically so `edge_id` is not regenerated later. Weight modifications retain both weights for future diff display even though the current merge consumes the branch weight.

Keys are processed in source/target/relation order. Input archives are validated for duplicate keys by the top-level merge before this diff is used.

## `merge_edges`

The edge pass applies the following policies:

| Ours             | Theirs            | Result                             |
| ---------------- | ----------------- | ---------------------------------- |
| unchanged        | unchanged         | base edge                          |
| added            | absent/unchanged  | added edge with branch ID          |
| absent/unchanged | added             | added edge with branch ID          |
| added            | added             | one edge, maximum weight, ours ID  |
| deleted          | deleted/unchanged | omit                               |
| unchanged        | deleted           | omit                               |
| weight changed   | unchanged         | changed edge with ours ID          |
| unchanged        | weight changed    | changed edge with theirs ID        |
| weight changed   | weight changed    | maximum weight, preferring ours ID |
| deleted          | weight changed    | `EdgeModifyDelete`                 |
| weight changed   | deleted           | `EdgeModifyDelete`                 |

Maximum weight is the automatic last-write-wins policy for simultaneous weight changes. When rebuilding a modified edge, the implementation preserves an ID from the responsible branch; simultaneous changes prefer ours for deterministic identity. A fresh UUID is only a defensive fallback when no originating edge can be found.

Relation reconstruction can return `MergeError::Internal` if the semantic key's relation string no longer parses as a governed relation.

## `validate_dangling_edges`

Dangling validation must run after entity merge, using the final entity-ID set. Each edge with a missing source or target yields `MergeConflict::DanglingEdge` with the missing endpoint. If both endpoints are missing, the source is reported first because validation uses an `if`/`else if` check.

Automatic and shortcut top-level strategies both perform this validation before returning `Clean`.
