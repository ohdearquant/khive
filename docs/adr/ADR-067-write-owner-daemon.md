# ADR-067: Write-Owner Daemon — Single-Writer Task and Write Queue

**Status**: Accepted (ratified 2026-07-05)
**Date**: 2026-06-23
**Amends**: [ADR-049](./ADR-049-khived-daemon.md)
**Depends on**: [ADR-005](./ADR-005-storage-capability-traits.md),
[ADR-028](./ADR-028-pack-scoped-backends.md),
[ADR-049](./ADR-049-khived-daemon.md)

## Context

SQLite permits one active writer. Earlier store paths reached that writer through both a
pool mutex and independent file-backed connections. Concurrent daemon requests could
therefore block at different serialization points, each with its own timeout behavior.

The daemon already owns one runtime and backend per process. It is the natural place to
turn implicit SQLite contention into an explicit bounded queue. Reads remain concurrent.

## Decision

One `WriterTask` owns the mutable SQLite connection used by concurrent request traffic.
Every public runtime write is expressed as an `atomic_unit` and sent through a bounded
asynchronous channel. Only the writer task starts and ends request-time write transactions.

Sequential bootstrap and migration writes occur before the accept loop and may use a direct
startup connection. They are never concurrent with request traffic.

### 1. Typed write requests

Each request retains its natural result type:

```rust
struct WriteRequest<R: Send + 'static> {
    op: Box<
        dyn FnOnce(&rusqlite::Connection) -> Result<R, StorageError>
            + Send
    >,
    reply: oneshot::Sender<Result<R, StorageError>>,
    require_solo: bool,
}
```

The queue uses a sealed type-erased request trait so heterogeneous `R` values share one
channel without reducing them to a row count. Batch summaries and typed domain results are
returned unchanged.

An operation closure receives a connection already inside the writer task's transaction. It
must not issue `BEGIN`, `COMMIT`, or `ROLLBACK`. It may use named savepoints.

### 2. Backpressure

The channel is bounded. Producers await capacity; they do not use unbounded buffering.
Callers needing a deadline wrap queue admission in a timeout and receive
`StorageError::WriteQueueFull { timeout_ms }`.

Callers must not hold database guards, permits, or other scarce resources while awaiting
queue capacity.

Queue capacity, wait duration, batch size, and service duration are observable metrics.

### 3. Commit batching

The writer task collects requests until either:

- `batch_window_ms` reaches 5 ms by default; or
- `batch_max_ops` reaches 64 by default.

It opens one outer `BEGIN IMMEDIATE` transaction for the collected batch. Each request runs
inside its own savepoint:

```text
BEGIN IMMEDIATE
  SAVEPOINT req_0
    request A
  RELEASE req_0

  SAVEPOINT req_1
    request B
  ROLLBACK TO req_1   -- only B failed
  RELEASE req_1
COMMIT
```

A hard error rolls back that request's savepoint and leaves siblings intact. Existing
per-record savepoints may nest inside the request savepoint so partial-success summaries
retain their meaning.

`require_solo` flushes the current batch and executes the marked request in its own outer
transaction.

### 4. Atomic unit contract

`SqlAccess::atomic_unit` is the public closure-scoped write seam. The closure:

- is invoked by the writer task;
- cannot escape with a connection or transaction handle;
- completes before its reply is sent;
- receives one commit or rollback outcome; and
- emits dependent event and projection writes inside the same unit when required.

All entity, note, graph, text, event, sparse, vector, curation, and public SQL-bridge write
paths route through this seam. A new runtime write path that opens its own request-time
connection violates this ADR.

### 5. Checkpoint coordination

The writer task publishes a lightweight write-activity signal to the checkpoint task. The
checkpoint task uses its own connection and follows ADR-091. It never borrows the writer
task's mutable connection.

The signal reports active/idle state only; it is not a second lock. Checkpoint work remains
skip-on-busy and cannot enqueue behind ordinary writes.

### 6. Transaction watchdog

`TXN_WATCHDOG_SECS` defaults to 30 seconds and covers both acquisition of SQLite's write
lock and execution of the batch.

