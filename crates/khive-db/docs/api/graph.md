# Graph Store (Edges)

`SqlGraphStore` (`crates/khive-db/src/stores/graph.rs`) implements the
`GraphStore` capability trait over the `graph_edges` table. This is the
function-specific technical reference for its write-routing and the
endpoint-existence guards that back `link`'s atomic-unit safety.

## `with_writer` — WriterTask routing (ADR-067 Component A, Fork C slice 2)

See `crates/khive-db/src/stores/graph.rs` — private method `with_writer`.

Routes a single-row write through the pool-wide `WriterTask` when
`KHIVE_WRITE_QUEUE=1` and a handle is available; otherwise falls back to the
legacy standalone-connection / pool-mutex path. This is the ONE routing
point for every `with_writer` caller in this store (`upsert_edge`,
`delete_edge`, `purge_incident_edges`). `f` must be DML-only — on the
flag-on path it runs inside the WriterTask's own transaction, so a bare
`BEGIN IMMEDIATE` would violate SQLite's nested-transaction rule.
`upsert_edges` (the batch method) does its own flag check and returns early
on `Some`, so its fallback call into this helper only ever executes on the
flag-off path (`self.writer_task` is `None` by construction whenever that
call is reached) — no double-routing.

## `edge_insert_guarded_by_endpoints_statement` — commit-time endpoint guard (ADR-099 §B3)

See `crates/khive-db/src/stores/graph.rs` — `edge_insert_guarded_by_endpoints_statement`.

The atomic `link` op's variant of `edge_upsert_statement`. Shares the SAME
`EDGE_NATURAL_KEY_CONFLICT_SET` conflict-arm text — the two builders cannot
diverge on write behavior — but wraps the `INSERT` in a guarded `SELECT ...
WHERE EXISTS(...)` that re-probes both endpoints for existence INSIDE the
transaction, at commit time, rather than trusting prepare-time validation
alone.

This guard is atomic-`link`-specific, not an `edge_upsert_statement`
concern: `LinkPlan`'s own doc comment (`khive-runtime::atomic_plan`) records
why it must be commit-time, not prepare-time — a `link` op's async prepare
pass (`validate_edge_relation_endpoints`) can run and pass BEFORE an earlier
op in the SAME atomic unit (e.g. `delete(X, hard)`) removes that very
endpoint; only a commit-time, in-transaction guard closes that intra-batch
ordering hazard (ADR-099 acceptance criteria: `[delete(X, hard), link(A,
X)]` must fail, not silently create a dangling edge). Canonical `link` has
no equivalent need — it executes and commits standalone, with no other op's
write interleaved between its own validation and its own write.

## `batch_upsert_edges` — shared DML loop (ADR-067 Component A)

See `crates/khive-db/src/stores/graph.rs` — private fn `batch_upsert_edges`.

Shared by both the legacy (flag-off) and WriterTask-routed (flag-on)
`upsert_edges` paths. Issues no `BEGIN`/`COMMIT`/`ROLLBACK` itself — the
caller owns the enclosing transaction. All-or-nothing: the first row
failure returns `Err` immediately (matching the pre-existing `upsert_edges`
contract, unlike `upsert_entities`/`upsert_notes`'s partial-success
accounting) — the caller's transaction wrapper (either the legacy
`with_writer` closure or `WriteRequest::execute_and_reply`) issues the
ROLLBACK.

Per-row DML comes from `edge_upsert_statement` — the SAME builder singleton
`upsert_edge` calls (ADR-099 §B3): this function previously hand-wrote a
second, textually-independent copy of the natural-key conflict arms here,
the exact drift class the `EDGE_NATURAL_KEY_CONFLICT_SET` extraction was
meant to close for good — a future change to that constant would have
silently stopped reaching this batch path. `bind_params` is the same
`SqlStatement` -> rusqlite binding `upsert_edge` uses; there is now exactly
one literal for the edge natural-key conflict arms in the whole workspace.

## `edge_endpoints_exist` / `batch_upsert_edges_guarded` — batch endpoint pre-check (#769)

See `crates/khive-db/src/stores/graph.rs` — private fns `edge_endpoints_exist`,
`batch_upsert_edges_guarded`.

`edge_endpoints_exist` is a standalone existence probe for both endpoints of
a would-be edge, matching exactly the `WHERE EXISTS(...)` shape
`edge_insert_guarded_by_endpoints_statement` embeds in its own guarded
`INSERT`. Two call sites:

- `batch_upsert_edges_guarded` uses it to pre-check an entire batch, inside
  one write-locked transaction, before issuing any `INSERT` — SQLite's
  `BEGIN IMMEDIATE` holds the write lock for the whole closure, so nothing
  can delete an endpoint between this check and the batch's inserts.
- `SqlGraphStore::upsert_edge_guarded` uses it to name which endpoint(s)
  were missing after a refused single-row insert, in the SAME writer
  closure as the insert itself — this is what makes the resulting
  `MissingEndpoints` an in-transaction fact rather than a reconstruction
  from a later, separately-scheduled read.

`batch_upsert_edges_guarded` mirrors `batch_upsert_edges`'s legacy/WriterTask
split but pre-checks every edge's endpoints with `edge_endpoints_exist`
BEFORE issuing any `INSERT` — if any endpoint is missing, the function
returns immediately with `affected: 0` and issues no writes at all, so the
caller's enclosing transaction has nothing to roll back (#769). Only once
every edge has been confirmed does it fall through to the plain
`edge_upsert_statement` writes, identical to `batch_upsert_edges`. The
refusing entry's index and its `MissingEndpoints` are captured by this same
pre-check pass and returned as `GuardedBatchOutcome::refused` — the runtime
layer no longer re-probes endpoints after the fact.
