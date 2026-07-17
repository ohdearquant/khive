//! Single-writer task and bounded write queue (ADR-067 Component A).
//!
//! `WriterTask` (via `spawn` and the drain loop `run_writer_task`) owns a
//! dedicated standalone writer `rusqlite::Connection` and is the only code
//! path that issues `BEGIN IMMEDIATE` for write traffic routed through the
//! channel it drains. Callers reach it exclusively through a
//! [`WriterTaskHandle`], sending a typed closure and awaiting a typed
//! oneshot reply so each store method's natural return type (e.g.
//! `BatchWriteSummary`) survives the trip through the type-erased channel
//! unmodified — a flat `Result<u64, StorageError>` reply would conflate
//! `affected`/`failed` into one count and drop `first_error`.
//!
//! See `crates/khive-db/docs/api/writer-task.md` for migration-slice scope
//! (which write paths currently route through this vs. the legacy
//! pool-mutex path) and the ADR-067 component breakdown.

use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use khive_storage::error::StorageError;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

/// Closure signature for a write operation executed against the writer
/// task's dedicated connection.
///
/// `conn` is already inside a `BEGIN IMMEDIATE` transaction opened by
/// `run_writer_task` when this runs. The closure must issue DML (and, in
/// later slices, named `SAVEPOINT`s) only — never a bare `BEGIN` / `COMMIT`
/// / `ROLLBACK` — a nested bare `BEGIN IMMEDIATE` would violate SQLite's
/// nested-transaction rule and return `SQLITE_ERROR: cannot start a
/// transaction within a transaction` (ADR-067 lines 271-276).
type WriteOp<R> = Box<dyn FnOnce(&Connection) -> Result<R, StorageError> + Send>;

/// One write request awaiting execution by the writer task.
///
/// Carries a typed closure and a typed oneshot reply so that the concrete
/// return type `R` (e.g. `BatchWriteSummary`) is preserved end to end,
/// while [`AnyWriteRequest`] lets the drain loop hold heterogeneous
/// requests in one homogeneous channel.
///
/// `top_level` (ADR-067 Component A): when `true`,
/// the drain loop runs this request's operation WITHOUT wrapping it in a
/// `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` — still serialized through the
/// single writer owner (only one request drains at a time regardless of
/// this flag), but with the transaction wrap skipped entirely. Exists for
/// statements SQLite forbids inside any open transaction (e.g. `VACUUM`);
/// see [`WriterTaskHandle::send_top_level`].
pub struct WriteRequest<R: Send + 'static> {
    op: WriteOp<R>,
    reply: oneshot::Sender<Result<R, StorageError>>,
    top_level: bool,
}

mod sealed {
    /// Restricts [`super::AnyWriteRequest`] to implementations defined in
    /// this module — only [`super::WriteRequest<R>`] implements it.
    pub trait Sealed {}
}

impl<R: Send + 'static> sealed::Sealed for WriteRequest<R> {}

/// Type-erased write request the writer task's drain loop can hold in a
/// homogeneous channel (`mpsc::Sender<Box<dyn AnyWriteRequest + Send>>`),
/// while each concrete [`WriteRequest<R>`] still carries its own typed
/// reply. Sealed: only this module may implement it (ADR-067 lines
/// 210-212).
pub trait AnyWriteRequest: sealed::Sealed + Send {
    /// Runs this request's operation against `conn`, commits or rolls back
    /// the enclosing transaction based on the outcome, and sends the
    /// (possibly commit-failure-adjusted) result to the request's oneshot
    /// reply channel.
    ///
    /// `conn` must already be inside a successfully-opened `BEGIN IMMEDIATE`
    /// transaction opened by the caller (`run_writer_task`) — this method
    /// issues only `COMMIT` / `ROLLBACK`, never `BEGIN`, so `run_writer_task`
    /// remains the sole issuer of `BEGIN IMMEDIATE` (ADR-067 Component A).
    /// Callers must use [`Self::reply_error`] instead when the enclosing
    /// `BEGIN IMMEDIATE` itself failed — this method must not be invoked in
    /// that case.
    fn execute_and_reply(self: Box<Self>, conn: &Connection);

