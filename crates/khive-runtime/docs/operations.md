# Operations — Design Notes

`operations.rs` composes storage capabilities into the runtime's user-facing verbs (create, get,
list, search, link, traverse, query, recall, etc.). This document collects design rationale that
doesn't belong as inline comments: why the file isn't split into submodules, and the fault-injection
testing infrastructure it hosts. The in-source comments carry only short pointers here.

## Why this file is not split into submodules

All verbs share internal helpers (namespace checks, edge validation, canonical-endpoint logic)
that require `pub(crate)` access — splitting into submodules would require `pub(crate)`
re-exports across every helper or circular dependencies, and inline tests exercise those private
helpers directly. Split plan: once the verb surface stabilises post-retrieval-refactor, group by
substrate (entity, note, edge, search) into submodules under an `operations/` directory.

## Fault-injection arm migration

Namespace-targeted fault injection uses scoped guards. The former `arm_fts_fail`,
`arm_fts_fail_many`, `arm_fts_fail_many_partial`, and `arm_vector_fail` names were removed in
favor of their `_scoped` variants so stale statement-form calls fail to compile. Statement-form
arming cannot be preserved because dropping the returned guard at the semicolon disarms an
unconsumed injection.

## Concurrency and correctness notes

### atomic_hard_delete_with_edge_purge

The endpoint row delete and the incident-edge cascade used to run as two independently-committing
storage calls. A concurrent guarded write (`upsert_edge_guarded`/`upsert_edges_guarded`) landing
between them could see the endpoint still live, insert a fresh edge against it, and then survive
the cascade that already ran — a durably dangling edge with no second purge. Routing both
statements through one `run_atomic_unit` call closes the window: since every write (this one and
the guarded insert) funnels through the same single-writer queue, a concurrent guarded write
either fully commits before this unit starts (and its edge is then swept by the purge below, in
the same transaction as the row delete) or fully commits after this unit has already committed
(and its own endpoint-existence check then sees the endpoint gone and refuses the write) — there
is no state in which it can observe the endpoint alive with edges already purged.

### merge_traversal_paths_by_root

`traverse` queries every namespace in the token's visible set independently — including
namespaces that don't own the root at all, which still contribute a root-only entry when
`include_roots` is set — and each per-namespace call already enforces `limit` on its own results.
Concatenating them naively would let a root visible in N namespaces return up to N * limit nodes,
would keep whichever namespace's copy of a shared node happened to arrive first (wrong
depth/`via_edge` when that wasn't the shortest path, non-BFS ordering), and would rebuild a
seen-set from scratch per namespace (quadratic in namespace count). The merge keys by
`(root_id, node_id)`, keeps the node's shallowest depth and the `via_edge` that produced it
(first-namespace-processed wins ties at equal depth — deterministic but not otherwise decidable
which tied edge is "more correct"), reorders BFS-style (ascending depth), and re-applies `limit`
to the merged non-root node count.

### update_edge_symmetric_dml

DML text is the single source of truth shared with the atomic `prepare_update_edge` symmetric
branch (`khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL` /
`EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL` / `EDGE_SYMMETRIC_UPDATE_INPLACE_SQL`): this function
binds them against `rusqlite::params!` (it runs inside an existing transaction on a borrowed
`&rusqlite::Connection`), while the atomic path binds the same text via `SqlValue` plan params —
see the constants' doc comment in `khive-db` for why a single bridge type isn't used for both.

## Fault-injection static state

Several `thread_local!`/`static` items in this file back the test-only fault-injection surface
(`cfg(any(test, feature = "fault-injection"))`), gated out of production/published binaries.
External integration test crates enable it via `khive-runtime = { ..., features =
["fault-injection"] }`.

- `LINK_FAIL_AFTER` (test-only): failure injection for `create_note_inner`.
- `VECTOR_FAIL_AFTER`: count-targetable vector-INSERT fault. When set to `N` (N > 0), the next N
  vector insert calls (entity or note, single- or multi-model) succeed and the (N+1)-th returns an
  injected error, then the counter resets to 0. `thread_local!` gives per-thread isolation
  (`#[tokio::test]` uses a current-thread runtime, so there's no thread migration mid-test),
  letting a test fail one specific model's insert in a multi-model fan-out without depending on
  `VECTOR_FAIL_NS`'s namespace match.
- `FTS_FAIL_NS` / `VECTOR_FAIL_NS` / `ENTITY_COMPENSATION_FAIL_NS` / `FTS_FAIL_MANY_NS` /
  `FTS_FAIL_MANY_PARTIAL_NS`: namespace-keyed one-shot arm sets, not a single `Option<String>`
  slot. `create_note_inner` and `create_entity_inner` share `FTS_FAIL_NS`/`VECTOR_FAIL_NS`, and a
  single-slot design let a concurrently running test's `arm_fts_fail_scoped(other_ns)` overwrite
  this test's armed namespace before its own create call consumed it, so the intended injection
  silently never fired (#1095). Keying by namespace fixes that at the root — arming `ns_B` inserts
  `ns_B` without evicting `ns_A`. These are process-wide (not thread-local) so a caller may arm on
  one OS thread and run the triggering `create_note`/`create_entity` on another (e.g. via
  `tokio::spawn` on a multi-thread runtime); the check-and-remove under the mutex lock keeps
  exactly-once semantics even under concurrent same-namespace creates. `FTS_FAIL_MANY_NS` /
  `FTS_FAIL_MANY_PARTIAL_NS` are separate from `FTS_FAIL_NS` so `create_note_inner` and
  `create_many` tests cannot disarm each other (#1263) — the "partial" variant returns
  `Ok(BatchWriteSummary)` with `failed > 0` so the `summary.failed > 0` rollback branch is
  exercised, distinct from the hard-`Err` variant.
- `ENTITY_COMPENSATION_FAIL_NS`: entity-create compensation failure injection. The matching
  compensation skips only the entity-row delete; FTS/vector cleanup still runs so tests can
  inspect the exact residual state and combined error contract.
