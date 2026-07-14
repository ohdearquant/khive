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

`spawn` opens a dedicated standalone writer connection
(`ConnectionPool::open_standalone_writer`), independent of that
Mutex-guarded connection. `capacity` bounds the channel (ADR-067 recommends
256; `PoolConfig::write_queue_capacity` resolves the default from
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

Exits when every `WriterTaskHandle` clone is dropped and the channel closes
(`rx.recv()` returns `None`), or if the blocking closure panics. Either way,
this task's `rx` is dropped when the function returns, which is what turns
subsequent `WriterTaskHandle::send` calls into `StorageError::Internal`
(ADR-067 failure-mode table: "Receiver drop (writer task stopped)" /
"Writer task panic").