    /// Runs this request's operation directly against `conn` — no
    /// transaction wrap, no `COMMIT`/`ROLLBACK` — and sends the result to
    /// the request's oneshot reply channel.
    ///
    /// Used only for [`Self::is_top_level`] requests: the drain loop calls
    /// this INSTEAD of `execute_and_reply` for such requests, skipping
    /// `BEGIN IMMEDIATE` entirely so a statement that must run outside any
    /// transaction (e.g. `VACUUM`) can still be serialized through the
    /// single writer owner.
    fn execute_and_reply_top_level(self: Box<Self>, conn: &Connection);

    /// Replies with `err` without running this request's operation or
    /// touching `conn`.
    ///
    /// Used when the enclosing `BEGIN IMMEDIATE` failed (for example,
    /// `SQLITE_BUSY` from lock contention with an unmigrated writer path
    /// still holding the pool's writer mutex — reachable while only
    /// `entity.rs` is routed through this channel). Running the operation
    /// anyway would execute its DML against `conn` in autocommit mode,
    /// landing partial writes for a request the caller is told failed.
    /// Skipping the operation entirely keeps "the caller got an error" and
    /// "no rows landed" true together.
    fn reply_error(self: Box<Self>, err: StorageError);

    /// `true` if the drain loop must run this request via
    /// [`Self::execute_and_reply_top_level`] (no transaction wrap) instead
    /// of [`Self::execute_and_reply`] (wrapped in `BEGIN IMMEDIATE`).
    fn is_top_level(&self) -> bool;
}

impl<R: Send + 'static> AnyWriteRequest for WriteRequest<R> {
    fn execute_and_reply(self: Box<Self>, conn: &Connection) {
        let outcome = (self.op)(conn);
        let final_result = match outcome {
            Ok(value) => match conn.execute_batch("COMMIT") {
                Ok(()) => Ok(value),
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(StorageError::Pool {
                        operation: "writer_task_commit".into(),
                        message: e.to_string(),
                    })
                }
            },
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        };
        // The receiver may already be gone (caller dropped its future) —
        // that is not this task's problem to report; it just moves on.
        let _ = self.reply.send(final_result);
    }

    fn execute_and_reply_top_level(self: Box<Self>, conn: &Connection) {
        let outcome = (self.op)(conn);
        // No COMMIT/ROLLBACK here: this request explicitly did not open a
        // transaction, so there is nothing for this method to close.
        let _ = self.reply.send(outcome);
    }

    fn reply_error(self: Box<Self>, err: StorageError) {
        // Same "receiver may already be gone" reasoning as above — send and
        // move on regardless of outcome.
        let _ = self.reply.send(Err(err));
    }

    fn is_top_level(&self) -> bool {
        self.top_level
    }
}

/// Sender half of the write queue. Cheaply cloneable (wraps an
/// `mpsc::Sender`) — every migrated store that shares one writer task holds
/// a clone of this handle.
#[derive(Clone, Debug)]
pub struct WriterTaskHandle {
    tx: mpsc::Sender<Box<dyn AnyWriteRequest + Send>>,
}

