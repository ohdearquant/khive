# Merge Results, Conflicts, and Errors

The merge API separates valid semantic conflicts from invalid inputs and internal failures. Callers resolve `MergeResult::Conflicts`; they handle `MergeError` as a failed merge attempt.

## `MergeResult`

`Clean` owns a complete merged `KgArchive`. `Conflicts` owns one or more `MergeConflict` values requiring resolution and intentionally carries no archive.

## `SnapshotMergeStrategy`

`Auto` performs semantic three-way conflict detection. `Ours` and `Theirs` are last-write-wins shortcuts, but both still reject invalid input and report dangling-edge conflicts. The type is named `SnapshotMergeStrategy` to avoid collision with the note-curation layer's `ContentMergeStrategy`.

## `MergeConflict`

| Variant             | Meaning                                                                 |
| ------------------- | ----------------------------------------------------------------------- |
| `NameConflict`      | both branches supply different entity names                             |
| `KindConflict`      | both branches supply different entity kinds                             |
| `PropertyMismatch`  | both branches supply different values for one property or `entity_type` |
| `ModifyDelete`      | one branch modifies an entity while the other deletes it                |
| `DuplicateAddition` | both branches add the same UUID with different content                  |
| `EdgeModifyDelete`  | one branch changes an edge weight while the other deletes it            |
| `DanglingEdge`      | a selected edge references an entity absent from the merged entity set  |

`BranchSide::Ours` denotes the local branch being merged into the common base; `Theirs` denotes the remote branch being incorporated.

## `MergeError`

| Variant             | Trigger                                                                            |
| ------------------- | ---------------------------------------------------------------------------------- |
| `NamespaceMismatch` | base, ours, and theirs do not share a namespace                                    |
| `InvalidEdgeWeight` | an input edge weight is NaN or infinite                                            |
| `DuplicateEntityId` | one archive repeats an entity UUID                                                 |
| `DuplicateEdgeKey`  | one archive repeats a semantic `(source, target, relation)` key                    |
| `Internal`          | duplicate edge UUID, relation reconstruction failure, or another invariant failure |

A conflict is not an error: it represents well-formed inputs whose changes need a policy or human decision. Invalid archives fail before diff computation.

## `MergeEngine`

`MergeEngine::merge_branch` accepts the common base and both branches plus a strategy, returning the same result/error split. Production VCS startup can register `ThreeWayMergeEngine` in place of the no-op implementation.
