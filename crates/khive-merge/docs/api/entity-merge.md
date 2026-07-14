# Entity Diff and Merge Rules

Entity merging classifies each UUID relative to the common base and reconciles the two branch classifications. Conflicts are field-specific so callers can distinguish identity disagreements from delete/modify races and property collisions.

## `diff_entities`

`diff_entities(base, branch)` returns one `EntityChange` per UUID present in either archive:

| Base    | Branch                 | Classification                         |
| ------- | ---------------------- | -------------------------------------- |
| absent  | present                | `Added` with the branch entity         |
| present | absent                 | `Deleted`                              |
| present | structurally equal     | `Unchanged`                            |
| present | structurally different | `Modified` with base and branch values |

Structural equality compares ID, kind, governed entity type, name, description, tags, and properties. It deliberately excludes creation and update timestamps. The modified form retains the base value for future “was → now” conflict displays even though the current merge patterns consume only the branch value.

UUIDs are processed in sorted order before insertion into the returned map, supporting deterministic downstream handling.

## `merge_entities`

The three-way merge applies these rules per UUID:

| Ours             | Theirs            | Result                                                              |
| ---------------- | ----------------- | ------------------------------------------------------------------- |
| unchanged        | unchanged         | base entity                                                         |
| added            | absent/unchanged  | added entity                                                        |
| absent/unchanged | added             | added entity                                                        |
| added            | added             | keep once if equal; otherwise `DuplicateAddition` and ours fallback |
| deleted          | deleted/unchanged | omit                                                                |
| unchanged        | deleted           | omit                                                                |
| modified         | unchanged         | ours entity                                                         |
| unchanged        | modified          | theirs entity                                                       |
| modified         | modified          | field-level merge                                                   |
| deleted          | modified          | `ModifyDelete`                                                      |
| modified         | deleted           | `ModifyDelete`                                                      |

For a conflicting double modification, the lower-level result retains the ours version as a provisional fallback while reporting every field conflict. The top-level automatic merge does not label that archive clean; it returns the conflict list for manual resolution.

## Field-level merge

Name and kind disagreements produce `NameConflict` and `KindConflict`. A governed `entity_type` disagreement uses `PropertyMismatch` with the key `entity_type` and JSON representations of both values.

Descriptions are annotations rather than identity, so an unequal description resolves to ours without a conflict. Tags are set-unioned and sorted. Properties are merged per key:

| Ours value       | Theirs value     | Result                                      |
| ---------------- | ---------------- | ------------------------------------------- |
| absent           | absent           | absent properties                           |
| object           | absent           | ours object                                 |
| absent           | object           | theirs object                               |
| equal values     | equal values     | one value                                   |
| different values | different values | `PropertyMismatch`; keep ours provisionally |
| absent key       | present key      | take theirs                                 |
| present key      | absent key       | keep ours                                   |

Non-object JSON property payloads are treated like absent property maps by this merge layer.

## Duplicate additions

When both branches add the same UUID, equality checks name, kind, entity type, description, properties, and tags. Tag order is normalized for this comparison. A mismatch reports the exact differing field names in `DuplicateAddition`; timestamps are not part of this duplicate-content comparison.
