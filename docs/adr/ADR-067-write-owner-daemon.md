# ADR-067: Write-Owner Daemon — Single-Writer Task and Write Queue

**Status**: Proposed
**Date**: 2026-06-23
**Issue**: #195 (cross-op atomicity for `--ops-file` bulk apply); 2026-06-22 write-wedge incident
**Amends**: ADR-049 (khived daemon — adds write-queue layer inside the daemon process)
**Depends on**: ADR-049 (socket framing, forward_or_spawn protocol — unchanged by this ADR)

---

## Context

### Deployment model (scope boundary)

The cloud storage model is SQLite-per-tenant with one daemon process per tenant. Each tenant's
database is a separate file with a separate connection pool and a separate writer Mutex. The
wedge described below is therefore contained to a single tenant: multiple concurrent agents
belonging to the same tenant share one process and one pool. This ADR solves
"one tenant, N concurrent agents, no wedge" and does not address cross-tenant scaling
(that is the per-process topology concern, addressed separately).

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

| #  | Entry point                                                                                                                                    | File                                                 | file-backed path                                                                 |
| -- | ---------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------- | -------------------------------------------------------------------------------- |
| 1  | `SqlEntityStore::with_writer`                                                                                                                  | `stores/entity.rs:68-80`                             | pool Mutex (`pool.try_writer()`)                                                 |
| 2  | `SqlNoteStore::with_writer`                                                                                                                    | `stores/note.rs:69-81`                               | pool Mutex (`pool.try_writer()`)                                                 |
| 3  | `SqlGraphStore::with_writer`                                                                                                                   | `stores/graph.rs:111-130`                            | **standalone connection** (`open_standalone_writer`)                             |
| 4  | `SqlFtsStore::with_writer`                                                                                                                     | `stores/text.rs:130-149`                             | **standalone connection** (`open_standalone_writer`)                             |
| 5  | `SqlEventStore::with_writer`                                                                                                                   | `stores/event.rs:104-123`                            | **standalone connection** (`open_standalone_writer`)                             |
| 6  | `SqliteSparseStore::with_writer`                                                                                                               | `stores/sparse.rs:133-145`                           | pool Mutex (`pool.try_writer()`) — no file-backed branch                         |
| 7  | `SqliteVecStore::with_writer`                                                                                                                  | `stores/vectors.rs:217-229`                          | pool Mutex (`pool.try_writer()`) — no file-backed branch                         |
| 8  | `SqlBridge::writer()`                                                                                                                          | `sql_bridge.rs:791-801`                              | **standalone connection** (`open_standalone_writer`)                             |
| 9  | `SqlBridge::begin_tx()`                                                                                                                        | `sql_bridge.rs:804-853`                              | **standalone connection** + `BEGIN IMMEDIATE`                                    |
| 10 | `SqlBridge::execute_batch()`                                                                                                                   | `sql_bridge.rs:312-338`                              | runs on the connection owned by `SqliteWriter`, which is a standalone connection |
| 11 | `curation::merge_entity`                                                                                                                       | `curation.rs:316-326`                                | pool Mutex (`pool.writer()`) directly                                            |
| 12 | `curation::merge_note`                                                                                                                         | `curation.rs:638-645`                                | pool Mutex (`pool.writer()`) directly                                            |
| 13 | `operations::link`                                                                                                                             | `operations.rs:3061-3073`                            | pool Mutex (`pool.writer()`) directly                                            |
| 14 | `StorageBackend DDL helpers` (`apply_schema`, `apply_pack_ddl`, `entity_store`, `graph_store`, `note_store`, `event_store`, `vec_store`, etc.) | `backend.rs:108,132,158,185,214,241,302,399,464,534` | pool Mutex (`pool.try_writer()`) directly                                        |

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

Replace all write-path contention with a single write-owner task inside the daemon. All write
operations — including those currently using standalone connections (graph, text, event, SqlBridge)
and those using the pool Mutex (entity, note, sparse, vectors, curation, operations, DDL helpers)
— route their mutations through a bounded async channel to this task. The task is the sole caller
of `BEGIN IMMEDIATE` and holds the single writer connection for the life of the daemon. Reads
remain concurrent and do not route through the write-owner task.

The transport layer (ADR-049 Unix socket framing, `forward_or_spawn`, `DaemonRequestFrame`) is
unchanged. The redesign is internal to the daemon process and is transparent to all callers.

---

## Components

### Component A: Write queue and single writer task

A dedicated Tokio task (`WriterTask`) is introduced in `crates/khive-db/src/writer_task.rs`
(or as a module within `pool.rs` if the scope warrants co-location). This task is the
exclusive owner of the writer connection. It receives write requests over a bounded
`tokio::mpsc::channel` and is the only code path that calls `BEGIN IMMEDIATE`.

**`WriteRequest` message shape**:

```
WriteRequest {
    statements: Vec<SqlStatement>,
    reply: oneshot::Sender<Result<u64, StorageError>>,
}
```

The `reply` sender delivers the result (rows affected or error) back to the originating async
task. Callers `await` the oneshot receiver and propagate the result as if they had called
`pool.writer()` directly.

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

**Complete write-path migration**: all 14 entries from the write-path inventory above are
migrated to route through the writer task channel. Specifically:

- Entries 1, 2, 6, 7 (`entity`, `note`, `sparse`, `vectors` stores): replace the `with_writer`
  helper so it sends a `WriteRequest` and awaits the oneshot reply, instead of calling
  `pool.try_writer()` inside `spawn_blocking`.
- Entries 3, 4, 5 (`graph`, `text`, `event` stores): replace the `with_writer` helper so it
  sends a `WriteRequest` and awaits the reply, instead of calling `open_standalone_writer()`.
  The `is_file_backed` branch that currently opens a standalone connection is removed; the writer
  task owns the single writer connection regardless of whether the backend is file-backed or
  in-memory.
