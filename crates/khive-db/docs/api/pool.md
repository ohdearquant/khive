# Connection Pool

`ConnectionPool` (`crates/khive-db/src/pool.rs`) owns the SQLite connection(s)
for a single database file and, when the write queue is enabled, the one
`WriterTask` that serializes every mutating statement through it. This is the
function-specific technical reference for the pool's private/internal
mechanics and the tests that pin them down; see `crates/khive-db/docs/design.md`
("Single-Writer Write Queue") for the ADR-067 rationale.

## `ConnectionPool::writer_task_handle` — single-writer-task rationale

See `crates/khive-db/src/pool.rs` — `writer_task_handle`.

Exactly one writer task exists per `ConnectionPool` (per DB file) no matter
how many stores or namespaces are constructed over it: the `OnceLock` runs
its init closure at most once, so concurrent callers either race to run it
once or block on the in-flight init and then all receive a clone of the same
resulting handle. This is what makes the write queue an actual
single-writer core rather than one writer task per store — a per-store
writer task would let concurrent migrated stores over the same pool spawn
independent writer connections that contend with each other at `BEGIN
IMMEDIATE`, defeating the point of ADR-067 Component A.

## File-backed write-capacity reserve

Every read-write file pool owns one immutable `WriteCapacityGuard`. The
default `KHIVE_DB_DISK_RESERVE_BYTES=1073741824` retains one GiB on the
database volume; `0` is an explicit opt-out for disposable scratch stores.
The guard takes a fresh `fs4::available_space` sample before first-open
creation/write-intent PRAGMAs, after acquiring the pooled writer mutex, before
a tracked standalone writer open, and before every dequeued writer-task
request. Raw SQL writer methods also sample at their logical call boundary, so
a long-lived boxed handle cannot rely solely on its connection-open sample.

A sample at or below the reserve returns typed
`SqliteError::DiskCapacityFloor` before SQLite executes the write. It does not
increment writer-acquisition counters and is not a busy/lock/timeout result.
The MCP wire form is `kind="resource_exhausted"`,
`code="sqlite_disk_reserve"`, with `available_bytes`, `reserve_bytes`, and no
fabricated timeout or retry-after value.

One narrow recovery exception applies to a retained standalone raw-SQL
writer: an exact `ROLLBACK` statement head bypasses the sample. This covers
both `ROLLBACK` and `ROLLBACK TO [SAVEPOINT] name`, allowing an already-open
transaction or savepoint to consume the headroom that admission reserved.
`BEGIN`/`START`, `COMMIT`/`END`, `SAVEPOINT`, `RELEASE`, and DML still fail at
the floor. The normal single-statement parser remains in force, so a rollback
cannot smuggle a second statement through the exception.

With a nonzero reserve, the configured database must be a filesystem path.
`file:` SQLite URIs are rejected rather than probing a guessed/decoded path;
use the equivalent filesystem path. A URI remains available only when the
caller explicitly disables the guard for a disposable store.

Filesystem paths use the same canonical resolver as the pool's `DbIdentity`.
That includes following a dangling final-component symlink before first open:
for `link.db -> /other-volume/real.db`, the permanent capacity probe is the
canonical parent of `real.db`, never the directory containing `link.db`.
Path-bound test samples pin this target/probe identity so a fake byte count
cannot let wrong-volume logic pass unnoticed.

The pool exposes no cloneable raw writer connection in production. Such a
handle could be retained and its mutex acquired after capacity changes,
bypassing the post-lock admission check. The only raw clone is compiled under
`cfg(test)` for retirement-authorizer quarantine coverage.

The check is an admission snapshot, not a filesystem quota. One very large
transaction or an external process can consume capacity after sampling; size
the reserve above the largest expected in-flight transaction. Until the
single-owner topology in ADR-150 lands, independent processes can also pass
simultaneous samples. If SQLite nevertheless returns `SQLITE_FULL`, khive logs
an ERROR-class escalation at the SQLite mapping/transaction boundary;
that event is never flattened into ordinary timeout telemetry.

