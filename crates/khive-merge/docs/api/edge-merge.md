# Edge Identity and Merge Rules

Edge merging treats `(source, target, relation)` as semantic identity while preserving the originating edge UUID. This separates conflict detection from the durable identity that must survive repeated diff and merge cycles.

## `EdgeKey::from_edge`

`EdgeKey` clones an edge's source, target, and relation string. For symmetric relations such as `competes_with` and `composed_with`, it canonicalizes endpoints to `(min, max)` as required by ADR-002. Reversing a symmetric edge therefore produces the same hash/equality key and is detected as a duplicate.

## `diff_edges`

Each semantic key is classified as `Added`, `Deleted`, `Unchanged`, or `Modified`. Added and modified changes retain the complete `ExportedEdge`, so one-sided changes pass through without regenerating identity or dropping properties and provenance.

The semantic key already governs source, target, and relation. Within one key, durable `edge_id`, weight, and properties participate in change classification; weight equality uses an absolute difference below `f64::EPSILON`. `created_at` and `updated_at` do not create a change by themselves. This matches entity merge's timestamp policy and prevents deterministic archive rebuild times from creating false conflicts. When a branch wins a merge-relevant change, its complete independent timestamp pair is carried with the record.

Keys are processed in source/target/relation order. Input archives are validated for duplicate keys by the top-level merge before this diff is used.

## `merge_edges`

The edge pass applies the following policies:

| Ours             | Theirs            | Result                                                                 |
| ---------------- | ----------------- | ---------------------------------------------------------------------- |
| unchanged        | unchanged         | base edge                                                              |
| added            | absent/unchanged  | complete added branch edge                                             |
| absent/unchanged | added             | complete added branch edge                                             |
| added            | added             | maximum weight and ours identity/timestamps; properties merge per key      |
| deleted          | deleted/unchanged | omit                                                                   |
| unchanged        | deleted           | omit                                                                   |
| modified         | unchanged         | complete modified ours edge                                            |
| unchanged        | modified          | complete modified theirs edge                                          |
| modified         | modified          | field-level three-way reconciliation                                   |
| deleted          | modified          | `EdgeModifyDelete`                                                     |
| modified         | deleted           | `EdgeModifyDelete`                                                     |

Double-modified records use these field policies:

- A weight changed by only one branch is retained exactly, including a decrease. Simultaneous weight changes retain the established maximum-weight policy.
- Object properties use base-aware three-way reconciliation per key, matching the existing entity-property policy: independent key changes are combined, a one-sided change or identical double change is retained, and divergent changes to the same key yield one payload-level `EdgePropertyMismatch` with ours as the provisional value for that key. `None` acts as an empty map; malformed legacy non-object payloads are reconciled atomically and conflict on divergent double changes.
- `edge_id` uses the same three-way rule. Divergent replacements yield `EdgeIdentityMismatch`; independently added records have no common durable identity and continue to prefer ours.
- Timestamps never trigger a modification or conflict. One-sided changes carry that branch's exact pair. A double-modified or double-added result deterministically carries ours' pair while reconciling the governed fields above.

`SnapshotMergeStrategy::Ours` and `Theirs` are the explicit resolutions for property or identity conflicts and retain the selected branch's complete record.

### Cross-key identity collisions

Per-key reconciliation above decides each semantic edge's UUID independently, so a branch-chosen identity (an added edge, or a one-sided/double modification that changes `edge_id`) can coincide with a durable UUID already used by a *different* semantic edge in the merged set. `merge_edges` reserves every UUID inherited unchanged from base first, then resolves branch-chosen UUIDs against that reserved set in deterministic key order: an edge whose own identity did not change always keeps it; a colliding branch-chosen identity falls back to that key's own base UUID and reports `EdgeIdentityCollision`. That fallback UUID must itself still be unclaimed — a chained collision (the fallback target was already taken by an earlier-sorted edge) is checked the same way as the initial attempt. When there is no unclaimed identity to fall back to, whether because the key has no base UUID or because its base UUID was already claimed, the edge is dropped from the merged set rather than duplicating another edge's UUID; the `EdgeIdentityCollision` conflict is the record of the drop.

## `validate_dangling_edges`

Dangling validation must run after entity merge, using the final entity-ID set. Each edge with a missing source or target yields `MergeConflict::DanglingEdge` with the missing endpoint. If both endpoints are missing, the source is reported first because validation uses an `if`/`else if` check.

Automatic and shortcut top-level strategies both perform this validation before returning `Clean`.
