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
rather than a hard failure.

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

### `writer_task_handle_fails_loud_without_tokio_runtime`

ADR-067 Component A runtime-handle guard: `write_queue_enabled` is set but
the calling thread has no Tokio runtime context, so spawning the writer task
(which requires `tokio::spawn`) is impossible. `writer_task_handle` must
return a clean typed error instead of panicking.

Deliberately a plain `#[test]` (no Tokio runtime) — mirrors
`writer_task::spawn_fails_on_in_memory_pool`'s shape: the failure must be
observable without ever entering an async context, since entering one here
would defeat the point of the test.