- Entries 8, 9, 10 (`SqlBridge::writer()`, `begin_tx()`, `execute_batch()`): `SqlBridge::writer()`
  and `execute_batch()` are updated to send through the channel. `begin_tx()` is left with its
  current standalone-connection behavior until the follow-up ADR for #195 specifies the
  transaction ownership model; this is an acknowledged gap (see Consequences).
- Entries 11, 12, 13 (`curation::merge_entity`, `curation::merge_note`, `operations::link`):
  replace direct `pool.writer()` calls with channel sends.
- Entry 14 (DDL helpers in `backend.rs`): DDL is applied during daemon startup before the
  accept loop begins and before concurrent agents connect. DDL helpers continue to use
  `pool.try_writer()` directly during the startup phase; they do not need to route through the
  writer task channel because no concurrent writes can occur during startup.

After migration, `BEGIN IMMEDIATE` is called in exactly one location: inside `WriterTask`.

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

To preserve per-request isolation, the writer task uses per-request SAVEPOINTs within the
batch transaction:

```
BEGIN IMMEDIATE
  SAVEPOINT req_0; <statements for request A>; RELEASE SAVEPOINT req_0;
  SAVEPOINT req_1; <statements for request B>; ROLLBACK TO SAVEPOINT req_1; RELEASE SAVEPOINT req_1;
  ...
COMMIT
```

A failure in request B rolls back only request B's SAVEPOINT. The oneshot reply for request B
carries a `StorageError` for that op. The oneshot reply for request A carries success. This
matches the existing server.rs per-op independence guarantee. Callers observe no change in error
semantics compared to the current standalone-per-op behavior.

**Atomic single-operation mode**: if a caller requires that its operation commits atomically and
is not co-batched with any other operation (for example, a migration step), it sets a
`WriteRequest::require_solo: bool` flag. The writer task flushes any pending batch before
processing a solo request and commits it in isolation.

### Component C: WAL checkpoint discipline

**What has shipped (Slice 1 / PR #221)**: `CheckpointConfig` (interval, warn threshold,
high-water threshold, all from-env) and `run_checkpoint_task` (a periodic loop spawned in the
daemon) were implemented as a self-contained, ADR-independent change. That work is complete and
merged. It is not conditional on this ADR.

**What this ADR adds (Slice 2)**: the writer task coordinates with the checkpoint task by
exposing a write-activity signal (for example, a shared `AtomicBool` or a tokio watch channel)
so the checkpoint task can observe its idle window accurately. The checkpoint task parameters
and table of env overrides are already defined by PR #221 and are not redefined here.

The checkpoint task uses a dedicated checkpoint connection (not the writer connection) so it
does not contend with the writer task. The coordination signal from Component A is the only
new artifact from this ADR that Component C needs.

### Component D: Transaction watchdog

The writer task tracks the start time of any in-flight `BEGIN IMMEDIATE`. If the transaction is
not committed or rolled back within a configurable timeout (`TXN_WATCHDOG_SECS`, default 30 s),
the watchdog issues `ROLLBACK` and returns `StorageError::WatchdogTimeout` to all senders
whose requests were in the timed-out batch.

**Execution model**: the writer task is a Tokio async task that owns a `JoinHandle` for each
`spawn_blocking` call it issues to run the blocking SQLite statement. The watchdog wraps the
`JoinHandle` in a `tokio::time::timeout`:

```rust
let result = tokio::time::timeout(
    Duration::from_secs(TXN_WATCHDOG_SECS),
    spawn_blocking_handle,
).await;
```

If the timeout fires before the blocking call returns, the writer task issues
`conn.execute_batch("ROLLBACK")` on a separate blocking call and sends
`StorageError::WatchdogTimeout` to every oneshot sender in the timed-out batch. The connection
is then considered poisoned and the writer task opens a fresh connection before accepting the
next batch.

**Relationship to `busy_timeout`**: `busy_timeout` (30 s, `pool.rs:29`) governs the per-connection
SQLite lock-acquisition wait on `BEGIN IMMEDIATE`. The watchdog governs the total transaction
duration after `BEGIN IMMEDIATE` has been acquired. Both remain configured; they address different
phases. After migration, `busy_timeout` applies only to the writer task's single connection
(and to the checkpoint connection), not to a pool of competing writers.

**ROLLBACK failure handling**: if `ROLLBACK` itself times out or returns an error (for example,
because the connection is in an unrecoverable state), the writer task logs the error, closes the
connection, and opens a fresh connection. It does not retry the original batch.

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

- Eliminates both contention paths: pool Mutex contention (entries 1, 2, 6, 7, 11, 12, 13, 14)
  and standalone-connection `BEGIN IMMEDIATE` contention (entries 3, 4, 5, 8, 9, 10). After
  migration, `BEGIN IMMEDIATE` is called in exactly one place.
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

**Slice 1 (complete, PR #221)**: `CheckpointConfig` with interval, warn threshold, and
high-water threshold, all overridable by environment variables. `run_checkpoint_task` spawned
in the daemon. This is a standalone de-risk measure that reduces wedge probability immediately
and does not require this ADR. Already merged.

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
can proceed in parallel after the Postgres ADR is accepted. Unblocks the multi-tenant escape
hatch if Slice 2 does not meet the throughput target at scale.

---

## Out of scope

The following are explicitly excluded from this ADR:

- **Multi-tenant isolation**: the per-process tenant topology is the solution; this ADR is
  intra-tenant only.
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
- PR #221 — Slice 1: `CheckpointConfig` and `run_checkpoint_task` (merged, ADR-independent)
- Issue #195: cross-op atomicity for `--ops-file` bulk apply
