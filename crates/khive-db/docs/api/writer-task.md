# Writer Task

`WriterTask` (`crates/khive-db/src/writer_task.rs`) is the ADR-067
Component A single-writer-connection mechanism: a dedicated background task
that owns one standalone writer `rusqlite::Connection` and drains a bounded
channel of typed write requests, issuing `BEGIN IMMEDIATE` once per request.
This is the function-specific technical reference for its migration scope
and failure modes.

## Migration-slice scope

Slice 1 builds the mechanism and wires exactly one write path
(`SqlEntityStore::upsert_entities`, gated behind `KHIVE_WRITE_QUEUE=1` /
`PoolConfig::write_queue_enabled`) through it. It commits one request per
`BEGIN IMMEDIATE` — Component B's batched-commit window and three-level
SAVEPOINT hierarchy, Component C's checkpoint coordination signal, and
Component D's transaction watchdog are later slices. With only one store
migrated, other write paths still open their own writer connections via the
pool's Mutex-guarded `writer()` connection, so this slice does not yet
reduce contention or claim the ADR's single-writer guarantee on its own — it
proves the mechanism works and that the flag-off path is unchanged.

`spawn` opens a dedicated standalone writer connection independent of that
Mutex-guarded connection. The lifetime connection is an infrastructure open
and does not enter write-traffic counters; the drain loop increments the
writer-task acquisition class once per dequeued top-level request or successful
`BEGIN IMMEDIATE`. `capacity` bounds the channel (ADR-067 recommends 256;
`PoolConfig::write_queue_capacity` resolves the default from
`KHIVE_WRITE_QUEUE_CAPACITY`).

## `run_writer_task` — drain loop and failure modes

See `crates/khive-db/src/writer_task.rs` — private fn `run_writer_task`.

A `BEGIN IMMEDIATE` failure (for example, `SQLITE_BUSY` from lock
contention with an unmigrated writer path still holding the pool's writer
mutex — reachable while only `entity.rs` is routed through this channel in
this slice) replies the request's error via `AnyWriteRequest::reply_error`
without ever invoking the request's operation closure via
`AnyWriteRequest::execute_and_reply`. Slice 1 has no watchdog/retry story
for a failed `BEGIN` (Component D is a later slice); the connection simply
tries `BEGIN IMMEDIATE` fresh on the next request.

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

All three handle surfaces (`send`, `send_with_timeout`, and
`send_top_level`) use this contract. Queue backpressure and
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