impl WriterTaskHandle {
    /// Enqueue a write operation and return the oneshot receiver its reply
    /// will arrive on, once the request has actually been accepted onto the
    /// channel.
    ///
    /// Shared by [`Self::send`] and [`Self::send_with_timeout`] so that a
    /// caller-supplied deadline (see `send_with_timeout`) can bound ONLY
    /// this enqueue step — never the reply-wait that follows it. Once this
    /// returns `Ok`, the request has been accepted by the writer task and
    /// will run to completion; the returned receiver must be awaited without
    /// a timeout; abandoning it here would silently drop the request's
    /// eventual result, not cancel the request itself.
    async fn enqueue<R, F>(
        &self,
        op: F,
    ) -> Result<oneshot::Receiver<Result<R, StorageError>>, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        self.enqueue_inner(op, false).await
    }

    /// Shared enqueue path for both transaction-wrapped ([`Self::enqueue`])
    /// and top-level ([`Self::send_top_level`]) requests — `top_level`
    /// controls which [`AnyWriteRequest`] method the drain loop invokes.
    async fn enqueue_inner<R, F>(
        &self,
        op: F,
        top_level: bool,
    ) -> Result<oneshot::Receiver<Result<R, StorageError>>, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = WriteRequest {
            op: Box::new(op),
            reply: reply_tx,
            top_level,
        };

        self.tx
            .send(Box::new(request))
            .await
            .map_err(|_| StorageError::Internal("writer task channel closed".to_string()))?;

        Ok(reply_rx)
    }

    /// Send a write operation to the writer task and await its typed reply.
    ///
    /// Backpressure: this suspends on the channel's `send().await` when the
    /// bounded queue is full (ADR-067 "Channel capacity and queue-full
    /// policy") — there is no `try_send` escape hatch. Callers that need a
    /// deadline on that wait should use [`Self::send_with_timeout`] instead.
    pub async fn send<R, F>(&self, op: F) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let reply_rx = self.enqueue(op).await?;
        reply_rx.await.map_err(|_| {
            StorageError::Internal("writer task dropped before replying".to_string())
        })?
    }

    /// Like [`Self::send`], but bounds the wait for the bounded channel to
    /// free capacity with `timeout`.
    ///
    /// The timeout applies ONLY to enqueueing the request (the channel
    /// `send().await` that can suspend on a full queue) — never to waiting
    /// for the writer task's reply once the request has been accepted.
    /// `StorageError::WriteQueueFull` means exactly "the bounded channel was
    /// full and this request was never accepted"; it must never be returned
    /// for a request that was accepted and is still executing (or already
    /// committed) by the time `timeout` elapses — that would misreport a
    /// slow op or a lock wait as a queue-capacity failure, and could tell a
    /// caller a write failed when it actually landed. ADR-067's queue-full
    /// policy has no immediate-error `try_send` path — only this caller-side
    /// deadline on the enqueue step.
    pub async fn send_with_timeout<R, F>(
        &self,
        op: F,
        timeout: std::time::Duration,
    ) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let reply_rx = match tokio::time::timeout(timeout, self.enqueue(op)).await {
            Ok(Ok(reply_rx)) => reply_rx,
            Ok(Err(e)) => return Err(e),
            Err(_elapsed) => {
                return Err(StorageError::WriteQueueFull {
                    timeout_ms: timeout.as_millis() as u64,
                })
            }
        };

        reply_rx.await.map_err(|_| {
            StorageError::Internal("writer task dropped before replying".to_string())
        })?
    }

    /// Send a write operation that MUST run outside any open transaction
    /// (e.g. `VACUUM`, which SQLite forbids inside `BEGIN`/`COMMIT`) and
    /// await its typed reply.
    ///
    /// Still serialized through the same single writer owner as
    /// [`Self::send`] — the request goes through the identical bounded
    /// channel and drain loop, one request at a time — but the drain loop
    /// skips the per-request `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` wrap
    /// entirely for this request (ADR-067 Component A). The single-writer
    /// guarantee is preserved; only
    /// the transaction wrap is skipped.
    pub async fn send_top_level<R, F>(&self, op: F) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let reply_rx = self.enqueue_inner(op, true).await?;
        reply_rx.await.map_err(|_| {
            StorageError::Internal("writer task dropped before replying".to_string())
        })?
    }

    /// Current write-queue backlog depth: requests enqueued but not yet
    /// accepted by the writer task's drain loop.
    ///
    /// Reads `mpsc::Sender::max_capacity() - capacity()`, so it is a
    /// point-in-time snapshot racy under concurrent senders/the drain loop
    /// draining concurrently — acceptable for a monitoring gauge (the
    /// load/perf harness metrics read-surface), never used for any correctness
    /// decision.
    pub fn queue_depth(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    /// The bounded channel's configured capacity
    /// (`PoolConfig::write_queue_capacity`).
    pub fn capacity(&self) -> usize {
        self.tx.max_capacity()
    }
}

