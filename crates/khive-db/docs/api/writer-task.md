# Writer Task

`WriterTask` (`crates/khive-db/src/writer_task.rs`) is the ADR-067
Component A single-writer-connection mechanism: a dedicated background task
that owns one standalone writer `rusqlite::Connection` and drains a bounded
channel of typed write requests, issuing `BEGIN IMMEDIATE` for each request
with a bounded retry (up to three attempts) on a busy/locked refusal, the
attempts sharing one configured `busy_timeout` acquisition budget. This is
the function-specific technical reference for its migration scope and
failure modes.

## Migration-slice scope (historical) — current routed-call inventory and admission mode live elsewhere

This section originally described Slice 1, when only
`SqlEntityStore::upsert_entities` was wired through the queue behind
`KHIVE_WRITE_QUEUE=1`. That single-path scope is superseded: the current
per-writer routing inventory — which callers reach `WriterTaskHandle`
queue-first, which are exempt by design (checkpointing, startup/schema
migrations, recovery bookkeeping), and which route through the same handle
without the per-request transaction wrap (top-level maintenance) — is
maintained as a single table in `crates/khive-db/src/writer_task.rs`'s
module-level doc comment ("ADR-136 D1 gate 5: writer classification"), not
duplicated here. The current admission mode — one shared admission authority
keyed by canonical database identity, a bounded per-operation admission
deadline, and a caller-visible `writer_queue_saturated` result — is
[ADR-131](../../../../docs/adr/ADR-131-batch-write-admission-control.md)'s
contract; whether `write_queue_enabled` defaults on for a given deployment is
governed by [ADR-135](../../../../docs/adr/ADR-135-write-scaling-demand-before-ownership.md)
Amendment 1 and [ADR-136](../../../../docs/adr/ADR-136-fair-write-admission-default.md)'s
strict-routing gates, not by this document.

Component B's batched-commit window and three-level SAVEPOINT hierarchy and
Component D's transaction watchdog remain unshipped: the drain loop still
commits one request per `BEGIN IMMEDIATE`.

`spawn` opens a dedicated standalone writer connection independent of the
pool's Mutex-guarded `writer()` connection used by any exempt or unrouted
path. The lifetime connection is an infrastructure open
and does not enter write-traffic counters; the drain loop increments the
writer-task acquisition class once per dequeued top-level request or successful
`BEGIN IMMEDIATE`. `capacity` bounds the channel (ADR-067 recommends 256;
`PoolConfig::write_queue_capacity` resolves the default from
`KHIVE_WRITE_QUEUE_CAPACITY`).

## Proposed disk-reserve admission (#1844; not implemented)

[ADR-154](../../../../docs/adr/ADR-154-sqlite-disk-reserve-admission.md) proposes
the disk-reserve contract. This section is an implementation map, not a claim
about current behavior.

The writer task does not sample free space when a caller enters the bounded
channel. At execution time it acquires the shared volume lease, successfully
executes `BEGIN IMMEDIATE`, and then probes the volume before invoking the
request closure. A refusal or probe failure runs `ROLLBACK` and replies with
the typed capacity error; a successful rollback does not retire the task. The
lease remains held through the ordinary `COMMIT` or `ROLLBACK`.

Every path follows the same lock order: volume lease, SQLite writer
acquisition, capacity probe, first logical write. Top-level requests acquire
the same lease and probe immediately before their first SQLite call because
they deliberately have no explicit `BEGIN`. Pooled/standalone writers and
startup migrations receive equivalent admission outside this drain loop.
Volume-lease acquisition has its own configured deadline; it does not reuse
the queue-only `write_admission_deadline_ms` governed by ADR-131.

The two current migration bootstrap writes happen before a migration
transaction exists: `apply_schema_plan` executes `SCHEMA_VERSION_TABLE`, and
`run_migrations_locked` executes `MIGRATION_TRACKING_TABLE`. Each acquires the
volume lease and probes immediately before its autocommit `execute_batch`,
skips that call on refusal, and holds the lease until the connection returns
to autocommit. Subsequent migration transactions use the ordinary
post-`BEGIN` probe.

Transaction terminators, checkpointing, diagnostics, reader release, and
recovery are explicit refusal bypasses. The guard therefore belongs at the
logical-request boundary, never inside generic statement execution. Native
`SQLITE_FULL` remains a separate higher-severity stage and is not rendered as
a successful capacity refusal. Checkpoint bypass includes PASSIVE,
ADR-091's scheduled threshold-armed `maybe_truncate`, and operator-authorized
stronger checkpoints.

