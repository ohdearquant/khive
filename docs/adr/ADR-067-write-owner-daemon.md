# ADR-067: Write-Owner Daemon — Single-Writer Task and Write Queue

**Status**: Proposed
**Date**: 2026-06-23
**Issue**: #195 (cross-op atomicity for `--ops-file` bulk apply); 2026-06-22 write-wedge incident
**Amends**: ADR-049 (khived daemon — adds write-queue layer inside the daemon process)
**Depends on**: ADR-049 (socket framing, forward_or_spawn protocol — unchanged by this ADR)

---

## Context

### Deployment model (scope boundary)

The deployment model addressed here is: one process serves one actor or a small set of
cooperating agents sharing a single database file. Each actor's database is a separate file
with a separate connection pool and a separate writer Mutex. The wedge described below is
therefore contained to a single process: multiple concurrent agents sharing one daemon process
and one pool. This ADR solves "one process, N concurrent agents, no wedge" and does not
address multi-process coordination (that is the per-process topology concern, addressed in
ADR-068).

### The WAL-starvation wedge

Under N concurrent agents sharing one daemon process, writes contend on SQLite's WAL write lock
through multiple independent code paths. The chain of evidence is as follows.

**Single writer Mutex in the pool** (`crates/khive-db/src/pool.rs:60`):

```rust
pub struct ConnectionPool {
    writer: Arc<Mutex<Connection>>,   // parking_lot, one connection
    readers: ArrayQueue<Connection>,
    ...
}
```

**Writer checkout with 5-second timeout** (`pool.rs:263`):

```rust
pub fn writer(&self) -> Result<WriterGuard<'_>, SqliteError> {
    let guard = self.writer
        .try_lock_for(self.config.checkout_timeout)   // checkout_timeout = 5s
        .ok_or_else(|| SqliteError::InvalidData(
            format!("timed out after {:?} waiting for sqlite writer connection", ...)
        ))?;
    Ok(WriterGuard { guard })
}
```

**BEGIN IMMEDIATE held for the full transaction** (`pool.rs:132`):

```rust
pub fn transaction<F, R>(&self, f: F) -> Result<R, SqliteError> {
    self.guard.execute_batch("BEGIN IMMEDIATE")?;
    match f(&self.guard) {
        Ok(result) => { self.guard.execute_batch("COMMIT")?; Ok(result) }
        Err(err) => { self.guard.execute_batch("ROLLBACK"); Err(err) }
    }
}
```

#### Write-path inventory

The pool Mutex is not the only write serialization point. The table below lists every distinct
write entry point in the current codebase (production / file-backed mode, which is what
`StorageBackend::sqlite` produces — `backend.rs:29,38` sets `is_file_backed: true`).

| #  | Entry point                                                                                                                                                                                                                                                                                                                                                                                        | File                                                 | file-backed path                                                                                                                                                                     |
| -- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 1  | `SqlEntityStore::with_writer`                                                                                                                                                                                                                                                                                                                                                                      | `stores/entity.rs:68-80`                             | pool Mutex (`pool.try_writer()`)                                                                                                                                                     |
| 2  | `SqlNoteStore::with_writer`                                                                                                                                                                                                                                                                                                                                                                        | `stores/note.rs:69-81`                               | pool Mutex (`pool.try_writer()`)                                                                                                                                                     |
| 3  | `SqlGraphStore::with_writer`                                                                                                                                                                                                                                                                                                                                                                       | `stores/graph.rs:111-130`                            | **standalone connection** (`open_standalone_writer`)                                                                                                                                 |
| 4  | `SqlFtsStore::with_writer`                                                                                                                                                                                                                                                                                                                                                                         | `stores/text.rs:130-149`                             | **standalone connection** (`open_standalone_writer`)                                                                                                                                 |
| 5  | `SqlEventStore::with_writer`                                                                                                                                                                                                                                                                                                                                                                       | `stores/event.rs:104-123`                            | **standalone connection** (`open_standalone_writer`)                                                                                                                                 |
| 6  | `SqliteSparseStore::with_writer`                                                                                                                                                                                                                                                                                                                                                                   | `stores/sparse.rs:133-145`                           | pool Mutex (`pool.try_writer()`) — no file-backed branch                                                                                                                             |
| 7  | `SqliteVecStore::with_writer`                                                                                                                                                                                                                                                                                                                                                                      | `stores/vectors.rs:217-229`                          | pool Mutex (`pool.try_writer()`) — no file-backed branch                                                                                                                             |
| 8  | `SqlBridge::writer()`                                                                                                                                                                                                                                                                                                                                                                              | `sql_bridge.rs:791-801`                              | **standalone connection** (`open_standalone_writer`)                                                                                                                                 |
| 9  | `SqlBridge::begin_tx()`                                                                                                                                                                                                                                                                                                                                                                            | `sql_bridge.rs:804-853`                              | **standalone connection** + `BEGIN IMMEDIATE`                                                                                                                                        |
| 10 | `SqlBridge::execute_batch()`                                                                                                                                                                                                                                                                                                                                                                       | `sql_bridge.rs:312-338`                              | runs on the connection owned by `SqliteWriter`, which is a standalone connection                                                                                                     |
| 11 | `curation::merge_entity`                                                                                                                                                                                                                                                                                                                                                                           | `curation.rs:316-326`                                | pool Mutex (`pool.writer()`) directly                                                                                                                                                |
| 12 | `curation::merge_note`                                                                                                                                                                                                                                                                                                                                                                             | `curation.rs:638-645`                                | pool Mutex (`pool.writer()`) directly                                                                                                                                                |
| 13 | `operations::link`                                                                                                                                                                                                                                                                                                                                                                                 | `operations.rs:3061-3073`                            | pool Mutex (`pool.writer()`) directly                                                                                                                                                |
| 14 | `StorageBackend startup/bootstrap writes` (`apply_schema`, `apply_pack_ddl`, `entity_store`, `graph_store`, `note_store`, `event_store`, `vec_store`, etc. **and** `register_embedding_model` at `backend.rs:392` — acquires pool writer at `:399`, INSERTs into `_embedding_models` at `:408`; called via `register_configured_embedding_models` at `config.rs:374`, invoked at `runtime.rs:132`) | `backend.rs:108,132,158,185,214,241,302,399,464,534` | pool Mutex (`pool.try_writer()`) directly; these are startup-only sequential writes (DDL and embedding-model registry DML) that run before the accept loop admits concurrent traffic |

