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

Under N concurrent agents sharing one daemon process, all writes serialize on a single
`parking_lot::Mutex<Connection>` held for the full SQLite transaction duration. The chain of
evidence is as follows.

**Single writer Mutex** (`crates/khive-db/src/pool.rs:60`):

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

**BEGIN IMMEDIATE held for the full transaction** (`pool.rs:136`):

```rust
pub fn transaction<F, R>(&self, f: F) -> Result<R, SqliteError> {
    self.guard.execute_batch("BEGIN IMMEDIATE")?;
    match f(&self.guard) {
        Ok(result) => { self.guard.execute_batch("COMMIT")?; Ok(result) }
        Err(err) => { self.guard.execute_batch("ROLLBACK"); Err(err) }
    }
}
```

Every store (`entity.rs:68`, `note.rs:69`, `graph.rs:111`) uses the same `with_writer` pattern:

```rust
async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError> {
    let pool = Arc::clone(&self.pool);
    tokio::task::spawn_blocking(move || {
        let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
        f(guard.conn()).map_err(|e| map_err(e, op))
    }).await ...
}
```

Each `spawn_blocking` call submits a task to the Tokio blocking thread pool. Multiple such tasks
compete for the same Mutex. The first winner calls `BEGIN IMMEDIATE`, which acquires the WAL
write lock, and holds the Rust Mutex for the entire duration of the SQLite transaction, including
any I/O within the handler closure.

**Contention cascade under load**: while Agent A holds the Mutex, Agents B through N each call
`pool.try_writer()` and block in `try_lock_for(5s)`. If Agent A's operation spans an embedding
call, a large FTS rebuild, or a complex merge, B through N time out with the 5-second error
before acquiring the lock.

**WAL growth amplifier** (`pool.rs:15-16`):

```rust
const WAL_AUTOCHECKPOINT_PAGES: &str = "4000";   // ~16 MB threshold
const JOURNAL_SIZE_LIMIT_BYTES: &str = "67108864"; // 64 MB journal limit
```

WAL autocheckpoint requires no active readers. Under concurrent ANN recall, standalone read
connections (`sql_bridge.rs:101`) hold WAL read snapshots. Checkpoint is deferred. WAL grows
past 4000 pages. SQLite begins returning `SQLITE_BUSY` to new `BEGIN IMMEDIATE` attempts
(the per-connection `busy_timeout` is 30 seconds — `pool.rs:29`), feeding additional latency
back into the write queue. The system appears wedged: writes queue behind busy-timeout cycles
while long-running reads pin the WAL.

**Degraded mode amplifier** (`pool.rs:211`): when `max_readers == 0` (in-memory or WAL
unavailable), `pool.reader()` acquires the writer Mutex, creating a deadlock risk if any caller
holds a `WriterGuard` on the same pool.

**Daemon accept loop** (`crates/khive-runtime/src/daemon.rs:466`): the daemon accepts N
connections concurrently via `tokio::spawn` per connection. Each spawned task calls `dispatch()`
on the shared `KhiveRuntime`, which owns one `Arc<StorageBackend>` and one `ConnectionPool`.
All N tasks funnel through the single writer Mutex without any application-level queuing.

**Root cause (one sentence)**: under N concurrent agents, all writes serialize on a single
`parking_lot::Mutex` (`pool.rs:60`) held for the full SQLite transaction duration; long-running
reads pin WAL readers, starving autocheckpoint, causing WAL growth that feeds `SQLITE_BUSY`
back into the write queue and can wedge the daemon.

### Issue #195 — cross-op atomicity for `--ops-file` bulk apply

`kkernel exec --ops-file` dispatches ops in chunks of 100 via `dispatch_request_local`
(`crates/kkernel/src/exec.rs:214`). The daemon fast-path is intentionally bypassed for bulk
apply (`exec.rs:408`). Each op within a chunk dispatches through its own verb handler, and each
handler acquires `pool.writer()` and issues a separate `BEGIN IMMEDIATE` / `COMMIT`. There is
no rollback across ops within a chunk, and no rollback across chunks. A failure in op 47
of a 100-op chunk leaves ops 1 through 46 committed.

The `SqlAccess` trait (`ADR-005`) exposes a `begin_tx(options: SqlTxOptions)` method
(`sql_bridge.rs:804`) that opens a standalone connection, issues `BEGIN IMMEDIATE`, and returns
a `Box<dyn SqlTransaction>` supporting `commit()` and `rollback()`. This is the correct seam
for cross-op atomicity. Threading a transaction context through the verb handler dispatch chain
requires a non-trivial API change and is scoped below.

---

## Decision

Replace the implicit per-operation `pool.writer()` Mutex contention model with a single
write-owner task inside the daemon. All write operations route their mutations through a bounded
async channel to this task. The task is the sole holder of the writer connection and is
responsible for batching, commit, rollback, and watchdog enforcement. Reads remain concurrent
and do not route through the write-owner task.

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