The connection is moved into a blocking closure. Before that move, the writer task obtains a
thread-safe SQLite interrupt handle. On timeout:

1. the async task signals `interrupt()`;
2. the blocking closure observes `SQLITE_INTERRUPT`;
3. that closure rolls back on the connection it owns;
4. the writer task awaits closure completion;
5. every request in the batch receives `WatchdogTimeout`; and
6. the task opens a fresh writer connection.

The async task never attempts rollback on a connection owned by another thread. If rollback,
SQL execution, or the blocking task itself fails, the connection is discarded and the
original batch is not retried automatically.

### 7. Shutdown

On daemon shutdown, queue admission closes. The writer task completes the in-flight batch
within the drain deadline, rejects queued-but-unstarted requests with a stable shutdown error,
and closes its connection. No reply sender may be left unresolved.

### 8. Transport

The Unix socket framing and thin-client protocol in ADR-049 are unchanged. Write
serialization is internal to the daemon and storage runtime.

## Invariants

- One request-time owner issues `BEGIN IMMEDIATE`.
- Reads never enter the write queue.
- Bootstrap writes finish before request acceptance.
- Queue memory is bounded.
- Each request has one typed reply.
- One failed request does not roll back a successful sibling.
- A closure cannot retain the connection after completion.
- Watchdog rollback runs on the owning blocking thread.
- A poisoned or interrupted connection is never reused.

## Migration

1. Introduce the typed request wrapper and bounded channel.
2. Start `WriterTask` during backend construction.
3. Route every store write helper through `atomic_unit`.
4. Route curation and SQL-bridge request writes through the same seam.
5. Preserve per-record savepoints and result summaries.
6. Add batching and solo-request behavior.
7. Wire checkpoint activity and watchdog interruption.
8. Remove standalone request-time writer creation.
9. Prove bootstrap writes complete before the accept loop starts.

## Verification

Tests must cover:

- concurrent writes issuing one outer writer transaction at a time;
- bounded queue backpressure and admission timeout;
- heterogeneous typed replies;
- sibling isolation under one failed savepoint;
- nested per-record partial success;
- solo-request flushing;
- reads progressing during queued writes;
- watchdog timeout during lock acquisition and execution;
- rollback and connection replacement after interrupt;
- panic and rollback-failure handling;
- shutdown resolving every reply; and
- no request-time direct `BEGIN IMMEDIATE` outside the writer task.

## Alternatives considered

| Alternative                                      | Reason rejected                                                            |
| ------------------------------------------------ | -------------------------------------------------------------------------- |
| Tune busy timeout and checkpoint thresholds only | Reduces symptoms but leaves multiple writer entry points.                  |
| Use multiple writer connections                  | SQLite still serializes writers and would move contention into the engine. |
| Serialize at the transport layer                 | Misses internal writes and couples storage correctness to one protocol.    |
| Make the queue unbounded                         | Converts contention into unbounded memory growth.                          |
| Route reads through the writer                   | Unnecessarily destroys read concurrency.                                   |

## Consequences

### Positive

- Backpressure and write ownership become explicit and observable.
- Commit batching amortizes WAL lock acquisition.
- Per-request savepoints preserve independent result semantics.
- Transaction lifetime is structurally closure-scoped.

### Negative

- Every request-time write path must use the shared seam.
- Queue wait replaces implicit mutex wait and must be accounted for in deadlines.
- Watchdog and connection-replacement behavior add failure-handling complexity.
- Batching introduces a small intentional commit delay.

## Scope

This ADR governs intra-process request-time SQLite writes. It does not define:

- cross-process write coordination;
- a shared-server database backend;
- changes to the daemon transport; or
- event retention and ANN snapshot policy.

## References

- [ADR-005](./ADR-005-storage-capability-traits.md): storage traits and SQL access
- [ADR-028](./ADR-028-pack-scoped-backends.md): backend ownership
- [ADR-049](./ADR-049-khived-daemon.md): daemon transport
- [ADR-068](./ADR-068-process-isolation-topology.md): process isolation
- [ADR-091](./ADR-091-wal-snapshot-lifetime.md): checkpoint and transaction-age policy
