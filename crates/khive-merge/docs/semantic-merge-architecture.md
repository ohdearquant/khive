# Semantic Merge Architecture

`khive-merge` is the forward-deployed semantic merge layer for `KgArchive` snapshots. It exists because graph records have identities and invariants that a line-oriented text merge cannot represent reliably.

## Why semantic merge exists

The current v1 `khive-vcs` path merges sorted NDJSON lines. That is sufficient for its deployed snapshot, branch, push, and pull surface, but it cannot reason directly about entity fields, edge weights, symmetric relations, or references to deleted entities.

This crate implements the ADR-010 v2 direction: compare both branches to a common snapshot, reconcile entity and edge meaning, and report typed conflicts. It preserves edge UUIDs across cycles and validates dangling references after the entity result is known.

## Forward-deployment status

The crate is implemented and tested but is not yet registered in a production pack. Promotion depends on the ADR-020 VCS integration surface exposing snapshot ancestry through a `SnapshotReader`-compatible adapter. At startup, that integration will register `ThreeWayMergeEngine` in place of the current no-op engine.

The merge types live here rather than in `khive-vcs` because VCS currently ships only the v1 surface. They can move to a shared crate when v2 is promoted.

## Decomposition

| Module                 | Responsibility                                          | Detailed reference                        |
| ---------------------- | ------------------------------------------------------- | ----------------------------------------- |
| `merge`                | validation and top-level orchestration                  | `docs/api/three-way-merge.md`             |
| `types`                | strategies, results, conflicts, errors, engine trait    | `docs/api/conflict-and-error-taxonomy.md` |
| `lca`                  | cycle-safe common-ancestor discovery                    | `docs/api/lowest-common-ancestor.md`      |
| `diff_local`, `entity` | merge-specific entity classification and reconciliation | `docs/api/entity-merge.md`                |
| `edge`                 | semantic edge reconciliation and dangling checks        | `docs/api/edge-merge.md`                  |
| `strategy`             | ours/theirs shortcut composition                        | `docs/api/three-way-merge.md`             |

`diff_local` intentionally implements only the categorized entity and edge changes consumed by merge. It is not a general bidirectional graph-diff format. If a standalone `khive-diff` crate supplies that contract, this private implementation can be replaced by the dependency.

## Load-bearing choices

Namespace isolation prevents cross-tenant graph composition. Finite edge weights prevent NaN or infinity from destabilizing equality and ordering. Stable edge UUIDs preserve provenance through repeated merges. Deterministic sorting and timestamps make equal inputs produce equal serialized results.

Conflicts remain data rather than errors because a name disagreement or modify/delete race is a legitimate state for a well-formed three-way merge. Invalid archives, by contrast, are rejected before any diff is computed.