The causal Linux acceptance lane mounts a private 32 MiB tmpfs and holds an old
reader while WAL writes consume it. The production `fs4` sampler must refuse at
the reserve with real free space remaining; a reserve-disabled control then
reaches primary `SQLITE_FULL` on that same mount. Checkpoint, diagnostic, and
migration paths also escalate raw FULL before their ordinary warning, response,
or versioned-error handling.

Infrastructure-only `open_standalone_writer_untracked` remains unguarded so a
checkpoint/diagnostic connection can exist under pressure. Those connections
must not issue request DML: the writer task rechecks before every request, and
checkpoint/diagnostics retain their read/recovery-only contracts.

## Test coverage notes

### `writer_guard_transaction_registers_during_closure_only`

ADR-091 Plank 0: `WriterGuard::transaction` registers an entry with the
shared open-transaction registry for the duration of the closure, and
deregisters it once the closure (and its commit/rollback) completes.

`#[serial(tx_registry)]`: the open-transaction registry is a process-wide
singleton (`khive_storage::tx_registry`) shared across every test in this
binary. This test filters by its own unique label so it is not vulnerable to
another test's entry being reported as "oldest", but it still shares the
same `tx_registry` serial group as `checkpoint.rs`'s and `sql_bridge.rs`'s
registry tests for defense-in-depth against cross-test interference.

## Raw SQL bridge handle budget

File-backed `SqlBridge` handles keep their standalone connection for the
handle lifetime, preserving connection-local behavior across calls. The pool
therefore owns two shared permit sets across every bridge constructed over it:
reader handles are capped at the effective `max_readers` (with a minimum of
one in degraded mode), and writer handles are capped at one. Acquisition waits
only for `checkout_timeout` and then returns `StorageError::Timeout`. Dropping
an idle boxed handle releases its permit. Once an operation has entered
`spawn_blocking`, its connection and permit travel together; cancelling the
awaiting task retains both until SQLite finishes and drops the resource, so a
detached blocking call cannot escape the cap.

Cancelling an in-flight call also permanently invalidates a STANDALONE
reader/writer handle: the call takes the boxed handle's connection on entry
and only a completed await returns it, so every subsequent call on the same
standalone handle returns a "connection already consumed" error. Callers
that cancel or time out such a call must drop the handle and acquire a
fresh one. A QUEUE-BACKED writer handle is different by design: its writes
route through the writer task (no boxed connection to lose), and its reads
lazily reopen a read-only connection under a reader permit, so a cancelled
queue-backed read is followed by a successful reopen on the next read
rather than a hard failure. The reopen is still bounded by the reader
permit budget: the cancelled call's connection and permit travel into the
detached blocking task and stay held until SQLite finishes, so while a
detached cancelled read holds the LAST reader permit, the reopen times out
on the reader budget (typed `Timeout`, `sql_bridge.reader_handle`) and
succeeds only once the detached read completes and releases the permit
(pinned by `cancelled_inflight_queue_backed_read_reopens_after_detached_read_completes`).

The manual `atomic_unit` path (write queue flag off, or no writer task
available) acquires the same one-permit writer budget before opening its
standalone writer. A live `writer()` handle therefore makes such an
`atomic_unit()` wait and time out after `checkout_timeout`, and vice versa:
do not hold a boxed writer handle across an `atomic_unit()` call on the same
pool — drop the handle first. With the write queue enabled, `atomic_unit`
runs inside the writer task instead and never touches this budget.

When the write queue is enabled, `writer()` is queue-first (ADR-136 D1
gate 1): it opens no standalone connection and holds no writer permit, and
every mutating call routes through the writer task. Reads through such a
queue-backed writer handle lazily open a standalone READ-ONLY connection on
first use (`SqliteWriter::ensure_conn`) and hold a pool-wide reader permit
for it, so a queue-backed handle can never execute DML on an untracked
read-write connection. The one-permit writer budget therefore counts only
standalone read-write writer handles — the flag-off/degraded `writer()`
path and the manual `atomic_unit` path above. Hold a writer handle only for
a burst of operations, then drop it.

