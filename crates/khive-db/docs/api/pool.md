# Connection Pool

`ConnectionPool` (`crates/khive-db/src/pool.rs`) owns the SQLite connection(s)
for a single database file and, when the write queue is enabled, the one
`WriterTask` that serializes every mutating statement through it. This is the
function-specific technical reference for the pool's private/internal
mechanics and the tests that pin them down; see `crates/khive-db/docs/design.md`
("Single-Writer Write Queue") for the ADR-067 rationale.

## WAL autocheckpoint ownership

Routine WAL reclamation has exactly two owners, selected by whether a dedicated
checkpoint task actually runs against the pool:

- **Claimed** — the scheduled ADR-091 checkpoint task calls
  `ConnectionPool::claim_checkpoint_ownership` at startup (and
  `propagate_checkpoint_claim_to_writer_task` for a writer task spawned before
  the claim). From then on every writer-capable connection sets
  `PRAGMA wal_autocheckpoint = 0`: the pool's startup writer is re-configured
  under the writer mutex, and every later open through the standalone boundary
  (store writers, raw SQL bridge writers, the writer task, diagnostics, the
  dedicated checkpoint connection) inherits the setting. Routine checkpoint
  I/O stays off application commit paths; the task's separately bounded
  TRUNCATE policy is unchanged.
- **Unclaimed** — no checkpoint task runs (embedded runtimes, one-shot CLI
  executions). Writer-capable connections keep a bounded autocheckpoint
  (4,000 pages), so SQLite's own reclamation prevents unbounded WAL growth
  and eventual disk exhaustion on writable pools that have no other
  checkpoint owner.

There is no environment or `PoolConfig` override in either direction: the only
way to move between the two postures is an actual ownership claim, which only
the scheduled checkpoint task makes. The standalone embedding-model registry
query is opened read-only and is not a third writer constructor.

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

## `ConnectionPool::writer_task_for_write` — write-time store routing (#1847)

Store construction is synchronous and can happen before a Tokio runtime is
entered. A constructor may therefore cache no writer-task handle even though a
later write runs inside a runtime where the pool can spawn (or retrieve) its
single task. Every SQLite-backed store resolves through
`writer_task_for_write` at the write seam, including transaction-owning and
batch entry points, so a construction-time `None` is never a permanent routing
decision.

When `write_routing_strict` is enabled, a missing handle fails closed: a
missing runtime preserves `StorageError::WriterTaskNoRuntime`, while an
explicitly disabled or degraded queue returns a typed `StorageError::Pool`
naming the operation. In compatibility mode, a caller may use its legacy
standalone/pool-mutex writer only after this lookup returns `None`; that exact
fallback emits a store-specific `direct_route_violation` when the file-backed
queue is enabled.

This routing hardening does **not** flip the strict-mode default. ADR-135 F2
and ADR-136 D2 still require production A/B evidence and a release gate before
`write_routing_strict` can become the default; until that evidence is accepted,
`KHIVE_WRITE_ROUTING=strict` remains opt-in.

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

## Raw SQL bridge admission and writer-handle budget

File-backed `SqlBridge` handles keep their standalone connection for the
handle lifetime, preserving connection-local statement/cache behavior across
calls and one explicitly admitted deferred read transaction. The pool
therefore owns two shared permit sets across every bridge constructed over it:
reader opens and active reads are capped at the effective `max_readers` (with a
minimum of one in degraded mode), and standalone writer handles are capped at
one. Acquisition waits only for `checkout_timeout` and then returns
`StorageError::Timeout`. An idle reader handle retains its cached connection
but no reader permit; a standalone writer retains its writer permit until the
handle drops. Once a read has entered `spawn_blocking`, its connection and
operation permit travel together; cancelling the awaiting task retains both
until SQLite finishes and drops the resource, so a detached blocking call
cannot escape the active-read cap.
Reader-open saturation reports `sql_bridge.reader_open`; active-query
saturation reports `sql_bridge.reader_operation`.

Cached read-only handles admit one explicit top-level deferred read transaction:
`BEGIN`, `BEGIN TRANSACTION`, and `BEGIN DEFERRED [TRANSACTION]` retain the
opening operation's reader permit, subsequent queries reuse it, and
`COMMIT`/`END` or full `ROLLBACK` releases it only after SQLite returns to
autocommit. Immediate/exclusive starts, `START`, nested `BEGIN`, `SAVEPOINT`,
`RELEASE`, and `ROLLBACK ... TO` are rejected with
`StorageError::InvalidInput`; `execute_batch` still rejects every transaction
control form. A terminal statement that fails while SQLite remains in the
transaction keeps the permit, allowing a later full rollback or safe handle
drop rather than exposing an unadmitted snapshot.