/// Spawn the write-owner task (ADR-067 Component A) on the current Tokio
/// runtime.
///
/// Opens a dedicated standalone writer connection
/// ([`ConnectionPool::open_standalone_writer`]), independent of the pool's
/// Mutex-guarded `writer()` connection used by unmigrated paths. Returns the
/// cloneable [`WriterTaskHandle`] sender half; the task runs until every
/// handle clone is dropped and the channel closes.
///
/// `capacity` bounds the channel (`PoolConfig::write_queue_capacity` /
/// `KHIVE_WRITE_QUEUE_CAPACITY`, ADR-067 recommends 256).
///
/// # Errors
/// Must be called from within a Tokio runtime context (calls
/// `tokio::spawn`). Returns an error if the pool cannot open a standalone
/// writer connection (e.g. an in-memory pool has no standalone-connection
/// support). See `crates/khive-db/docs/api/writer-task.md` for the
/// migration-slice scope this commits per `BEGIN IMMEDIATE`.
pub fn spawn(pool: &ConnectionPool, capacity: usize) -> Result<WriterTaskHandle, SqliteError> {
    let conn = pool.open_standalone_writer()?;
    let (tx, rx) = mpsc::channel(capacity.max(1));
    tokio::spawn(run_writer_task(conn, rx));
    Ok(WriterTaskHandle { tx })
}