Entries 3, 4, 5, 8, 9, and 10 open a **new standalone connection per operation** in
file-backed mode, each configured with `busy_timeout` (`sql_bridge.rs:141`,
`stores/graph.rs:76`, `stores/text.rs:120`, `stores/event.rs:94`). These standalone
connections compete for the SQLite WAL write lock through `busy_timeout` waits rather than the
Rust Mutex, but they produce the same contention outcome: concurrent writers stall waiting for
the lock, and WAL growth exacerbates the wait.

**Contention cascade under load**: two write-contention paths operate concurrently. Entries 1,
2, 6, 7, 11, 12, 13, and 14 block in `pool.try_lock_for(5s)`. Entries 3, 4, 5, 8, 9, and 10
open standalone connections and call `BEGIN IMMEDIATE` directly, blocking in `busy_timeout`
(30 seconds, `pool.rs:29`). Under sustained load from N agents, both paths degrade
simultaneously.

**WAL growth amplifier** (`pool.rs:15-16`):

```rust
const WAL_AUTOCHECKPOINT_PAGES: &str = "4000";   // ~16 MB threshold
const JOURNAL_SIZE_LIMIT_BYTES: &str = "67108864"; // 64 MB journal limit
```

WAL autocheckpoint requires no active readers. Under concurrent ANN recall, standalone read
connections (`sql_bridge.rs:101`) hold WAL read snapshots. Checkpoint is deferred. WAL grows
past 4000 pages. SQLite begins returning `SQLITE_BUSY` to new `BEGIN IMMEDIATE` attempts,
feeding additional latency back into the write queue. The system appears wedged: writes queue
behind busy-timeout cycles while long-running reads pin the WAL.

**Degraded mode amplifier** (`pool.rs:211`): when `max_readers == 0` (in-memory or WAL
unavailable), `pool.reader()` acquires the writer Mutex, creating a deadlock risk if any caller
holds a `WriterGuard` on the same pool.

**Daemon accept loop** (`crates/khive-runtime/src/daemon.rs:466`): the daemon accepts N
connections concurrently via `tokio::spawn` per connection. Each spawned task calls `dispatch()`
on the shared `KhiveRuntime`, which owns one `Arc<StorageBackend>` and one `ConnectionPool`.
All N tasks funnel writes through the paths in the inventory above without any application-level
queuing.

**Root cause (one sentence)**: under N concurrent agents, writes reach SQLite's WAL write lock
through multiple independent paths — some blocked on the pool Mutex, others issuing `BEGIN
IMMEDIATE` on standalone connections — while long-running reads pin WAL readers, starving
autocheckpoint, causing WAL growth that lengthens `busy_timeout` waits and can wedge the daemon.

### Issue #195 — cross-op atomicity for `--ops-file` bulk apply

`kkernel exec --ops-file` dispatches ops in chunks of 100 via `dispatch_request_local`
(`crates/kkernel/src/exec.rs:214`). The daemon fast-path is intentionally bypassed for bulk
apply (`exec.rs:408`). Each op within a chunk dispatches through its own verb handler, and each
handler acquires a write path independently and issues a separate `BEGIN IMMEDIATE` / `COMMIT`.
There is no rollback across ops within a chunk, and no rollback across chunks. A failure in op
47 of a 100-op chunk leaves ops 1 through 46 committed.

The `SqlAccess` trait (`ADR-005`) exposes a `begin_tx(options: SqlTxOptions)` method
(`sql_bridge.rs:804`) that opens a standalone connection, issues `BEGIN IMMEDIATE`, and returns
a `Box<dyn SqlTransaction>` supporting `commit()` and `rollback()`. This is the correct seam
for cross-op atomicity. Threading a transaction context through the verb handler dispatch chain
requires a non-trivial API change and is scoped below.

---

## Decision

Introduce a single write-owner task (`WriterTask`) inside the daemon. Concurrent write operations
reaching the daemon accept loop — including those currently using standalone connections (graph,
text, event, SqlBridge file-backed path) and those using the pool Mutex (entity, note, sparse,
vectors, curation, operations) — route their mutations through a bounded async channel to this
task. `WriterTask` is the sole caller of `BEGIN IMMEDIATE` for write traffic processed through
the accept loop. Reads remain concurrent and do not route through the write-owner task.

**Scope of the guarantee**: two write paths are explicitly excluded from this ADR and continue to
call `BEGIN IMMEDIATE` independently: (1) `SqlBridge::begin_tx()` (`sql_bridge.rs:834`), deferred
to the follow-up ADR for issue #195; and (2) startup/bootstrap writes in `backend.rs`
(`register_embedding_model`, DDL helpers), which run sequentially before the accept loop admits
any concurrent traffic and require no queuing.

The transport layer (ADR-049 Unix socket framing, `forward_or_spawn`, `DaemonRequestFrame`) is
unchanged. The redesign is internal to the daemon process and is transparent to all callers.

---

## Components

### Component A: Write queue and single writer task

A dedicated Tokio task (`WriterTask`) is introduced in `crates/khive-db/src/writer_task.rs`
(or as a module within `pool.rs` if the scope warrants co-location). This task is the
exclusive owner of the writer connection. It receives write requests over a bounded
`tokio::mpsc::channel` and is the only code path that calls `BEGIN IMMEDIATE` for write traffic
routed through the daemon accept loop. The two excluded paths — `SqlBridge::begin_tx()` and
startup/bootstrap writes — are detailed in the Decision section above and in Consequences.

