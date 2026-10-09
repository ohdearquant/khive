# Operations — Design Notes

`operations.rs` composes storage capabilities into the runtime's user-facing verbs (create, get,
list, search, link, traverse, query, recall, etc.). This document collects design rationale that
doesn't belong as inline comments: the module boundaries and fault-injection testing infrastructure.
The in-source comments carry only short pointers here.

## Module layout

`operations.rs` retains the shared types and remaining production verb bodies. Entity create and
claim live in `operations/entity_create.rs`; bulk preparation lives in `operations/entity_bulk.rs`.
`operations/entity_indexes.rs` prepares required indexes and attachments before admitted candidates
enter the writer. Public method paths are unchanged. Fault-injection state,
scoped guards and arming helpers live in `src/operations/fault_injection.rs`; the parent re-exports
the existing public and crate-visible APIs and imports only the private state its operations use.
The child exposes those internal items to its parent with `pub(super)` access.

Unit tests remain in `src/operations_tests.rs` under the same `operations::tests` module and retain
access to the parent helpers. Manifest route tests live separately in
`secret_gate_finalizer/entity_route_tests.rs` and enter the real runtime methods.

## Entity candidate finalization

`secret_gate_finalizer/entity_admission.rs` captures one immutable manifest snapshot and checks
the final candidate. Production snapshots are empty. Only namespace-scoped `cfg(test)` fixtures
can supply a match; there is no caller, configuration or environment switch for admission.
Ordinary updates preserve the legacy patch-only scan under an empty snapshot. A non-empty
snapshot scans the complete merged candidate; caller-supplied and carried reserved stamps
remain refused, including unchanged echoes.

For a match, `entity_transaction.rs` owns the stamped row, conditional insert or revision guard,
required effects and exemption audit inside the caller's outer atomic unit. Embeddings are
prepared before acquiring its writer. `Exempted` is observed only after commit acknowledgement.
Confirmed rollback allows a separate best-effort failure audit; an uncertain acknowledgement
retains the storage error. Failure-audit errors emit a redacted diagnostic through an independent
log sink. Cloned plans have independent failure state.

Direct source and finding ingest retain a captured `EntityCandidateContext` through early
preflight, final preparation and retries. The public facade returns either an untouched legacy
candidate, a committed admitted candidate, or conditional-write contention. It accepts no SQL,
manifest installation, caller stamp or audit-verb override. Source ingest keeps its bounded CAS
rebase; finding ingest keeps its legacy upsert for clean candidates and uses insert-if-absent
for admitted candidates. Notes, atomic prepare and administrative curation remain reservation-only.

## Fault-injection arm migration

Namespace-targeted fault injection uses scoped guards. The former `arm_fts_fail`,
`arm_fts_fail_many`, `arm_fts_fail_many_partial`, and `arm_vector_fail` names were removed in
favor of their `_scoped` variants so stale statement-form calls fail to compile. Statement-form
arming cannot be preserved because dropping the returned guard at the semicolon disarms an
unconsumed injection.

## Concurrency and correctness notes

### Legacy create/delete and conditional-ingest post-commit results

The report-returning runtime create/delete methods return the committed record or
delete result together with every failed post-commit stage. Older methods keep
their original Rust return types. If a post-commit stage fails, those methods
return a structured `RuntimeError::Khive` with `reason=post_commit_degraded`,
`committed=true`, `retryable=false`, the operation and committed `record_id`,
and a JSON array in `post_commit_degradations` (each entry has `stage` and
`error`). Its projected `domain_disposition` is `committed`. The caller must
reconcile that ID and failed stage rather than repeat the create/delete. The
report-returning methods remain the preferred API when a caller can handle the
committed value and diagnostics in one successful result.

Conditional note ingestion uses the same existing typed error after a newly inserted
note encounters FTS acquisition/upsert, embedding, vector-store acquisition or vector
insertion failures. `operation=try_create_note` identifies all three conditional-note
entry points. Healthy models continue, and a duplicate returns `None` without indexing.
The trusted `comm.ingest` handler preserves its committed acknowledgement and one inbox
wake while adding `post_commit_degradations`; precommit and unrelated errors still
propagate. This additive ingest response is specified in the 2026-10-05
amendment to [ADR-056](../../../docs/adr/ADR-056-channel-transport-layer.md).

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

This function runs inside an existing transaction on a borrowed `&rusqlite::Connection`, so it
binds SQL against `rusqlite::params!` rather than the `SqlStatement`/`SqlValue` plan shape used
elsewhere — see the constants' doc comment in `khive-db` for why a single bridge type isn't used
for both.

It binds `khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL` for the probe,
`EDGE_SYMMETRIC_DELETE_NONCANONICAL_GUARDED_SQL` for the absorbed arm, and
`EDGE_SYMMETRIC_UPDATE_INPLACE_SQL` for the in-place arm.

**Only the probe text is still shared with the atomic path.** The atomic `prepare_update_edge`
symmetric branch builds its two write statements from
`edge_symmetric_delete_if_conflict_statement` and
`edge_symmetric_absorb_or_update_inplace_statement`, which carry their own SQL. They are not
textual copies of the constants above: the atomic delete additionally requires a surviving row
at the natural key via an `EXISTS` subquery, and the atomic in-place statement is a single
two-armed `UPDATE` selected by `changes()` rather than a probe followed by a branch. Changing
one path's SQL therefore does **not** change the other's, and an edit to either has to be
mirrored deliberately.

`EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL` (unguarded) is bound by neither of these. It remains
in use by merge's predicate-based rewrites in `khive-runtime::curation`, which run inside their
own single-writer transaction.

## Fault-injection static state

The `thread_local!`/`static` items in `src/operations/fault_injection.rs` back the test-only fault-injection surface
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
- `PREFIX_RESOLVE_FAIL_NS`: storage-failure injection for `resolve_prefix_inner`, keyed by the
  scanned prefix string rather than a namespace — `resolve_prefix_unfiltered` and
  `resolve_prefix_unfiltered_including_deleted` pass `namespaces: None` by contract, so there is
  no namespace to key on. Armed via `arm_prefix_resolve_fail_scoped(prefix)`; the next call
  scanning that exact prefix returns an injected `StorageError::Timeout` instead of running the
  table scan, then disarms. Used to prove callers that resolve an id through a prefix (e.g. the
  `get` verb's fallback chain) distinguish a storage fault from a genuine no-match instead of
  reporting both as not-found.