/// Drain loop: the sole caller of `BEGIN IMMEDIATE` for write traffic routed
/// through the channel. A `BEGIN IMMEDIATE` failure replies the request's
/// error via [`AnyWriteRequest::reply_error`] without invoking the
/// request's closure; no retry — the connection tries fresh next request.
/// Exits when every [`WriterTaskHandle`] clone drops and the channel closes,
/// or the blocking closure panics — either way `rx` drops with it, which is
/// what turns subsequent `send` calls into `StorageError::Internal`. See
/// `crates/khive-db/docs/api/writer-task.md` for the ADR-067 failure-mode
/// table this implements.
async fn run_writer_task(
    mut conn: Connection,
    mut rx: mpsc::Receiver<Box<dyn AnyWriteRequest + Send>>,
) {
    while let Some(request) = rx.recv().await {
        let outcome = tokio::task::spawn_blocking(move || {
            if request.is_top_level() {
                // ADR-067 Component A:
                // no BEGIN IMMEDIATE for this request — some statements
                // (e.g. VACUUM) are rejected by SQLite inside any open
                // transaction. Still runs on this task's dedicated
                // connection and still serialized one-request-at-a-time by
                // this same drain loop, so the single-writer guarantee
                // holds; only the transaction wrap is skipped.
                request.execute_and_reply_top_level(&conn);
                return conn;
            }
            let _tx_handle =
                khive_storage::tx_registry::register(Some("writer_task_tx".to_string()));
            match conn.execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => request.execute_and_reply(&conn),
                Err(e) => {
                    // Do NOT run the request's operation: `conn` never
                    // entered a transaction, so executing the op's DML here
                    // would run in autocommit mode and land partial writes
                    // for a request the caller is about to be told failed.
                    tracing::warn!(
                        error = %e,
                        "writer task: BEGIN IMMEDIATE failed; replying an \
                         error without running the request's operation"
                    );
                    request.reply_error(StorageError::Pool {
                        operation: "writer_task_begin".into(),
                        message: e.to_string(),
                    });
                }
            }
            conn
        })
        .await;

        match outcome {
            Ok(returned_conn) => conn = returned_conn,
            Err(join_err) => {
                tracing::error!(
                    error = %join_err,
                    "writer task blocking closure panicked; writer task is exiting"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;
    use serial_test::serial;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn file_pool(path: &std::path::Path) -> ConnectionPool {
        let cfg = PoolConfig {
            path: Some(path.to_path_buf()),
            ..PoolConfig::default()
        };
        ConnectionPool::new(cfg).expect("pool open")
    }

    // `#[serial(tx_registry)]`: `run_writer_task` registers a `writer_task_tx`
    // handle in the process-wide `tx_registry` singleton for the life of each
    // `BEGIN IMMEDIATE`. Tests that observe the registry (the checkpoint
    // `tx_age_sweep_*` group) read `tx_registry::oldest()`; an un-serialized
    // spawning test here would leak a longer-lived `writer_task_tx` into that
    // read and make the sweep name the wrong transaction. Share the key.
    #[tokio::test]
    #[serial(tx_registry)]
    async fn begin_immediate_failure_replies_error_without_running_op() {
        // Real lock contention, not a simulation: hold the database-level
        // write lock from the pool's own writer connection (the unmigrated
        // path this fix is guarding against) so the writer task's dedicated
        // connection genuinely fails `BEGIN IMMEDIATE` with `SQLITE_BUSY`
        // after a short `busy_timeout`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_begin_failure.db");
        let cfg = PoolConfig {
            path: Some(path.clone()),
            busy_timeout: Duration::from_millis(150),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
                .unwrap();
        }

        let handle = spawn(&pool, 8).expect("writer task should spawn on a file-backed pool");

        let lock_holder = pool.try_writer().unwrap();
        lock_holder.conn().execute_batch("BEGIN IMMEDIATE").unwrap();

        let op_ran = Arc::new(AtomicBool::new(false));
        let op_ran_clone = Arc::clone(&op_ran);
        let result = handle
            .send(move |conn| {
                op_ran_clone.store(true, Ordering::SeqCst);
                conn.execute("INSERT INTO t (id, v) VALUES (99, 'should-not-land')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_insert".into(),
                        message: e.to_string(),
                    })
            })
            .await;

        assert!(
            matches!(
                &result,
                Err(StorageError::Pool { operation, .. }) if operation == "writer_task_begin"
            ),
            "expected a writer_task_begin Pool error on BEGIN IMMEDIATE \
             failure, got {result:?}"
        );
        assert!(
            !op_ran.load(Ordering::SeqCst),
            "the request's operation closure must never run when BEGIN \
             IMMEDIATE fails — running it would land a partial write in \
             autocommit mode for a request the caller is told failed"
        );

        // Release the contended lock, then verify no row landed from the
        // failed request.
        lock_holder.conn().execute_batch("ROLLBACK").unwrap();
        drop(lock_holder);

        let reader = pool.reader().expect("reader");
        let count: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id = 99", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "no row must have landed from the request whose BEGIN IMMEDIATE failed"
        );
    }

    // `#[serial(tx_registry)]`: shares the key with the checkpoint
    // `tx_age_sweep_*` tests — see the note on
    // `begin_immediate_failure_replies_error_without_running_op`.
    #[tokio::test]
    #[serial(tx_registry)]
    async fn writer_task_executes_op_and_commits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_commit.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
                .unwrap();
        }

        let handle = spawn(&pool, 8).expect("writer task should spawn on a file-backed pool");

        let affected = handle
            .send(|conn| {
                conn.execute("INSERT INTO t (id, v) VALUES (1, 'hello')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_insert".into(),
                        message: e.to_string(),
                    })
            })
            .await
            .expect("op should succeed");
        assert_eq!(affected, 1);

        // Verify the write actually committed to the shared file — read it
        // back via a fresh pooled reader connection, not the writer task's
        // own connection.
        let reader = pool.reader().expect("reader");
        let v: String = reader
            .conn()
            .query_row("SELECT v FROM t WHERE id = 1", [], |row| row.get(0))
            .expect("row must be committed and visible to a reader");
        assert_eq!(v, "hello");
    }

    #[test]
    fn spawn_fails_on_in_memory_pool() {
        // In-memory pools have no standalone-connection support
        // (`ConnectionPool::open_standalone_writer`) — `spawn` must surface
        // that as an error rather than panicking. Deliberately a plain
        // `#[test]` (no Tokio runtime): `spawn` fails before it ever reaches
        // `tokio::spawn`, so no runtime is required for this path.
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();
        let result = spawn(&pool, 8);
        assert!(
            result.is_err(),
            "in-memory pools must reject spawn, not panic"
        );
    }

    #[tokio::test]
    async fn full_channel_applies_backpressure_not_immediate_error() {
        // Build the channel directly (bypassing `spawn`/`run_writer_task`)
        // so nothing ever drains it — deterministic control over "the
        // channel is full" instead of racing a real writer task's
        // processing speed.
        let (tx, _rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(1);
        let handle = WriterTaskHandle { tx };

        // First send fills the sole channel slot. Its reply never arrives
        // since nothing drains `_rx`, so run it in the background.
        let first = tokio::spawn({
            let handle = handle.clone();
            async move {
                let _ = handle.send(|_conn| Ok::<(), StorageError>(())).await;
            }
        });

        // Give the first send a moment to occupy the channel slot.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Second send must block (backpressure), not fail immediately: a
        // short timeout should elapse rather than resolve.
        let second = tokio::time::timeout(
            Duration::from_millis(100),
            handle.send(|_conn| Ok::<(), StorageError>(())),
        )
        .await;

        assert!(
            second.is_err(),
            "a full channel must apply backpressure (send suspends) rather \
             than erroring immediately — no try_send escape hatch per ADR-067"
        );

        first.abort();
    }

    #[tokio::test]
    async fn send_with_timeout_maps_full_channel_to_write_queue_full() {
        let (tx, _rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(1);
        let handle = WriterTaskHandle { tx };

        let first = tokio::spawn({
            let handle = handle.clone();
            async move {
                let _ = handle.send(|_conn| Ok::<(), StorageError>(())).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let result = handle
            .send_with_timeout(
                |_conn| Ok::<(), StorageError>(()),
                Duration::from_millis(50),
            )
            .await;

        match result {
            Err(StorageError::WriteQueueFull { timeout_ms }) => assert_eq!(timeout_ms, 50),
            other => panic!("expected WriteQueueFull, got {other:?}"),
        }

        first.abort();
    }

    // `#[serial(tx_registry)]`: this test deliberately keeps a request (and
    // thus its `writer_task_tx` registry handle) alive past a timeout, so it is
    // the worst polluter of the checkpoint `tx_age_sweep_*` reads if left
    // un-serialized. Shares the key — see the note on
    // `begin_immediate_failure_replies_error_without_running_op`.
    #[tokio::test]
    #[serial(tx_registry)]
    async fn send_with_timeout_returns_op_result_when_op_outlives_the_timeout() {
        // `send_with_timeout`'s timeout must bound ONLY the enqueue step —
        // never the reply-wait. An accepted request (channel not full) must
        // run to completion and report its REAL result even when that takes
        // longer than `timeout`; before this fix, wrapping the whole
        // send-plus-reply-wait in one timeout would misreport this as
        // `WriteQueueFull` despite the write actually landing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_slow_op.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
                .unwrap();
        }

        let handle = spawn(&pool, 8).expect("writer task should spawn on a file-backed pool");

        let result = handle
            .send_with_timeout(
                |conn| {
                    // Deliberately slower than the timeout below: proves the
                    // reply-wait itself is never bounded by `timeout`.
                    std::thread::sleep(Duration::from_millis(150));
                    conn.execute("INSERT INTO t (id, v) VALUES (1, 'slow')", [])
                        .map_err(|e| StorageError::Pool {
                            operation: "test_insert".into(),
                            message: e.to_string(),
                        })
                },
                Duration::from_millis(20),
            )
            .await;

        let affected = result.expect(
            "an accepted request must return its real result even when the \
             op takes longer than the enqueue timeout, not WriteQueueFull",
        );
        assert_eq!(affected, 1);

        // The slow op's write must have actually committed, not just been
        // reported as successful.
        let reader = pool.reader().expect("reader");
        let v: String = reader
            .conn()
            .query_row("SELECT v FROM t WHERE id = 1", [], |row| row.get(0))
            .expect("the slow op's write must have committed");
        assert_eq!(v, "slow");
    }

    #[tokio::test]
    async fn dropped_receiver_maps_send_to_internal_error() {
        // Simulates the writer task having stopped/panicked: its `rx` is
        // gone, so `tx.send()` must fail rather than hang.
        let (tx, rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(4);
        drop(rx);

        let handle = WriterTaskHandle { tx };
        let result = handle.send(|_conn| Ok::<(), StorageError>(())).await;

        match result {
            Err(StorageError::Internal(_)) => {}
            other => panic!("expected Internal error on a closed channel, got {other:?}"),
        }
    }
}