**Channel capacity**: The channel is bounded. Callers block on `channel.send()` when the queue
is full, providing natural backpressure on write throughput. The default capacity is
configurable via `PoolConfig` (recommended default: 256 pending operations). A full channel
returns a `StorageError` with an explicit message; it does not silently discard work.

**Store changes**: The `with_writer` helper in `entity.rs`, `note.rs`, `graph.rs`, `text.rs`,
and `vectors.rs` is updated to send a `WriteRequest` to the channel and await the reply,
replacing `pool.try_writer()` and `spawn_blocking`. The `SqlBridge` implementations in
`sql_bridge.rs` that call `pool.writer().execute_batch("BEGIN IMMEDIATE")` are similarly
updated.

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

**Cross-request batching isolation**: when the writer task collects operations from multiple
agents into a single transaction, a failure in one operation causes `ROLLBACK` for the entire
batch. Each originating sender whose operation was in the failed batch receives a
`StorageError::TransactionRolledBack` response with a retry signal. Callers at the verb handler
level may retry the operation; the handler is responsible for treating a retry error as a
retriable failure rather than a permanent error. The ADR mandates that retry errors are surfaced
distinctly from permanent errors in the `StorageError` enum.

**Atomic single-operation mode**: if a caller requires that its operation commits atomically and
is not co-batched with any other operation (for example, a migration step), it sets a
`WriteRequest::require_solo: bool` flag. The writer task flushes any pending batch before
processing a solo request and commits it in isolation.

### Component C: WAL checkpoint discipline

A periodic checkpoint task runs concurrently with the writer task. It issues
`PRAGMA wal_checkpoint(PASSIVE)` on a timer when no transaction is active, and escalates to
`PRAGMA wal_checkpoint(TRUNCATE)` when the WAL page count exceeds a threshold. The checkpoint
task is started in `run_daemon` after the listener is bound and before the accept loop begins.

**Checkpoint parameters** (configurable via `PoolConfig`, overridable by environment variables):

| Constant                     | Default | Env override                       | Meaning                              |
| ---------------------------- | ------- | ---------------------------------- | ------------------------------------ |
| `CHECKPOINT_INTERVAL_MS`     | 500 ms  | `KHIVE_CHECKPOINT_INTERVAL_MS`     | Passive checkpoint cadence           |
| `WAL_WARN_PAGES`             | 2000    | `KHIVE_WAL_WARN_PAGES`             | Log warning threshold                |
| `WAL_FORCE_CHECKPOINT_PAGES` | 6000    | `KHIVE_WAL_FORCE_CHECKPOINT_PAGES` | Force truncate threshold             |
| `CHECKPOINT_IDLE_WINDOW_MS`  | 50 ms   | `KHIVE_CHECKPOINT_IDLE_MS`         | Quiet-period guard before checkpoint |

The checkpoint task holds a shared reference to the pool and uses a dedicated checkpoint
connection (not the writer connection) so it does not contend with the writer task.

This component is also being partially implemented as Slice 1 (WAL config exposure), which
extracts the hardcoded constants in `pool.rs:15-16` into `PoolConfig` and adds environment
variable overrides. Slice 1 does not add the periodic task and does not require this ADR to
be accepted. Slice 2 (this ADR) adds the periodic checkpoint task as a component of the
write-queue redesign.

### Component D: Transaction watchdog

The writer task tracks the start time of any in-flight `BEGIN IMMEDIATE`. If the transaction is
not committed or rolled back within a configurable timeout (`TXN_WATCHDOG_SECS`, default 30 s),
the watchdog issues `ROLLBACK` and returns a `StorageError::WatchdogTimeout` to all senders
whose requests were in the timed-out batch. This replaces the implicit `busy_timeout = 30s` at
the SQLite connection level, which today fires when `BEGIN IMMEDIATE` fails to acquire the WAL
write lock. The watchdog fires at the application level when the transaction itself takes too
long, giving cleaner error attribution.

### Component E: Transport layer (unchanged)

The ADR-049 Unix socket framing (4-byte length-prefix, JSON payload, 8 MiB cap) and the
`forward_or_spawn` client logic (`crates/khive-mcp/src/daemon.rs:489`) are unchanged. The
single-writer guarantee lives entirely inside the daemon process. All callers of the MCP
`request` tool observe no protocol change.

### Issue #195 — decision: dependent follow-up ADR

Cross-op atomicity for `--ops-file` bulk apply is scoped as a follow-up ADR (ADR-068 candidate)
rather than a section of this ADR, for the following reasons:

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
points; and the error response shape for a partial-failure rollback. It may depend on
Component A's `WriteRequest` infrastructure if the writer task is the natural owner of the
transaction context.

---

## Alternatives considered

### Alternative 1: Tune WAL and busy timeout (Slice 1 only, no structural change)

Expose `WAL_AUTOCHECKPOINT_PAGES`, `JOURNAL_SIZE_LIMIT_BYTES`, and `busy_timeout` as
configurable parameters and add the periodic passive checkpoint task. This reduces wedge
probability under moderate load by keeping the WAL shorter and checkpointing more aggressively.

