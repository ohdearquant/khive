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
`upsert_edge_with_outcome`, `upsert_edge_guarded`, `delete_edge`,
`purge_incident_edges`). Single-statement callers pass DML-only closures; on
the flag-on path those run inside the WriterTask's own transaction, so a bare
`BEGIN IMMEDIATE` would violate SQLite's nested-transaction rule. Transactional
methods (`upsert_edge_with_outcome`, `upsert_edge_guarded`, `upsert_edges`, and
`upsert_edges_guarded`) first check the WriterTask and send it only their DML helper. Their
fallback call into `with_writer` is reached only when `self.writer_task` is
`None`, and its closure owns the explicit `BEGIN IMMEDIATE`/commit/rollback —
no nested transaction or double routing.

## `edge_insert_guarded_by_endpoints_statement` — commit-time endpoint guard (ADR-099 §B3)

See `crates/khive-db/src/stores/graph.rs` — `edge_insert_guarded_by_endpoints_statement`.

The guarded `link` variant of `edge_upsert_statement`, shared by canonical
singleton link and atomic-apply link. It shares the SAME
`EDGE_NATURAL_KEY_CONFLICT_SET` conflict-arm text — the two builders cannot
diverge on write behavior — but wraps the `INSERT` in a guarded `SELECT ...
WHERE EXISTS(...)` that re-probes both endpoints for existence INSIDE the
transaction, at commit time, rather than trusting prepare-time validation
alone.

Atomic apply needs the commit-time check because prepare can pass before an
earlier op in the SAME atomic unit removes the endpoint. Canonical link has
the equivalent cross-request window between async validation and its later
write, so it uses the same guarded statement. In both paths, endpoint facts
and the edge write share one writer transaction.

## `edge_upsert_returning` — atomic persisted row and disposition (#1761)

`edge_upsert_returning` first executes the candidate insert with `ON CONFLICT
DO NOTHING RETURNING ...`; a returned row proves creation. On conflict it
executes the canonical update/revival upsert with the same `RETURNING` columns,
which proves reuse and supplies the persisted row. Both write statements run
inside one writer transaction. This also classifies an id collision correctly;
comparing candidate and persisted ids alone would not. The returned edge and
boolean are therefore fixed before the writer transaction is released.

`upsert_edge_with_outcome` uses this form for coordinator writes whose target
lives on another backend. `upsert_edge_guarded` uses it after the endpoint
predicate, and `batch_upsert_edges_guarded` collects one outcome per input in
the same all-or-nothing transaction. None of these callers performs a later
natural-key read that a concurrent delete or relink could race.

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
every edge has been confirmed does it execute the ordinary
`edge_upsert_statement` SQL through `edge_upsert_returning`, preserving input
order while capturing each persisted id and disposition. The
refusing entry's index and its `MissingEndpoints` are captured by this same
pre-check pass and returned as `GuardedBatchOutcome::refused` — the runtime
layer no longer re-probes endpoints after the fact.