The optional writer task owns its separate, fixed connection and is not a
caller-held SQL bridge handle. Store-specific standalone connections are also
outside this raw-SQL handle budget; their write ownership remains governed by
ADR-067 and ADR-135.

`query_page` materializes at most the caller's requested `page.limit` rows on
the SQLite bridge, but it does not impose a server-side maximum. Callers own
choosing sane limits; large offsets and expensive query plans can still make
SQLite do substantial engine work before those bounded rows are returned.

These caps are NOT a global SQLite connection cap: they bound live
caller-held bridge handles only. The pool's own reader queue and writer
connection, the writer task's fixed connection, store-specific standalone
connections, and diagnostics/checkpoint connections all sit outside the
budget and are not counted against it. Both semaphore capacities
(`sql_bridge_reader_slots` at the effective reader count,
`sql_bridge_writer_slots` at one) are fixed at pool construction
(`ConnectionPool::new`) and never resized; the budget for a database file is
whatever the pool that owns it was built with.

## `execute_batch` transaction-control rejection and handle poisoning

Every `execute_batch` implementation rejects transaction-control statements
(`BEGIN`, `COMMIT`, `END`, `ROLLBACK`, `SAVEPOINT`, `RELEASE` — matched
case-insensitively at the statement head, tolerating leading whitespace and
`--`/`/* */` comments) with a typed `StorageError::InvalidInput` BEFORE
executing anything:

- The queue-backed path runs inside the writer task's own `BEGIN
  IMMEDIATE`; a caller `COMMIT` would close the task's transaction and
  terminate the writer task permanently.
- The standalone path wraps the caller list in its own `BEGIN IMMEDIATE`; a
  caller `COMMIT` would commit early, and a later statement failure would
  roll back only the tail — breaking all-or-nothing.
- The in-memory pool-backed path and the `InlineWriter` used by
  `atomic_unit` enforce the same rejection, since both run under an owned
  transaction boundary.

Rejected batches execute nothing, leave the handle untouched, and the handle
remains fully reusable.

Two further poisoning paths apply to the standalone (file-backed)
`execute_batch`, both dropping the handle instead of restoring it. Every
subsequent call on the poisoned handle then fails with the generic
"connection already consumed" pool error — callers must drop the handle and
acquire a fresh one, exactly as for cancellation invalidation above:

- **Failed ROLLBACK.** A statement failed and the error path's `ROLLBACK`
  also failed, leaving the connection's transaction state unknown. The
  returned error carries the poison context ("ROLLBACK after statement
  failure failed: ...") alongside the ORIGINAL statement error (never
  masked by the rollback failure alone). Pinned by
  `failed_rollback_poisons_handle_reuse_fails_loud`, which forces the
  failed ROLLBACK via a connection authorizer that denies the rollback
  transaction operation.
- **Non-transient BEGIN failure.** `BEGIN IMMEDIATE` failed with anything
  other than SQLite busy/locked (which is transient contention and restores
  the handle as reusable); the connection's transaction state is suspect.
  The returned error carries the poison context ("BEGIN IMMEDIATE failed
  non-transiently; connection transaction state is suspect") alongside the
  original BEGIN error. Pinned by
  `non_transient_begin_failure_poisons_handle` and
  `busy_begin_failure_restores_handle_reusable`.
- **COMMIT failure followed by successful ROLLBACK.** The transaction's
  `COMMIT` failed, cleanup `ROLLBACK` succeeded, and the handle is restored as
  reusable while the COMMIT error is surfaced to the caller. A failed
  ROLLBACK is the separate poisoning case above; the distinction keeps a
  recoverable connection available.

### `writer_task_handle_fails_loud_without_tokio_runtime`

ADR-067 Component A runtime-handle guard: `write_queue_enabled` is set but
the calling thread has no Tokio runtime context, so spawning the writer task
(which requires `tokio::spawn`) is impossible. `writer_task_handle` must
return a clean typed error instead of panicking.

Deliberately a plain `#[test]` (no Tokio runtime) — mirrors
`writer_task::spawn_fails_on_in_memory_pool`'s shape: the failure must be
observable without ever entering an async context, since entering one here
would defeat the point of the test.