## Writer-stage telemetry (#1849)

Every completed writer-task request records a backend-scoped in-memory sample
with four independent stages: `queue_wait_micros` starts before bounded-channel
admission and ends when the drain loop dequeues the request;
`transaction_acquire_micros` measures the bounded `BEGIN IMMEDIATE` attempt
sequence, including its retry backoffs; `body_micros`
measures the typed operation closure; and `commit_micros` measures only
SQLite's `COMMIT`. `total_micros`, queue depth at entry, and observation time
remain siblings. A top-level request has zero acquisition/commit stages; a
request that fails before a stage runs likewise reports zero for that stage.
Rollback/recovery work remains visible in the difference between the total
and named stages rather than being falsely attributed to COMMIT.

`last_writer_stage_observation(pool)` is a pure per-backend read used by the
daemon metrics frame. When a request crosses the existing slow-write
threshold, the durable `slow_write` sink row also carries the four stage
fields (while retaining `elapsed_ms` and `queue_depth` for compatibility).
Observation is completed before the oneshot reply wakes the caller, so a
successful response cannot race ahead of its telemetry sample.

## `run_writer_task` — drain loop and failure modes

See `crates/khive-db/src/writer_task.rs` — private fn `run_writer_task`.

A busy/locked `BEGIN IMMEDIATE` refusal (for example, from an explicitly
exempt writer or a non-strict compatibility fallback still holding another
writer connection) is retried only at this pre-execution seam. The writer
makes at most three total BEGIN attempts, sleeping 5 ms and then 10 ms between
them, and the attempts together share a single configured `busy_timeout`
acquisition budget rather than each waiting out a full window of their own:
before each retry the connection's busy timeout is lowered to whatever
remains of that budget and restored afterward, and a refusal that has
already exhausted the budget is not retried. Persistent contention is
therefore bounded to at most one busy-timeout window plus 15 ms of explicit
backoff, not the sum of three; the request's operation closure remains owned
and uninvoked throughout. Non-busy BEGIN failures are never retried. After
the bounded attempts, a final
failure replies via `AnyWriteRequest::reply_error` without ever invoking the
request's operation closure via `AnyWriteRequest::execute_and_reply`.
For transaction-wrapped requests, the scoped `writer_task_tx` registry span
is dropped before the oneshot reply wakes the caller, both after a completed
transaction and after a failed `BEGIN`. A caller that has observed its reply
therefore cannot still observe that request as an open SQL transaction.
When the final raw SQLite code is `SQLITE_BUSY` or `SQLITE_LOCKED`, the caller
receives `StorageError::WriterTaskBusy` with the connection's configured busy
timeout. Its runtime/MCP classification remains `writer_task_begin_busy`,
`retryable: true`, and `operation: "writer_task_begin"`, with no queue-admission
scope or retry hint. Any other `BEGIN` error retains the generic pool failure.
Transient contention does not retire the writer task.

Request-path stores refresh a missing construction-time handle at write time.
Strict routing therefore fails closed before any store fallback, and a
non-strict fallback emits a store-specific `direct_route_violation`. The
strict-default flip is intentionally outside this tranche: ADR-135 F2 and
ADR-136 D2 still gate it on accepted production A/B and release evidence.

Exits normally when every `WriterTaskHandle` clone is dropped and the channel
closes (`rx.recv()` returns `None`). A panic while executing a request, a failed
rollback, or a connection that remains outside autocommit mode instead puts
that writer-task instance into a permanent terminal state. The task does not
restart: the pool retains the same handle, so subsequent sends observe the
closed receiver rather than creating a replacement task.

### Terminal failure contract

Panic containment happens inside the concrete `WriteRequest<R>`, after its
typed reply sender has been separated from the operation closure. This allows
the active caller to receive `StorageError::WriterTaskTerminated` with the
strongest state the writer task can prove. Once a request makes the task
terminal, the receiver is closed **before** buffered requests are drained.
Closing first prevents concurrent producers from extending the drain forever;
drained requests receive a typed error without invoking their closures.

