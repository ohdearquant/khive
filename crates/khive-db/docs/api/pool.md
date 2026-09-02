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

## Pooled reader routing and raw-SQL exception budget

Every typed store read uses `ConnectionPool::reader_until` for both file-backed
and in-memory databases. A file-backed `SqlAccess::reader()` is only a logical
handle: constructing or retaining it opens no SQLite connection. Its ordinary
`query_row`, `query_all`, and `query_page` calls check out one pooled reader for
the operation and return it only after statement finalization, callback cleanup,
and connection reset/replacement. Ordinary reads through a queue-backed
`SqlWriter` use the same route. There is no standalone fallback after pool
saturation.

The pool-wide reader admission budget is the effective `max_readers` (minimum
one in degraded in-memory mode). Pooled guards and the explicit raw-SQL
transaction exception below share that budget, so their combined live work
cannot exceed it. A real wait that exhausts `checkout_timeout` before work
begins returns retryable `StorageError::AdmissionTimeout`; cancellation or an
expired request context returns non-retryable `StorageError::Timeout`. Raw-SQL
ordinary reads name `sql_bridge.reader_operation`; typed stores retain their
capability operation name.

`KHIVE_CHECKOUT_TIMEOUT_SECS` configures `checkout_timeout` (default five
seconds) for each reader-admission attempt. One operation can perform several
sequential reads, and each operation-scoped checkout has its own bounded wait;
the caller's total wall time can therefore exceed five seconds even though no
single admission wait does. The timeout never triggers a fresh connection open.

### Closed standalone-reader exceptions

`ConnectionPool::open_standalone_reader` is crate-private and requires a
`StandaloneReaderPurpose`; there is no ordinary-request variant. The closed
list is:

- an explicitly requested, multi-call deferred raw-SQL read transaction;
- boot-time schema/model-registry inspection that runs before a runtime pool
  can own the read;
- a diagnostic that genuinely requires an independent snapshot.

The current PASSIVE `db_diagnostics` probe is not the third case: PASSIVE may
backfill WAL frames and therefore deliberately uses its separately documented,
untracked standalone writer. Adding any new standalone-reader purpose requires
changing the enum and the ADR exception list together.

For the raw-SQL transaction exception, `BEGIN`, `BEGIN TRANSACTION`, and
`BEGIN DEFERRED [TRANSACTION]` lazily open one standalone reader and retain one
reader-admission permit. Subsequent queries reuse that connection and permit;
`COMMIT`/`END` or full `ROLLBACK` closes the connection and releases admission
only after SQLite returns to autocommit. A successful begin also registers a
backend-scoped `sql_bridge_cached_read_transaction` span in `tx_registry`.
Immediate/exclusive starts, `START`, nested `BEGIN`, `SAVEPOINT`, `RELEASE`,
and `ROLLBACK ... TO` remain `StorageError::InvalidInput`. The compatible
standalone-open timeout remains `StorageError::Timeout` at
`sql_bridge.reader_open`; it is counted in reader diagnostics.

An interrupted explicit transaction is rolled back before reuse. If rollback
or callback cleanup cannot prove a safe autocommit connection, it is closed and
the logical reader is poisoned, preserving the existing
"connection already consumed" failure on later calls. A successful terminal
control closes the exceptional connection, and the same logical handle returns
to pooled routing for its next ordinary query.

### Reader diagnostics

`db_diagnostics.reader_contention` is pool-scoped and resets only when the
`ConnectionPool` is reconstructed. It reports:

- `reader_admission_capacity` and the point-in-time
  `available_reader_admission_slots`;
- aggregate request `reader_acquisitions`, split into
  `pooled_reader_checkouts` and `standalone_reader_opens`;
- `infrastructure_standalone_reader_opens`, kept outside the request aggregate;
- `reader_checkout_timeouts`, including pooled and closed-exception admission
  waits but excluding cooperative cancellation;
- point-in-time and peak pooled checkouts, completed pooled checkouts, and the
  longest completed pooled hold in microseconds. Hold time includes reset or
  replacement because the connection is not reusable before that finishes.

These fields make a reader wait's shape observable end to end: capacity,
availability, active/peak work, bounded failures, and completed hold evidence
appear in one payload, so a caller can distinguish an admission wait that
resolves within its first window from one that times out. A flat
`standalone_reader_opens` counter across ordinary read verbs is ADR-166 G2's
route invariant.

### Writer handles and request cancellation

The manual `atomic_unit` path (write queue off or unavailable) and a standalone
`writer()` share a separate one-permit writer budget. A standalone read-write
writer preserves reads on its own connection for transaction visibility; each
reader-supertrait call also takes reader admission, but it is not a standalone
reader open. A queue-backed writer opens no standalone connection for ordinary
reads and uses the pool as described above; only its explicit deferred read
transaction takes the closed exception.

Request-owned reads install an exact-connection interrupt handle plus the
connection-global progress callback. Cancellation and the absolute deadline
stop SQLite VM work; the connection and permit remain owned until cleanup
finishes (or inside the detached worker after the bounded hard-cap path), so
admission cannot be returned ahead of live SQLite state. Prepared-statement
classification remains authoritative: only `Statement::readonly()` statements
outside transaction control register for interruption. DML-with-RETURNING,
transaction control, `execute_batch`, atomic units, and admitted writes preserve
their completion result.

`query_page` materializes at most the caller's requested `page.limit` rows but
does not impose a server-side maximum. Large offsets or expensive plans can
still make SQLite do substantial work before those bounded rows return.

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