Rejected as the sole mitigation because it does not eliminate the root cause: the Mutex is still
held for the full transaction duration, and under sustained concurrent load the 5-second checkout
timeout will still fire. Slice 1 is implemented as a prerequisite de-risk measure; it is not a
substitute for the structural redesign.

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

- Eliminates the Mutex contention wedge: the write path no longer times out under N concurrent
  agents because there is no per-request Mutex acquisition with a fixed timeout.
- Batched commits amortize WAL write-lock acquisition, improving write throughput under load.
- The transaction watchdog provides clean application-level attribution for slow writes instead
  of opaque SQLite `SQLITE_BUSY` errors.
- Backpressure is explicit and measurable (bounded channel depth) instead of implicit (Mutex
  contention visible only as timeouts).
- Coupling at the pool boundary drops from approximately 0.85 (every store tightly coupled to
  the single writer Mutex) to approximately 0.3 (every store holds a channel send endpoint and
  awaits a oneshot reply).
- Reads remain fully concurrent and are unaffected by writer task load.

### Negative

- Cross-request batching introduces a new failure mode: a slow operation in one batch can cause
  a `ROLLBACK` that affects unrelated operations from other agents. The retry signal
  (`StorageError::TransactionRolledBack`) must be handled at the verb handler level.
- The `with_writer` helper in five store files must be updated. This is a bounded and mechanical
  change, but it is a large diff.
- The `begin_tx()` seam (`sql_bridge.rs:804`) currently opens a standalone connection outside
  the write-queue. After this ADR, the relationship between `begin_tx()` and the writer task
  must be clarified in the follow-up ADR for #195. Until that ADR lands, `begin_tx()` retains
  its current standalone-connection behavior.
- Backpressure is visible to callers as a blocking `channel.send()` rather than an immediate
  error. This is intentional, but callers must not hold other resources while blocked.

### Neutral

- `kkernel exec --ops-file` currently bypasses the daemon fast-path (`exec.rs:408`). This
  behavior is unchanged by this ADR. The write-queue is inside the daemon; bulk apply uses an
  in-process runtime and is therefore not affected by this ADR's changes until the #195
  follow-up threads transaction context through `dispatch_request_local`.

---

## Migration and sequencing

The recommended landing sequence is:

**Slice 1 (in progress, no ADR required)**: Extract `WAL_AUTOCHECKPOINT_PAGES`,
`JOURNAL_SIZE_LIMIT_BYTES`, and `busy_timeout` from `pool.rs` constants into `PoolConfig`
fields with environment variable overrides. Add a passive checkpoint periodic task in
`run_daemon`. This is a standalone de-risk measure that reduces wedge probability immediately.
Estimated effort: 2 days. Does not require this ADR to be accepted.

**Slice 2 (this ADR): Write-owner task and write queue**. After ADR-067 is accepted:

1. Implement `WriterTask` in `crates/khive-db/src/writer_task.rs`.
2. Add `WriteRequest` message type and `WriteChannel` wrapper to `pool.rs` or a new module.
3. Replace `with_writer` in `entity.rs`, `note.rs`, `graph.rs`, `text.rs`, and `vectors.rs`.
4. Update `SqlBridge::execute_batch` and related pool writer paths in `sql_bridge.rs`.
5. Start `WriterTask` in `run_daemon` alongside the checkpoint task.
6. Gate behind `KHIVE_WRITE_QUEUE=1` environment variable for initial rollout; remove the gate
   after integration tests confirm correctness under concurrent load.
7. Add the transaction watchdog inside `WriterTask`.
   Estimated effort: 2 to 3 weeks including integration test coverage.

**Slice 3 (follow-up ADR): #195 cross-op atomicity**. After Slice 2 is stable: draft the
follow-up ADR for threading `SqlTransaction` context through `dispatch_request_local`. Depends
on Slice 2's `WriterTask` being the stable owner of write connections.
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

---

## References

- ADR-005: Storage capability traits — 8 backend-neutral traits; `SqlAccess::begin_tx` seam
- ADR-017: Pack standard — `with_writer` pattern is the current pack write convention
- ADR-028: Pack-scoped backends — `ConnectionPool` ownership model
- ADR-049: khived daemon — socket framing and `forward_or_spawn` protocol (unchanged)
- `crates/khive-db/src/pool.rs` — single writer Mutex, WAL constants, checkout/busy timeouts
- `crates/khive-db/src/sql_bridge.rs` — `begin_tx` seam, `BEGIN IMMEDIATE` hardcoding
- `crates/khive-db/src/stores/entity.rs` — `with_writer` pattern (lines 68-80)
- `crates/khive-db/src/stores/note.rs` — `with_writer` pattern (lines 68-80)
- `crates/khive-db/src/stores/graph.rs` — `with_writer` pattern (lines 111-130)
- `crates/khive-runtime/src/daemon.rs` — `run_daemon` accept loop (lines 466-479)
- `crates/kkernel/src/exec.rs` — `apply_ops_file` and daemon fast-path bypass (lines 205-264, 408)
- Issue #195: cross-op atomicity for `--ops-file` bulk apply
