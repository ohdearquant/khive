# State persistence: durable mutation and namespace loading

Source: `crates/khive-pack-brain/src/persist.rs`. Covers how `BrainState` mutations are
made durable (issues #457/#458) and how a namespace's state is cold-loaded/swapped.

## `persist_brain_state_mutation`: why `atomic_unit`, not a manual transaction

This deliberately does not issue a manual `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` sequence on
a plain `SqlWriter` handle (the trait's former `begin_tx`/`SqlTransaction` surface, retired
entirely, was likewise not an option): under `KHIVE_WRITE_QUEUE=1` that sequence would nest
inside the WriterTask's own per-request `BEGIN IMMEDIATE`, which SQLite rejects ("cannot
start a transaction within a transaction" — the same class of bug `fold_gate.rs`'s
`atomic_unit` conversion fixed, see `crates/khive-pack-brain/docs/api/fold-gate.md`).
Handing the whole append+upsert unit to `atomic_unit` instead means the WriterTask's own
transaction wrapping provides the atomicity on the flag-on path, and `run_manual_atomic_unit`
(khive-db) preserves the old manual-transaction shape byte-for-byte on the flag-off/in-memory
path.

## Why `persist_brain_state_mutation` takes `&dyn SqlAccess`, not `&KhiveRuntime`

The only thing this function ever needed from the runtime was its `SqlAccess` handle
(`KhiveRuntime::sql()`). Narrowing the parameter lets tests exercise this function against
a bare `SqlBridge`/`ConnectionPool` (write-queue-enabled via a `PoolConfig` literal,
mirroring `fold_gate.rs`'s routing test) without needing a full file-backed `KhiveRuntime`
and its associated `KHIVE_WRITE_QUEUE` env-var race across this crate's test binary.
