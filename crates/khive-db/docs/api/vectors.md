# Vector Store

`SqliteVectorStore` (`crates/khive-db/src/stores/vectors.rs`) implements the
`VectorStore` capability trait over sqlite-vec `vec0` virtual tables. This is
the function-specific technical reference for its private write-routing
helpers and the SAVEPOINT/transaction-rollback guarantees the test suite
pins down; public-item contracts stay complete in their own doc-comments.

## `with_writer` / `with_writer_unmanaged` — WriterTask routing (ADR-067 Component A, Fork C slice 2)

See `crates/khive-db/src/stores/vectors.rs` — private methods `with_writer`,
`with_writer_unmanaged`.

`with_writer` routes a single-row write through the pool-wide `WriterTask`
when `KHIVE_WRITE_QUEUE=1` and a handle is available; otherwise it falls
back to the legacy pool-mutex path (`with_writer_unmanaged`).

This is the routing point for callers whose closure is DML-only
(`delete`/`vec_delete`, `delete_subjects`/`vec_delete_subjects`): on the
flag-on path the closure runs inside the WriterTask's own transaction, so a
bare `BEGIN IMMEDIATE` (or an inner `conn.unchecked_transaction()`) would
violate SQLite's nested-transaction rule. `insert`/`update` (which need
their own delete-then-insert atomicity), `insert_batch` (the batch method),
and `orphan_sweep` (ADR-067 Amendment 1) each do their own flag check and
return early on `Some`, routing a DML-only closure directly through the
WriterTask instead — their fallback calls into `with_writer` only ever
execute on the flag-off path (`self.writer_task` is `None` by construction
whenever those calls are reached), so there is no double-routing.

`with_writer_unmanaged` bypasses the WriterTask channel unconditionally
regardless of `KHIVE_WRITE_QUEUE`. Reserved for closures that manage their
own transaction — those cannot be sent through the WriterTask channel, which
already wraps every request in its own transaction. `orphan_sweep`'s
flag-off path (`Transaction::new_unchecked`, its own manual `BEGIN
IMMEDIATE`) is the only caller — on the flag-on path `orphan_sweep` routes a
DML-only closure directly through the WriterTask instead, since routing a
`Transaction::new_unchecked` through the channel would nest a transaction
inside the WriterTask's own transaction.

## `replace_vector_row_dml` — shared DELETE-then-INSERT replacement (#546)

See `crates/khive-db/src/stores/vectors.rs` — private fn `replace_vector_row_dml`.

`vec0` virtual tables do not support `INSERT OR REPLACE`, so every
replacement path (single-record insert/update, batch insert, and the
WriterTask-routed atomic upsert) deletes the prior row for `subject_id` then
inserts the new one. `subject_id` is the vec0 table's primary key, so this also
repairs stale namespace metadata. This function issues no
`BEGIN`/`COMMIT`/`SAVEPOINT` itself — the caller owns the enclosing
transaction or savepoint and its rollback semantics, so this can run equally
inside a plain `Connection`, an `unchecked_transaction()`, or a named
`SAVEPOINT`.

`failpoint_flag`, when `Some` in a `cfg(test)` build, is checked between the
DELETE and the INSERT so tests can force an error at that exact point and
assert the caller's rollback restores the prior row (no-worse-than-stale
guarantee). It is inert in release builds.

## `insert_batch` / `update` replacement tests

`insert_batch_replaces_cross_namespace_row` and
`update_replaces_cross_namespace_row` seed a subject under one namespace and
replace it under another. Both assert that the old namespace row disappears,
the replacement is readable under the new namespace, and the replacement
embedding remains searchable.

`insert_batch_cross_namespace_replacements_are_ordered` writes the same subject
twice in one batch under different namespaces. Both records succeed, and the
second record is the final readable value.

### True ROLLBACK TO SAVEPOINT sentinels (failpoint-driven)

The sentinel tests (`insert_batch_rollback_restores_deleted_stale_after_post_delete_insert_failure`
and its `update` counterpart) use a `cfg(test)` failpoint that fires AFTER a
successful same-namespace DELETE and BEFORE the INSERT. This means:
- The stale row is genuinely gone from the DB when the error fires.
- Only a correct ROLLBACK TO SAVEPOINT (or `tx.rollback`) restores it.
- Removing those rollback lines WILL make these tests fail.

Value-level failures (dim/finite/count) are rejected before the SAVEPOINT
opens, so there is no natural same-namespace path to reach a post-DELETE
INSERT failure through the public API. The failpoint is the only way to
produce this condition in a unit test without modifying production logic.

### `insert_batch_rollback_restores_deleted_stale_after_post_delete_insert_failure`

SENTINEL — stale row is restored when DELETE succeeds but INSERT is forced
to fail via the `cfg(test)` failpoint.

Setup: insert stale `(id_X, ns:a, vec1)`. Failpoint `FAIL_AFTER_DELETE` is
armed before the batch call. Batch: one record `(id_X, ns:a, vec2)` — same
namespace, correct dims, all finite — so the production DELETE genuinely
removes the stale row, then the failpoint fires before INSERT.

Expected: `ROLLBACK TO SAVEPOINT vec_batch_record` restores the stale row —
`batch_exists` finds id_X in ns:a, search with vec1 returns similarity
> 0.999 (not vec2), and `BatchWriteSummary` reports attempted=1, affected=0,
failed=1.

FAILURE MODE: deleting the `ROLLBACK TO SAVEPOINT vec_batch_record` line
from `insert_batch` makes this test fail — the stale row is gone.

### `update_rollback_restores_deleted_stale_after_post_delete_insert_failure`

SENTINEL — stale row is restored when DELETE succeeds but INSERT is forced
to fail via the `cfg(test)` failpoint.

Setup: insert stale `(id_X, ns:a, vec1)`. Failpoint `FAIL_AFTER_DELETE` is
armed before the update call. Call `update(id_X, ns:a, vec2)` — same
namespace, correct dims, finite: DELETE removes the stale row, then the
failpoint fires before INSERT.

Expected: `unchecked_transaction` rolls back, restoring the stale row —
`batch_exists` finds id_X in ns:a, search with vec1 returns similarity
> 0.999 (not vec2), and `update` returns `Err` (the injected error
propagates out).

FAILURE MODE: removing the transaction's rollback from `update` makes this
test fail — the stale row is gone.

### `insert_rollback_restores_deleted_stale_after_post_delete_insert_failure`

#546: the flag-off single-record `insert` path previously ran its own
inline DELETE+INSERT with no failpoint hook at all, so the post-delete
rollback guarantee was never exercised on this path (only `update` and the
batch/atomic-upsert helpers were covered). Now that `insert` routes through
the shared `replace_vector_row_dml` helper, the same failpoint must fire
here too and `unchecked_transaction` must roll back the DELETE, restoring
the stale row.

FAILURE MODE (pre-#546): this test could not even be written against the
old `insert` body — there was no failpoint hook to arm. Removing the shared
helper (or its transaction rollback) makes this fail.