As a defense-in-depth postcondition, every ordinary cached-reader operation
must be in autocommit before its operation permit is released. Unadmitted state
is rolled back while the permit is still held; an uncleanable connection is
discarded first. The connection field precedes the explicit-transaction permit
in the owned handle, so cancellation or handle drop closes SQLite (and its WAL
snapshot) before returning admission. An idle cached reader can therefore
never retain a WAL snapshot outside the active-read budget.

`multiple_long_lived_idle_cached_readers_allow_bounded_checkpoint_progress`
is the #1828 integration contract: eight cached reader handles remain open
against a two-reader budget after each handle completes its one-shot read. The
test proves idle handles no longer retain admission while repeated writes and
the central ADR-091 PASSIVE checkpointer make complete frame progress and keep
the WAL bounded. Autocheckpoint is disabled in the fixture, so the result
neither depends on per-commit checkpoint I/O nor weakens #1848's single central
checkpoint-owner direction.

This fixture does not reproduce or close #1460 or #1812. An idle autocommit
connection does not pin WAL; those issues concern production stdio/multiprocess
pinning and continuous concurrent-session WAL bounds, respectively, and remain
open pending their own production-shaped regressions and fixes.

Cancelling an in-flight call also permanently invalidates a STANDALONE
reader/writer handle: the call takes the boxed handle's connection on entry
and only a completed await returns it, so every subsequent call on the same
standalone handle returns a "connection already consumed" error. Callers
that cancel or time out such a call must drop the handle and acquire a
fresh one. A QUEUE-BACKED writer handle is different by design: its writes
route through the writer task (no boxed connection to lose), and its reads
lazily reopen a cached read-only connection, so a cancelled
queue-backed read is followed by a successful reopen on the next read
rather than a hard failure. The reopen is still bounded by the reader
permit budget: the cancelled call's connection and permit travel into the
detached blocking task and stay held until SQLite finishes, so while a
detached cancelled read holds the LAST reader permit, the reopen times out
on the reader budget (typed `Timeout`, `sql_bridge.reader_open`) and
succeeds only once the detached read completes and releases the permit
(pinned by `cancelled_inflight_queue_backed_read_reopens_after_detached_read_completes`).

The manual `atomic_unit` path (write queue flag off, or no writer task
available) acquires the same one-permit writer budget before opening its
standalone writer. A live `writer()` handle therefore makes such an
`atomic_unit()` wait and time out after `checkout_timeout`, and vice versa:
do not hold a boxed writer handle across an `atomic_unit()` call on the same
pool — drop the handle first. With the write queue enabled, `atomic_unit`
runs inside the writer task instead and never touches this budget.
The cached-reader admission state does not apply to a standalone
read-write writer: its handle-scoped writer permit remains held across the
whole manual transaction, including reader-supertrait calls within that
transaction, while each such read additionally uses an active-reader permit.

When the write queue is enabled, `writer()` is queue-first (ADR-136 D1
gate 1): it opens no standalone connection and holds no writer permit, and
every mutating call routes through the writer task. Reads through such a
queue-backed writer handle lazily open a standalone READ-ONLY connection on
first use (`SqliteWriter::ensure_conn`) and cache it without a reader permit;
each ordinary query acquires the pool-wide reader permit for its blocking
operation, while an explicit deferred read transaction retains one permit
across its whole multi-call span.
The read-only open still ensures a queue-backed handle can never execute DML
on an untracked read-write connection. The one-permit writer budget therefore
counts only standalone read-write writer handles — the flag-off/degraded
`writer()` path and the manual `atomic_unit` path above. Hold a writer handle
only for a burst of operations, then drop it.

The optional writer task owns its separate, fixed connection and is not a
caller-held SQL bridge handle. Store-specific standalone connections are also
outside this raw-SQL handle budget; their write ownership remains governed by
ADR-067 and ADR-135.

`query_page` materializes at most the caller's requested `page.limit` rows on
the SQLite bridge, but it does not impose a server-side maximum. Callers own
choosing sane limits; large offsets and expensive query plans can still make
SQLite do substantial engine work before those bounded rows are returned.

These caps are NOT a global SQLite connection cap. The reader semaphore bounds
concurrent opens and active raw-SQL reads, not idle cached reader connections;
callers retaining many reader handles remain responsible for their process's
file-descriptor budget. The writer semaphore bounds caller-held standalone
read-write handles. The pool's own reader queue and writer connection, the
writer task's fixed connection, store-specific standalone connections, and
diagnostics/checkpoint connections all sit outside these budgets. Both
semaphore capacities (`sql_bridge_reader_slots` at the effective reader count,
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