| Request position / condition                                               | `WriterTaskRequestState` | Guarantee                                                                                      |
| -------------------------------------------------------------------------- | ------------------------ | ---------------------------------------------------------------------------------------------- |
| Active transaction-wrapped request panics; `ROLLBACK` succeeds             | `TransactionRolledBack`  | The request ran, but its SQLite transaction was rolled back; no wrapped database write commits |
| Active request fails or `COMMIT` fails; `ROLLBACK` fails                   | `SideEffectsUnknown`     | The request ran and the task cannot prove the transaction's final state                        |
| Any transaction terminator reports success but autocommit remains disabled | `SideEffectsUnknown`     | The connection is poisoned and is retired before it can serve another request                  |
| Active top-level request panics or returns with an open transaction        | `SideEffectsUnknown`     | The task cannot prove which top-level side effects committed                                   |
| Request was buffered behind the terminal request                           | `NotStarted`             | Its operation closure is never invoked                                                         |
| Send begins after the receiver has closed                                  | `NotStarted`             | The request was not accepted and its operation closure is never invoked                        |
| An accepted request loses its reply outside the contained request path     | `SideEffectsUnknown`     | The caller cannot prove whether the operation began or which side effects occurred             |

### Failure counters (ADR-133 D8)

`ConnectionPool::writer_acquisition_snapshot` carries two counters populated
exclusively at the `run_writer_task` drain loop's outer match on the
per-request outcome, so they stay correct without anyone re-classifying a
verb by hand:

| Counter | Population |
| --- | --- |
| `writer_task_request_failures` | Incremented once for every dequeued request whose processing at the writer seam terminated in error — every row of the table above except `NotStarted` outcomes for requests that never reached the seam, plus a request whose blocking closure fails to join outside the panic boundary. |
| `writer_task_side_effects_unknown` | The subset of `writer_task_request_failures` whose `WriterTaskRequestState` was exactly `SideEffectsUnknown`. |

A request that is only ever buffered behind another request's terminal
failure (`NotStarted`, drained via `close_and_fail_queued_requests`) never
reached the seam and moves neither counter — it was refused by the closed
queue, not by its own execution.

`crates/khive-db/src/diagnostics.rs` republishes both directly under
`db_diagnostics.writer_contention` as plain `u64` fields — unlike the
runtime-supplied `audit_*` fields, they come straight from the pool and are
never `Option`.

BEGIN contention has two additional counters at the same surface, one a
subset of the other: the backward-compatible `writer_task_begin_busy` field
increments for every busy/locked `BEGIN IMMEDIATE` refusal, whether or not a
retry follows it — this preserves its pre-retry meaning as the total
contention count. `writer_task_begin_busy_absorbed` increments for the
subset of those refusals that a subsequent bounded BEGIN attempt went on to
absorb, so the caller never observed them; `writer_task_begin_busy -
writer_task_begin_busy_absorbed` is the count the caller actually observed
as a failure. A successful retry therefore still moves `writer_task_begin_busy`
without the caller ever seeing a failure; a request refused N times before
either the delay schedule or the shared `busy_timeout` budget runs out moves
`writer_task_begin_busy` N times and `writer_task_begin_busy_absorbed`
N-1 times (every refusal but the final, unretried one).

### Bounded enqueue admission (#1382)

Production store write paths and the SQL bridge's writer requests use
`send_bounded` / `send_top_level_bounded`, which bound only the
enqueue-capacity wait with `PoolConfig::write_admission_deadline_ms`
(ADR-131 Decision 2; default 2000 ms, validated range [100, 10000] ms,
captured at `spawn` as `WriterTaskHandle::enqueue_timeout`) before falling
back to `StorageError::WriteQueueFull`. This is a dedicated admission
authority distinct from `PoolConfig::checkout_timeout` (reader/pool
checkout). Once a request is accepted onto the
channel, the reply wait is unbounded by this mechanism, identical to plain
`send`/`send_top_level`. The raw `send`, `send_top_level`, and
`send_with_timeout` methods remain the underlying primitives — indefinite
channel backpressure by default, or a caller-supplied deadline — and stay
available to callers and tests that need that behavior explicitly.

All five handle surfaces (`send`, `send_with_timeout`, `send_top_level`,
`send_bounded`, `send_top_level_bounded`) use this contract. Queue
backpressure and
`WriteQueueFull` remain unchanged: a timeout while waiting for capacity is
not a writer-task termination. `WriterTaskTerminated` is deliberately not
retryable because retrying an outcome marked `SideEffectsUnknown` could
duplicate a committed side effect; callers must make a new, explicit decision
using operation-level idempotency.

When `ROLLBACK` succeeds and autocommit mode is restored, the failure is not
terminal. An operation error is returned unchanged; a failed `COMMIT` retains
the existing `writer_task_commit` pool error. In both cases the writer remains
available for the next request.

The drain loop verifies `Connection::is_autocommit()` before dispatching every
request. This check is especially important for top-level requests: because
they intentionally skip `BEGIN IMMEDIATE`, dispatching one on a connection
left inside an earlier failed transaction would silently join that stale
transaction.