**`WriteRequest` message shape**:

Store methods that migrate to the channel have heterogeneous return types: `upsert_entities`,
`upsert_notes`, `upsert_documents`, `append_events`, `upsert_batch`, and `insert_batch` all return
`BatchWriteSummary` (with `attempted`/`affected`/`failed`/`first_error` fields); `append_event`
and `upsert_edges` return `()` or `u64`; `SqlBridge::execute_batch` returns `u64`. A flat
`reply: oneshot::Sender<Result<u64, StorageError>>` cannot carry `BatchWriteSummary` without
losing the partial-success contract — `affected` and `failed` would be conflated into a single
rows-affected count, and `first_error` would be dropped. This is a breaking change to the
`khive-storage` trait surface and must not happen as a side effect of the write-queue migration.

`WriteRequest` therefore carries a typed closure and a type-erased reply channel:

```rust
type WriteOp = Box<dyn FnOnce(&rusqlite::Connection) -> Box<dyn std::any::Any + Send> + Send>;

struct WriteRequest {
    /// Closure that executes DML statements against the WriterTask's connection.
    /// The connection is already inside an outer BEGIN IMMEDIATE when this closure runs.
    /// The closure must NOT issue BEGIN / COMMIT / ROLLBACK; it may issue named SAVEPOINTs.
    op: WriteOp,
    /// Type-erased sender. The closure boxes its result; the store method downcasts it.
    reply: Box<dyn std::any::Any + Send>,
}
```

In practice, each store method constructs a concrete `WriteRequest<R>` where `R` is its natural
return type:

```rust
struct WriteRequest<R: Send + 'static> {
    op: Box<dyn FnOnce(&rusqlite::Connection) -> Result<R, StorageError> + Send>,
    reply: oneshot::Sender<Result<R, StorageError>>,
}
```

The channel is `tokio::mpsc::Sender<Box<dyn AnyWriteRequest + Send>>` where `AnyWriteRequest` is
a sealed trait whose only method is `execute_and_reply(&rusqlite::Connection)`. This lets the
`WriterTask` loop drain a homogeneous channel while each request carries its own typed reply.

The caller's side:

```rust
let (tx, rx) = oneshot::channel::<Result<BatchWriteSummary, StorageError>>();
channel.send(Box::new(WriteRequest { op: closure, reply: tx })).await?;
let summary = rx.await.map_err(|_| StorageError::Internal("writer task dropped".into()))??;
```

The `reply` sender delivers the result back to the originating async task with its full natural
type. Callers `await` the oneshot receiver and propagate the result as if they had called the
store method directly. No `BatchWriteSummary` fields are lost.

**Channel capacity and queue-full policy**: The channel is bounded. When the channel is full,
callers call `channel.send().await`, which applies backpressure: the caller suspends until
capacity is available. There is no immediate-error path on a full channel (no `try_send`);
callers that need a deadline should wrap the `send().await` in a `tokio::time::timeout`.
A timeout at that boundary returns `StorageError::WriteQueueFull { timeout_ms }`. The default
capacity is configurable via `PoolConfig` (recommended default: 256 pending operations).

**Failure mode table**:

| Condition                                      | Behavior                                                                                    |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------- |
| Channel full                                   | Caller blocks on `channel.send().await` (backpressure)                                      |
| Send timeout (caller wraps in `time::timeout`) | `StorageError::WriteQueueFull` returned to caller                                           |
| Writer task panic                              | Receiver drops; subsequent `send()` returns `SendError`; mapped to `StorageError::Internal` |
| Receiver drop (writer task stopped)            | Same as panic: `send()` returns `SendError`                                                 |
| `oneshot::Sender` drop before reply            | Caller's `recv()` returns `RecvError`; mapped to `StorageError::Internal`                   |
| Shutdown                                       | Writer task drains in-flight requests, replies to each, then exits                          |
| Ordering                                       | FIFO within a batch window; cross-batch order is not guaranteed                             |

**Complete write-path migration**: all **non-exempt concurrent accept-loop** write-path
inventory entries are migrated to route through the writer task channel. Two entries are
explicitly excluded and do **not** route through the channel: Entry 9 (`SqlBridge::begin_tx()`),
deferred to the #195 follow-up ADR, and Entry 14 (sequential startup/bootstrap writes), which
run before the accept loop and cannot race. Specifically:

**Complete `BEGIN IMMEDIATE` site inventory** (verified by `rg -n "BEGIN IMMEDIATE"
crates/khive-db/src/stores/ crates/khive-db/src/sql_bridge.rs`, excluding test files and
comments). Each site is dispositioned MIGRATE or EXEMPT with rationale:

| Site                | Function                                                                                        | Disposition                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| ------------------- | ----------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `entity.rs:325`     | `upsert_entities` (inside `with_writer` closure)                                                | **MIGRATE** — remove bare `BEGIN IMMEDIATE`; closure body becomes DML-only under WriterTask SAVEPOINT                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `note.rs:348`       | `upsert_notes` (inside `with_writer` closure)                                                   | **MIGRATE** — same pattern as entity                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `graph.rs:390`      | `upsert_edges` (inside `with_writer` closure)                                                   | **MIGRATE** — same pattern; entry 3 migrates the helper too                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `text.rs:330`       | `upsert_document` (inside `with_writer` closure)                                                | **MIGRATE** — single-doc write; closure becomes DML-only                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `text.rs:395`       | `upsert_documents` (inside `with_writer` closure)                                               | **MIGRATE** — batch write with per-doc inner SAVEPOINTs (partial-success); the inner SAVEPOINTs are preserved (SQLite allows nested named SAVEPOINTs); the outer `BEGIN IMMEDIATE` is removed and replaced by the WriterTask's outer transaction + per-request SAVEPOINT                                                                                                                                                                                                                                                                           |
| `text.rs:1139`      | `rename_namespace` (inside `with_writer` closure)                                               | **EXEMPT** — annotated `#[allow(dead_code)]`, no production callers anywhere in the codebase; forward-deployed infrastructure. Exempt pending a production call site; revisit when wired in                                                                                                                                                                                                                                                                                                                                                        |
| `event.rs:652`      | `append_event` (inside `with_writer` closure)                                                   | **MIGRATE** — single-event write; closure becomes DML-only                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `event.rs:667`      | `append_events` (inside `with_writer` closure)                                                  | **MIGRATE** — batch write returning `BatchWriteSummary`; outer `BEGIN IMMEDIATE` removed, the per-event loop is preserved. Existing semantics are **all-or-nothing** (NOT partial-success): the first failed event insert returns `Err` and rolls back the whole batch (`event.rs:670-674`), and a fully-successful batch returns `BatchWriteSummary { failed: 0 }`. The migration preserves this — an insert error remains a hard request error that rolls back the per-request SAVEPOINT; it is not converted to partial-success commit behavior |
| `sparse.rs:249`     | `insert_batch` (trait method; delegates to `insert_sparse_batch`, inside `with_writer` closure) | **MIGRATE** — partial-success loop; outer `BEGIN IMMEDIATE` removed                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `vectors.rs:355`    | `insert_batch` (inside `with_writer` closure)                                                   | **MIGRATE** — has inner per-record SAVEPOINTs (`vec_batch_record`); the inner SAVEPOINTs are preserved; outer `BEGIN IMMEDIATE` removed                                                                                                                                                                                                                                                                                                                                                                                                            |
| `sql_bridge.rs:322` | `SqliteWriter::execute_batch` (file-backed `SqlBridge::writer()` path)                          | **MIGRATE** — already Entry 10; `SqlBridge::writer()` and `execute_batch()` send through the channel; the `SqliteWriter` that wraps the standalone connection is retired                                                                                                                                                                                                                                                                                                                                                                           |
| `sql_bridge.rs:699` | `PoolBackedWriter::execute_batch` (in-memory path)                                              | **EXEMPT** — `PoolBackedWriter` is produced only when `!is_file_backed` (`sql_bridge.rs:798`); the production daemon always uses `is_file_backed = true` (`backend.rs:29,38`); in-memory path is for tests and is not reachable from the daemon accept loop                                                                                                                                                                                                                                                                                        |
| `sql_bridge.rs:834` | `SqlBridge::begin_tx()`                                                                         | **EXEMPT (deferred)** — entry 9; deferred to the follow-up ADR for issue #195; acknowledged gap in Consequences                                                                                                                                                                                                                                                                                                                                                                                                                                    |

A bare `BEGIN IMMEDIATE` inside the WriterTask's outer transaction violates SQLite's nested-BEGIN
rule and will return `SQLITE_ERROR: cannot start a transaction within a transaction`. Each MIGRATE
site must be rewritten so the closure body contains only DML statements and named SAVEPOINT
operations managed by the WriterTask wrapper — never a bare `BEGIN IMMEDIATE`. The EXEMPT sites
are unreachable from the concurrent write path this ADR targets and do not introduce the
nested-BEGIN hazard.

Migration steps for each Entry in the write-path inventory (§Write-path inventory):

- Entries 1, 2, 6, 7 (`entity`, `note`, `sparse`, `vectors` stores): replace the `with_writer`
  helper so it sends a typed `WriteRequest` and awaits the oneshot reply (see WriteRequest shape
  below), instead of calling `pool.try_writer()` inside `spawn_blocking`. Remove the `BEGIN
  IMMEDIATE` / `COMMIT` / `ROLLBACK` calls from each closure body.
- Entries 3, 4, 5 (`graph`, `text`, `event` stores): replace the `with_writer` helper so it
  sends a typed `WriteRequest` and awaits the reply, instead of calling `open_standalone_writer()`.
  The `is_file_backed` branch that currently opens a standalone connection is removed. Remove the
  `BEGIN IMMEDIATE` / `COMMIT` / `ROLLBACK` calls from each closure body. Preserve inner
  per-record SAVEPOINTs in `text.rs:395` and the per-event loop in `event.rs:667`.
- Entries 8, 10 (`SqlBridge::writer()`, `SqliteWriter::execute_batch`): `SqlBridge::writer()` and
  `execute_batch()` are updated to send through the channel; the `SqliteWriter` standalone path
  is retired.
- Entries 11, 12, 13 (`curation::merge_entity`, `curation::merge_note`, `operations::link`):
  replace direct `pool.writer()` calls with channel sends.
- Entry 14 (startup/bootstrap writes in `backend.rs`): these run during sequential daemon startup
  before the accept loop begins. They continue to use `pool.try_writer()` directly; no channel
  routing required because no concurrent writes can occur during startup.

