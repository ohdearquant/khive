# Pool / writer-task / backend internals

Long-form rationale extracted from `src/pool.rs` doc-comments. Public-item
contracts stay complete in the source; this file carries the "why" and
cross-references.

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

## `pool.rs` tests

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

### `writer_task_handle_fails_loud_without_tokio_runtime`

ADR-067 Component A runtime-handle guard: `write_queue_enabled` is set but
the calling thread has no Tokio runtime context, so spawning the writer task
(which requires `tokio::spawn`) is impossible. `writer_task_handle` must
return a clean typed error instead of panicking.

Deliberately a plain `#[test]` (no Tokio runtime) — mirrors
`writer_task::spawn_fails_on_in_memory_pool`'s shape: the failure must be
observable without ever entering an async context, since entering one here
would defeat the point of the test.