After migration, `BEGIN IMMEDIATE` for daemon write traffic is owned solely by `WriterTask`,
**excluding** `SqlBridge::begin_tx()` (deferred to the #195 follow-up ADR) and the
startup/bootstrap writes in `backend.rs` (sequential, pre-accept-loop). The narrow guarantee is:
no in-flight concurrent write request reaching the accept loop issues `BEGIN IMMEDIATE` outside
`WriterTask`.

**Reads are unaffected**: `with_reader` paths and standalone reader connections
(`open_standalone_reader` in `sql_bridge.rs:101`) are not routed through the writer task.
Reads remain fully concurrent.

### Component B: Batched commits

The writer task collects pending `WriteRequest` entries from the channel using a drain loop with
a configurable collect window. When either the window expires or the batch reaches a threshold
number of operations, the task issues a single `BEGIN IMMEDIATE` / `COMMIT` wrapping all
collected statements. This amortizes the WAL write-lock acquisition cost across N operations
instead of paying it N times.

**Batch parameters** (configurable in `PoolConfig`):

| Parameter         | Default | Meaning                                    |
| ----------------- | ------- | ------------------------------------------ |
| `batch_window_ms` | 5 ms    | Maximum time to collect before committing  |
| `batch_max_ops`   | 64      | Maximum number of write requests per batch |

**Cross-request batching isolation**: the current `request` tool contract — established in
`crates/khive-mcp/src/server.rs:566-568` — is that Single and Parallel ops run concurrently
and per-op failure does not abort siblings; each op produces an independent `ok`/`error` result.
Cross-request batching inside the writer task must not break this contract.

To preserve per-request isolation, the writer task uses a three-level SAVEPOINT hierarchy within
the batch transaction. SQLite allows nested named SAVEPOINTs, so all three levels are legal
within one `BEGIN IMMEDIATE`:

```
BEGIN IMMEDIATE                                        -- WriterTask outer transaction
  SAVEPOINT req_0                                      -- Level 2: per-request isolation
    <DML for request A, e.g. single upsert_entity>
  RELEASE SAVEPOINT req_0                              -- success: promote to outer txn
  SAVEPOINT req_1                                      -- Level 2: per-request isolation
    SAVEPOINT inner_rec_0                              -- Level 3: per-record (batch methods)
      <DML for record 0>
    RELEASE SAVEPOINT inner_rec_0                      -- record 0 ok
    SAVEPOINT inner_rec_1
      <DML for record 1>
    ROLLBACK TO SAVEPOINT inner_rec_1                  -- record 1 failed (partial success)
    RELEASE SAVEPOINT inner_rec_1
    <...>
    -- reply carries BatchWriteSummary{attempted:2, affected:1, failed:1}
  RELEASE SAVEPOINT req_1                              -- request-level success (partial ok)
  SAVEPOINT req_2                                      -- Level 2: per-request isolation
    <DML for request C>
  ROLLBACK TO SAVEPOINT req_2                          -- hard StorageError; roll back request C
  RELEASE SAVEPOINT req_2                              -- releases the savepoint marker
COMMIT                                                 -- all surviving requests committed
```

Level 2 (per-request SAVEPOINT): `WriterTask` wraps each drained `WriteRequest` in a named
SAVEPOINT (e.g. `req_<n>`). On the closure returning `Ok(R)`, the WriterTask RELEASEs the
SAVEPOINT, promoting that request's writes into the outer transaction. On a hard `Err(StorageError)`,
the WriterTask issues `ROLLBACK TO SAVEPOINT req_<n>` then `RELEASE SAVEPOINT req_<n>`, leaving
sibling requests untouched. The typed oneshot reply carries the full `Result<R, StorageError>`.

Level 3 (per-record SAVEPOINT, inside batch closures): batch methods such as `upsert_documents`
(`text.rs:395`) and `insert_batch` (`vectors.rs:355`) already manage inner named SAVEPOINTs
(`fts_upsert_doc`, `vec_batch_record`) for per-record partial success. These inner SAVEPOINTs are
preserved unchanged inside the closure body. The outer `BEGIN IMMEDIATE` those methods currently
issue is removed; the Level 2 per-request SAVEPOINT from `WriterTask` replaces it as the
isolation boundary. The `BatchWriteSummary` (with its `affected`/`failed`/`first_error` fields)
is returned through the typed reply channel unmodified.

A failure in request B's per-request SAVEPOINT rolls back only request B. The typed oneshot reply
for request B carries `Err(StorageError)`. The typed reply for request A carries its full
`Ok(BatchWriteSummary{...})`. This matches the existing server.rs per-op independence guarantee.
Callers observe no change in error semantics or return type compared to current standalone-per-op
behavior. No `BatchWriteSummary` fields are dropped.

**Atomic single-operation mode**: if a caller requires that its operation commits atomically and
is not co-batched with any other operation (for example, a migration step), it sets a
`WriteRequest::require_solo: bool` flag. The writer task flushes any pending batch before
processing a solo request and commits it in isolation.

### Component C: WAL checkpoint discipline

**What is in flight (Slice 1 / PR #221)**: `CheckpointConfig` (interval, warn threshold,
high-water threshold, all from-env) and `run_checkpoint_task` (a periodic loop spawned in the
daemon) are implemented in PR #221 (branch `perf/khive-db-wal-checkpoint`), which is currently
in adversarial review and not yet merged. This work is self-contained and ADR-independent. It
is a prerequisite for Component C's coordination signal: PR #221 must land before ADR-067's
`WriterTask` can wire to the checkpoint discipline it provides. Do not claim PR #221 is
merged or complete until the merge commit is confirmed.

**What this ADR adds (Slice 2)**: the writer task coordinates with the checkpoint task by
exposing a write-activity signal (for example, a shared `AtomicBool` or a tokio watch channel)
so the checkpoint task can observe its idle window accurately. The checkpoint task parameters
and table of env overrides are already defined by PR #221 and are not redefined here.

The checkpoint task uses a dedicated checkpoint connection (not the writer connection) so it
does not contend with the writer task. The coordination signal from Component A is the only
new artifact from this ADR that Component C needs.

### Component D: Transaction watchdog

If a batch does not complete within a configurable timeout (`TXN_WATCHDOG_SECS`, default 30 s),
the watchdog interrupts the in-flight SQL statement. The blocking closure — which still owns the
connection — detects the interrupt signal, performs `ROLLBACK` locally, and returns an error. The
writer task then sends `StorageError::WatchdogTimeout` to every oneshot sender whose requests
were in the timed-out batch and opens a fresh connection before accepting the next batch. The
watchdog does not roll back on a foreign connection; the closure always rolls back on the
connection it owns.

**Why the watchdog deadline covers lock acquisition and execution together**: the `spawn_blocking`
closure is dispatched before `BEGIN IMMEDIATE` is issued, so the watchdog timer necessarily
covers both the `busy_timeout` lock-acquisition wait and the post-BEGIN execution time. The
`busy_timeout` setting (`pool.rs:29`, 30 s) limits how long `BEGIN IMMEDIATE` will spin waiting
for the WAL write lock; `TXN_WATCHDOG_SECS` limits the total wall time the blocking closure may
run. After migration, `WriterTask` holds the single writer connection, so `busy_timeout` applies
only to that one connection (and the checkpoint connection). Both settings remain configured; they
are complementary, not redundant.

**Execution model**: the writer task is a Tokio async task. The `rusqlite::Connection` is moved
into each `spawn_blocking` closure; the async task cannot touch the same connection while the
closure runs. Cancelling the `JoinHandle` does not cancel the running closure. To interrupt a
slow batch, the writer task uses `rusqlite`'s interrupt API.

**Before** moving the connection into `spawn_blocking`, the writer task obtains an interrupt
handle. On timeout, it calls `interrupt()` and then awaits the handle to observe the
closure's rolled-back result:

```rust
// Obtain interrupt handle BEFORE moving conn into the closure.
let interrupt_handle = conn.get_interrupt_handle(); // InterruptHandle: Send + Sync

// Spawn the blocking batch. conn is moved in; async task no longer holds it.
let mut handle = tokio::task::spawn_blocking(move || {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    // ... execute per-request SAVEPOINTs and DML ...
    conn.execute_batch("COMMIT")?;
    Ok(conn) // return conn on success so it can be reused
});

// Borrow the handle so it is not consumed by timeout; we need it for the interrupt path.
match tokio::time::timeout(Duration::from_secs(TXN_WATCHDOG_SECS), &mut handle).await {
    Ok(join_result) => {
        // Batch completed within deadline. join_result is Result<Result<conn, rusqlite::Error>, JoinError>.
        match join_result {
            Ok(Ok(conn)) => { /* reuse conn for next batch */ }
            Ok(Err(e))   => { /* SQL error — mark conn poisoned, open fresh */ }
            Err(_panic)  => { /* spawn_blocking panicked — open fresh conn */ }
        }
    }
    Err(_elapsed) => {
        // Deadline expired. Signal the in-flight SQL statement to abort.
        // interrupt() is safe to call from any thread; it sets a flag SQLite checks
        // between opcodes, causing the next statement to return SQLITE_INTERRUPT.
        interrupt_handle.interrupt();

        // The closure still owns conn. When it observes SQLITE_INTERRUPT
        // (rusqlite::Error::SqliteFailure with code ErrorCode::OperationInterrupted),
        // it executes conn.execute_batch("ROLLBACK") locally and returns Err(...).
        // Await the now-completing handle (bounded by cleanup time, not a new timeout).
        let _ = handle.await; // result carries the post-interrupt rollback outcome

        // Send WatchdogTimeout to all senders in this batch, then open a fresh connection.
        for sender in batch_senders {
            let _ = sender.send(Err(StorageError::WatchdogTimeout));
        }
        conn = open_fresh_writer_connection();
    }
}
```

Key properties:

- `interrupt_handle.interrupt()` signals the in-flight statement; it does **not** roll back on a
  foreign connection.
- The blocking closure detects `SqliteFailure { code: ErrorCode::OperationInterrupted }` and
  issues `conn.execute_batch("ROLLBACK")` locally before returning.
- `InterruptHandle` is `Send + Sync` and safe to hold in the async task concurrent with the
  blocking closure. Obtained via `Connection::get_interrupt_handle()` (rusqlite 0.33 stable API).
- The `&mut handle` borrow in `timeout(..., &mut handle)` does not consume the `JoinHandle`, so
  the elapsed branch can still `.await` it to observe the post-interrupt result.

**ROLLBACK failure handling**: if the `ROLLBACK` inside the closure itself fails (the connection
is unrecoverable after interrupt), the closure returns an error. The writer task observes this via
the awaited `JoinHandle`, marks the connection poisoned, and opens a fresh connection. It does not
retry the original batch.

### Component E: Transport layer (unchanged)

The ADR-049 Unix socket framing (4-byte length-prefix, JSON payload, 8 MiB cap) and the
`forward_or_spawn` client logic (`crates/khive-mcp/src/daemon.rs:489`) are unchanged. The
single-writer guarantee lives entirely inside the daemon process. All callers of the MCP
`request` tool observe no protocol change.

### Issue #195 — decision: dependent follow-up ADR

Cross-op atomicity for `--ops-file` bulk apply is scoped as a separate follow-up ADR (number
assigned when drafted) rather than a section of this ADR, for the following reasons:

1. **Different seam**: #195 requires threading a `SqlTransaction` context from the
   `begin_tx()` seam in `sql_bridge.rs:804` through the verb handler dispatch path. This touches
   the `VerbRegistry`, every store's `with_writer` helper, and the handler call signatures. It
   is a non-trivial API surface change independent of the write-queue mechanics.

2. **Ordering dependency**: the write-queue model (Component A) must be stable before the
   all-or-nothing path can be designed, because the transaction context for a bulk apply would
   be managed by the writer task. Designing #195 before Component A is accepted would require
   redesigning the transaction handoff.

3. **Blast radius containment**: bundling #195 into this ADR would double the implementation
   surface in a single change. Isolating it allows each piece to be reviewed, tested, and
   rolled back independently.

The follow-up ADR for #195 must specify: the `begin_tx()` call site in the bulk-apply path;
how the `SqlTransaction` is threaded through `dispatch_request_local`; the commit and rollback
points; and the error response shape for a partial-failure rollback. It must depend on
Component A's `WriterTask` being the stable owner of write connections.

---

## Alternatives considered

### Alternative 1: Tune WAL and busy timeout (Slice 1 only, no structural change)

Expose `WAL_AUTOCHECKPOINT_PAGES`, `JOURNAL_SIZE_LIMIT_BYTES`, and `busy_timeout` as
configurable parameters and add the periodic passive checkpoint task. This reduces wedge
probability under moderate load by keeping the WAL shorter and checkpointing more aggressively.

Rejected as the sole mitigation because it does not eliminate the root cause: standalone
connections (graph, text, event, SqlBridge) still compete via `busy_timeout` waits, and pool
Mutex holders still contend under load. Slice 1 is implemented as a prerequisite de-risk
measure (PR #221); it is not a substitute for the structural redesign.

### Alternative 2: Multiple writer connections (connection-per-request pool)

Increase the number of writer connections and issue `BEGIN IMMEDIATE` on each independently.
SQLite serializes `BEGIN IMMEDIATE` at the database level via the WAL write lock, so multiple
connections holding concurrent `BEGIN IMMEDIATE` attempts simply move the serialization point
from the Rust Mutex to the SQLite internal lock, with identical throughput but worse error
attribution (SQLite `SQLITE_BUSY` instead of a typed Rust error).

Rejected. This approach also loses the ability to batch commits across requests, which is the
primary throughput lever available within the SQLite single-writer constraint.

### Alternative 3: Shared Postgres MVCC (escape hatch, deferred ADR)

Postgres MVCC eliminates the WAL-starvation wedge class entirely: concurrent writers obtain row
locks, not a global write lock, and readers never block writers. The storage trait surface
(ADR-005) is backend-neutral: no `rusqlite` types appear in any `Arc<dyn Trait>` interface
(`SqlAccess`, `EntityStore`, `NoteStore`, `GraphStore`). A Postgres backend would be a new crate
(`crates/khive-pg`) with Postgres DDL equivalents, `PgBridge` implementing `SqlAccess` with
Postgres transaction semantics (`SET TRANSACTION ISOLATION LEVEL`), `tsvector`-backed
`TextSearch`, and `pgvector`-backed `VectorStore`.

This is the measure-first escape hatch: if the write-queue redesign (this ADR) plus the WAL
checkpoint discipline (Slice 1) do not achieve the throughput target under realistic tenant
load, the Postgres backend is the next escalation point. It is deferred because it is a
multi-week effort (estimated 3 to 5 weeks for a feature-complete backend) and the per-process
tenant topology already eliminates the cross-tenant scaling problem that Postgres would
primarily solve.

### Alternative 4: Serialize at the HTTP gateway layer

Add a request queue at the gateway that ensures at most one write-intent request reaches the
daemon at a time. Rejected because this gates on the HTTP gateway (ADR-future, not shipped) and
misplaces the serialization concern: the write contention is a storage-layer property, not a
protocol-layer property. It would also prevent concurrent reads from progressing, which is
unnecessary.

---

## Consequences

### Positive

- Eliminates both contention paths for daemon write traffic: pool Mutex contention (entries 1, 2,
  6, 7, 11, 12, 13) and standalone-connection `BEGIN IMMEDIATE` contention (entries 3, 4, 5, 8,
  10). After migration, `BEGIN IMMEDIATE` for concurrent write requests routed through the accept
  loop is called only inside `WriterTask`. The two exceptions — `SqlBridge::begin_tx()` (deferred
  to the #195 follow-up ADR) and startup/bootstrap writes in `backend.rs` (sequential,
  pre-accept-loop) — are not part of the concurrent write traffic this ADR targets.
- Batched commits amortize WAL write-lock acquisition, improving write throughput under load.
- Per-request SAVEPOINTs preserve the existing server.rs per-op independence contract: a failure
  in one request does not roll back unrelated concurrent requests.
- The transaction watchdog provides clean application-level attribution for slow writes instead
  of opaque SQLite `SQLITE_BUSY` errors.
- Backpressure is explicit and measurable (bounded channel depth) instead of implicit (Mutex
  contention visible only as timeouts).
- Reads remain fully concurrent and are unaffected by writer task load.

### Negative

- All write-path entry points in the store layer must be migrated. This includes not only the
  five `with_writer` helpers cited in the original design (`entity`, `note`, `graph`, `text`,
  `vectors`) but also `event` and `sparse` stores, `SqlBridge::writer()`, `SqlBridge::execute_batch()`,
  `curation::merge_entity`, `curation::merge_note`, and `operations::link`. The migration is
  bounded and mechanical but is a large diff across many files.
- `SqlBridge::begin_tx()` (`sql_bridge.rs:804`) is not migrated by this ADR. It continues to
  open a standalone connection. This means the writer task does not yet own the transaction
  context for the `begin_tx` path, and write contention from that path is not eliminated until
  the follow-up ADR for #195 lands.
- The `acquire_write_lock` pattern used by DDL helpers during daemon startup continues to use
  `pool.try_writer()` directly. This is safe because startup is sequential (no concurrent agents),
  but the asymmetry must be documented for implementers.
- Backpressure is visible to callers as a blocking `channel.send().await` rather than an
  immediate error. Callers must not hold other resources while suspended.

### Neutral

- `kkernel exec --ops-file` currently bypasses the daemon fast-path (`exec.rs:408`). This
  behavior is unchanged by this ADR. The write-queue is inside the daemon; bulk apply uses an
  in-process runtime and is therefore not affected by this ADR's changes until the #195
  follow-up threads transaction context through `dispatch_request_local`.

---

## Migration and sequencing

The recommended landing sequence is:

**Slice 1 (in-flight, PR #221)**: `CheckpointConfig` with interval, warn threshold, and
high-water threshold, all overridable by environment variables. `run_checkpoint_task` spawned
in the daemon. This is a standalone de-risk measure that reduces wedge probability immediately
and does not require this ADR. PR #221 (branch `perf/khive-db-wal-checkpoint`) is in adversarial
review and not yet merged. Slice 1 must land before Slice 2's `WriterTask` can wire to the
checkpoint coordination signal PR #221 provides.

**Slice 2 (this ADR): Write-owner task and write queue**. After ADR-067 is accepted:

1. Implement `WriterTask` in `crates/khive-db/src/writer_task.rs`.
2. Add `WriteRequest` message type, `WriteChannel` wrapper, and SAVEPOINT-per-request logic.
3. Migrate `with_writer` in `entity.rs`, `note.rs`, `graph.rs`, `text.rs`, `event.rs`,
   `sparse.rs`, and `vectors.rs` — all seven stores.
4. Update `SqlBridge::writer()` and `SqlBridge::execute_batch()` in `sql_bridge.rs`.
5. Update `curation::merge_entity`, `curation::merge_note`, and `operations::link` to send
   through the channel.
6. Leave `SqlBridge::begin_tx()` and the DDL helpers (`backend.rs`) as-is (see Consequences).
7. Start `WriterTask` in `run_daemon` and wire the write-activity signal to the checkpoint task.
8. Gate behind `KHIVE_WRITE_QUEUE=1` environment variable for initial rollout; remove the gate
   after integration tests confirm correctness under concurrent load.
9. Add the transaction watchdog inside `WriterTask`.
   Estimated effort: 2 to 3 weeks including integration test coverage.

**Slice 3 (follow-up ADR): #195 cross-op atomicity**. After Slice 2 is stable: draft the
follow-up ADR for threading `SqlTransaction` context through `dispatch_request_local` and
migrating `begin_tx()` to route through the writer task. Depends on Slice 2's `WriterTask`
being the stable owner of write connections.
Estimated effort: 1 to 2 weeks once Slice 2 is stable.

**Slice 4 (deferred, separate ADR): Postgres backend**. Decoupled from Slices 1 through 3 and
can proceed in parallel after the Postgres ADR is accepted. Unblocks the multi-process
scale path if Slice 2 does not meet the throughput target at scale.

---

## Out of scope

The following are explicitly excluded from this ADR:

- **Multi-process isolation**: the per-process topology (ADR-068) is the solution; this ADR
  addresses intra-process write coordination only.
- **HTTP gateway layer**: not shipped; the write-queue is inside the daemon.
- **Postgres backend**: covered by a future ADR once measure-first evidence is collected.
- **Cross-op atomicity for `--ops-file`**: follow-up ADR after Slice 2 is stable.
- **ANN index write path**: ANN index persistence (`retrieval_snapshots`) is a separate write
  concern and is not routed through the general write-queue unless the follow-up ADR explicitly
  includes it.
- **ADR-049 transport framing changes**: the Unix socket protocol is unchanged.
- **`SqlBridge::begin_tx()` migration**: deferred to the follow-up ADR for #195.
- **DDL helper migration**: DDL runs during sequential startup before concurrent agents connect
  and continues to use `pool.try_writer()` directly.

---

## References

- ADR-005: Storage capability traits — 8 backend-neutral traits; `SqlAccess::begin_tx` seam
- ADR-017: Pack standard — `with_writer` pattern is the current pack write convention
- ADR-028: Pack-scoped backends — `ConnectionPool` ownership model
- ADR-049: khived daemon — socket framing and `forward_or_spawn` protocol (unchanged)
- `crates/khive-db/src/pool.rs` — single writer Mutex (`pool.rs:60`), WAL constants (`pool.rs:15-16`), checkout timeout (`pool.rs:263`), busy timeout (`pool.rs:29`)
- `crates/khive-db/src/backend.rs` — `StorageBackend::sqlite` sets `is_file_backed: true` (`backend.rs:29,38`); DDL helpers use `pool.try_writer()` directly
- `crates/khive-db/src/sql_bridge.rs` — `open_standalone_writer` (`sql_bridge.rs:126`), `execute_batch` with `BEGIN IMMEDIATE` (`sql_bridge.rs:320-329`), `writer()` standalone path (`sql_bridge.rs:791-801`), `begin_tx()` standalone path (`sql_bridge.rs:804-853`)
- `crates/khive-db/src/stores/entity.rs` — `with_writer` via pool Mutex (`entity.rs:68-80`)
- `crates/khive-db/src/stores/note.rs` — `with_writer` via pool Mutex (`note.rs:69-81`)
- `crates/khive-db/src/stores/graph.rs` — `with_writer` via `open_standalone_writer` in file-backed mode (`graph.rs:111-130`)
- `crates/khive-db/src/stores/text.rs` — `with_writer` via `open_standalone_writer` in file-backed mode (`text.rs:130-149`)
- `crates/khive-db/src/stores/event.rs` — `with_writer` via `open_standalone_writer` in file-backed mode (`event.rs:104-123`)
- `crates/khive-db/src/stores/sparse.rs` — `with_writer` via pool Mutex, no file-backed branch (`sparse.rs:133-145`)
- `crates/khive-db/src/stores/vectors.rs` — `with_writer` via pool Mutex, no file-backed branch (`vectors.rs:217-229`)
- `crates/khive-runtime/src/curation.rs` — `merge_entity` and `merge_note` use `pool.writer()` directly (`curation.rs:316-326`, `638-645`)
- `crates/khive-runtime/src/operations.rs` — `link` uses `pool.writer()` directly (`operations.rs:3061-3073`)
- `crates/khive-mcp/src/server.rs` — Single/Parallel per-op independence contract (`server.rs:566-568`); per-op result preservation (`server.rs:637,755`)
- `crates/khive-runtime/src/daemon.rs` — `run_daemon` accept loop (`daemon.rs:466`)
- `crates/kkernel/src/exec.rs` — `apply_ops_file` and daemon fast-path bypass (`exec.rs:214`, `408`)
- PR #221 — Slice 1: `CheckpointConfig` and `run_checkpoint_task` (in-flight, ADR-independent; must land before Slice 2)
- Issue #195: cross-op atomicity for `--ops-file` bulk apply
