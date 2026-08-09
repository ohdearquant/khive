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
//!
//! ## ADR-136 D1 gate 5: writer classification
//!
//! Every connection that issues writes against a khive-db file falls into
//! exactly one of the rows below. `SqlAccess`-reachable is the dividing
//! line: only requests reachable through `khive_storage::SqlAccess`
//! (`SqlBridge::writer`/`atomic_unit`, and the `stores::*` methods built on
//! them) are subject to `PoolConfig::write_queue_enabled` /
//! `write_routing_strict` routing at all. Everything else here is an
//! explicit, intentional exemption — not an implicit gap left over from an
//! incomplete migration.
//!
//! | Writer | Connection | `SqlAccess`-reachable | Routing |
//! | --- | --- | --- | --- |
//! | Request-path writes (`create`, `update`, batch upserts, …) | `WriterTaskHandle` (queue-first) or a standalone/pool-mutex connection on degrade | Yes | Queue-first (ADR-136 D1 gate 1); strict routing fails closed on degrade |
//! | Startup / schema migrations (`migrations::run_migrations`, `apply_schema_plan`) | The pool's own writer-mutex connection (`ConnectionPool::new`, before the `WriterTask` is ever spawned) | No | Exempt — runs once at boot, before any queue handle exists to route through |
//! | Checkpointing (`checkpoint::CheckpointConnection`) | Dedicated standalone connection, opened once at task startup (ADR-091 Amendment 5, #1652) | No | Exempt by design — the whole point of that fix was moving checkpoint I/O OFF the pool writer's admission path; routing it back through the write queue would reintroduce the contention it removed |
//! | Recovery (`walpin` beacon/sidecar bookkeeping) | None — file-level bookkeeping (PID beacons, heartbeats) alongside the database, not a SQL connection against it | No | N/A — never acquires a database writer connection |
//! | Top-level maintenance (`VACUUM` via `execute_script_top_level`/`WriterTaskHandle::send_top_level`) | `WriterTaskHandle`, skipping the per-request `BEGIN IMMEDIATE`/`COMMIT` wrap only | Yes | Routed through the SAME single writer owner as every other queued request — only the transaction wrap is skipped, never the queue |
//!
//! A `direct_route_violation` sink row (`timeout_sink::emit_direct_route_violation`,
//! ADR-136 D1 gate 6c) is emitted ONLY for the first row's degrade path — a
//! `SqlAccess`-reachable write that bypassed an enabled queue. The other four
//! rows are exempt by design and never emit that row.

use rusqlite::Connection;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

use khive_storage::error::{StorageError, WriterTaskRequestState};

use crate::error::SqliteError;
use crate::pool::{ConnectionPool, WriterAcquisitionCounters};

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

/// Latest completed writer-task span for one database, decomposed at the
/// boundaries an operator can act on (#1849). A zero value means that phase
/// did not run (top-level/early-failure request) or completed below the
/// microsecond clock resolution; `total_micros` retains the full span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterStageObservation {
    pub queue_wait_micros: u64,
    pub transaction_acquire_micros: u64,
    pub body_micros: u64,
    pub commit_micros: u64,
    pub total_micros: u64,
    pub queue_depth_at_entry: u64,
    pub observed_at_unix_ms: u64,
}

static WRITER_STAGE_OBSERVATIONS: OnceLock<Mutex<HashMap<String, WriterStageObservation>>> =
    OnceLock::new();

fn writer_stage_observations() -> &'static Mutex<HashMap<String, WriterStageObservation>> {
    WRITER_STAGE_OBSERVATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn observed_at_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure in-memory read of the most recently completed writer-task span for
/// this exact backend. It acquires no SQLite connection and performs no I/O.
pub fn last_writer_stage_observation(pool: &ConnectionPool) -> Option<WriterStageObservation> {
    let db = crate::timeout_sink::db_label(pool);
    writer_stage_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&db)
        .cloned()
}

struct WriteTelemetry {
    db: String,
    submitted_at: Instant,
    queue_depth_at_entry: usize,
    slow_write_threshold: Option<Duration>,
}

impl WriteTelemetry {
    fn new(
        db: String,
        queue_depth_at_entry: usize,
        slow_write_threshold: Option<Duration>,
    ) -> Self {
        Self {
            db,
            submitted_at: Instant::now(),
            queue_depth_at_entry,
            slow_write_threshold,
        }
    }

    fn queue_wait(&self) -> Duration {
        self.submitted_at.elapsed()
    }

    fn finish(
        self,
        queue_wait: Duration,
        transaction_acquire: Duration,
        body: Duration,
        commit: Duration,
    ) {
        let total = self.submitted_at.elapsed();
        let observation = WriterStageObservation {
            queue_wait_micros: duration_micros(queue_wait),
            transaction_acquire_micros: duration_micros(transaction_acquire),
            body_micros: duration_micros(body),
            commit_micros: duration_micros(commit),
            total_micros: duration_micros(total),
            queue_depth_at_entry: self.queue_depth_at_entry as u64,
            observed_at_unix_ms: observed_at_unix_ms(),
        };
        writer_stage_observations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.db.clone(), observation.clone());

        if self
            .slow_write_threshold
            .is_some_and(|threshold| total >= threshold)
        {
            crate::timeout_sink::emit_slow_write(&self.db, &observation);
        }
    }
}

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
    telemetry: WriteTelemetry,
}

mod sealed {
    /// Restricts [`super::AnyWriteRequest`] to implementations defined in
    /// this module — only [`super::WriteRequest<R>`] implements it — and
    /// carries the drain loop's internal terminal-state reporting methods
    /// without changing the public execution-method signatures.
    pub trait Sealed {
        fn execute_and_reply_reporting_terminal(
            self: Box<Self>,
            conn: &rusqlite::Connection,
            tx_span: Option<khive_storage::tx_registry::TxHandle>,
            queue_wait: std::time::Duration,
            transaction_acquire: std::time::Duration,
        ) -> Option<khive_storage::error::WriterTaskRequestState>;

        fn execute_and_reply_top_level_reporting_terminal(
            self: Box<Self>,
            conn: &rusqlite::Connection,
            queue_wait: std::time::Duration,
        ) -> Option<khive_storage::error::WriterTaskRequestState>;

        fn reply_error_after_begin(
            self: Box<Self>,
            err: khive_storage::error::StorageError,
            queue_wait: std::time::Duration,
            transaction_acquire: std::time::Duration,
        );
    }
}

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

    /// Time from the caller constructing this request (before bounded-channel
    /// admission) until the writer task dequeued it.
    fn queue_wait(&self) -> Duration;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollbackDisposition {
    RolledBack,
    SideEffectsUnknown,
}

/// Roll back a failed wrapped request and verify that the connection really
/// returned to autocommit mode before it can serve another request.
fn rollback_after_failure(conn: &Connection, failure_context: &'static str) -> RollbackDisposition {
    match conn.execute_batch("ROLLBACK") {
        Ok(()) if conn.is_autocommit() => RollbackDisposition::RolledBack,
        Ok(()) => {
            tracing::error!(
                failure_context,
                "writer transaction: ROLLBACK returned success but the connection is still in a \
                 transaction; request side effects are unknown"
            );
            RollbackDisposition::SideEffectsUnknown
        }
        Err(rollback_error) => {
            tracing::error!(
                error = %rollback_error,
                failure_context,
                "writer transaction: rollback after request failure failed; request side effects are \
                 unknown"
            );
            RollbackDisposition::SideEffectsUnknown
        }
    }
}

/// Execute an operation inside an already-open transaction and apply the
/// shared commit, rollback, panic, and autocommit verification rules.
pub(crate) fn execute_wrapped_transaction<R, F>(
    conn: &Connection,
    commit_operation: &'static str,
    operation: F,
) -> (Result<R, StorageError>, Option<WriterTaskRequestState>)
where
    F: FnOnce(&Connection) -> Result<R, StorageError>,
{
    let profiled = execute_wrapped_transaction_profiled(conn, commit_operation, operation);
    (profiled.result, profiled.terminal_state)
}

struct ProfiledWrappedTransaction<R> {
    result: Result<R, StorageError>,
    terminal_state: Option<WriterTaskRequestState>,
    body: Duration,
    commit: Duration,
}

fn execute_wrapped_transaction_profiled<R, F>(
    conn: &Connection,
    commit_operation: &'static str,
    operation: F,
) -> ProfiledWrappedTransaction<R>
where
    F: FnOnce(&Connection) -> Result<R, StorageError>,
{
    let body_started = Instant::now();
    let operation_outcome = catch_unwind(AssertUnwindSafe(|| operation(conn)));
    let body = body_started.elapsed();

    match operation_outcome {
        Ok(Ok(value)) => {
            let commit_started = Instant::now();
            let commit_outcome = conn.execute_batch("COMMIT");
            let commit = commit_started.elapsed();
            match commit_outcome {
                Ok(()) if conn.is_autocommit() => ProfiledWrappedTransaction {
                    result: Ok(value),
                    terminal_state: None,
                    body,
                    commit,
                },
                Ok(()) => {
                    tracing::error!(
                    "writer transaction: COMMIT returned success but the connection is still in \
                     a transaction; request side effects are unknown"
                );
                    let request_state = WriterTaskRequestState::SideEffectsUnknown;
                    ProfiledWrappedTransaction {
                        result: Err(writer_task_terminated(request_state)),
                        terminal_state: Some(request_state),
                        body,
                        commit,
                    }
                }
                Err(commit_error) => match rollback_after_failure(conn, "commit failure") {
                    RollbackDisposition::RolledBack => ProfiledWrappedTransaction {
                        result: Err(StorageError::Pool {
                            operation: commit_operation.into(),
                            message: commit_error.to_string(),
                        }),
                        terminal_state: None,
                        body,
                        commit,
                    },
                    RollbackDisposition::SideEffectsUnknown => {
                        let request_state = WriterTaskRequestState::SideEffectsUnknown;
                        ProfiledWrappedTransaction {
                            result: Err(writer_task_terminated(request_state)),
                            terminal_state: Some(request_state),
                            body,
                            commit,
                        }
                    }
                },
            }
        }
        Ok(Err(operation_error)) => {
            match rollback_after_failure(conn, "request operation failure") {
                RollbackDisposition::RolledBack => ProfiledWrappedTransaction {
                    result: Err(operation_error),
                    terminal_state: None,
                    body,
                    commit: Duration::ZERO,
                },
                RollbackDisposition::SideEffectsUnknown => {
                    let request_state = WriterTaskRequestState::SideEffectsUnknown;
                    ProfiledWrappedTransaction {
                        result: Err(writer_task_terminated(request_state)),
                        terminal_state: Some(request_state),
                        body,
                        commit: Duration::ZERO,
                    }
                }
            }
        }
        Err(_panic_payload) => {
            let request_state = match rollback_after_failure(conn, "request panic") {
                RollbackDisposition::RolledBack => WriterTaskRequestState::TransactionRolledBack,
                RollbackDisposition::SideEffectsUnknown => {
                    WriterTaskRequestState::SideEffectsUnknown
                }
            };
            ProfiledWrappedTransaction {
                result: Err(writer_task_terminated(request_state)),
                terminal_state: Some(request_state),
                body,
                commit: Duration::ZERO,
            }
        }
    }
}

impl<R: Send + 'static> sealed::Sealed for WriteRequest<R> {
    fn execute_and_reply_reporting_terminal(
        self: Box<Self>,
        conn: &Connection,
        tx_span: Option<khive_storage::tx_registry::TxHandle>,
        queue_wait: Duration,
        transaction_acquire: Duration,
    ) -> Option<WriterTaskRequestState> {
        // Keep the typed reply sender outside the unwind boundary. Calling
        // `(self.op)(conn)` directly would drop `self.reply` while unwinding,
        // leaving the active caller with only an untyped RecvError.
        let WriteRequest {
            op,
            reply,
            telemetry,
            ..
        } = *self;
        let profiled = execute_wrapped_transaction_profiled(conn, "writer_task_commit", op);
        // The reply wake can make the caller immediately observe the
        // committed/error result. Deregister the transaction span first so
        // tx_registry continues to mean "currently open SQL transaction",
        // never "a transaction whose caller has already resumed" (#1790).
        drop(tx_span);
        telemetry.finish(
            queue_wait,
            transaction_acquire,
            profiled.body,
            profiled.commit,
        );
        // The receiver may already be gone (caller dropped its future) —
        // that is not this task's problem to report.
        let _ = reply.send(profiled.result);
        profiled.terminal_state
    }

    fn execute_and_reply_top_level_reporting_terminal(
        self: Box<Self>,
        conn: &Connection,
        queue_wait: Duration,
    ) -> Option<WriterTaskRequestState> {
        let WriteRequest {
            op,
            reply,
            telemetry,
            ..
        } = *self;
        let body_started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| op(conn)));
        let body = body_started.elapsed();
        telemetry.finish(queue_wait, Duration::ZERO, body, Duration::ZERO);
        match outcome {
            Ok(outcome) if conn.is_autocommit() => {
                // No COMMIT/ROLLBACK here: this request explicitly did not
                // open a transaction, so there is nothing to close.
                let _ = reply.send(outcome);
                None
            }
            Ok(_outcome) => {
                tracing::error!(
                    "writer task: top-level request returned with an open transaction; request \
                     side effects are unknown"
                );
                let request_state = WriterTaskRequestState::SideEffectsUnknown;
                let _ = reply.send(Err(writer_task_terminated(request_state)));
                Some(request_state)
            }
            Err(_panic_payload) => {
                // Statements completed before a top-level panic may already
                // have autocommitted. Report the ambiguity; never invent a
                // rollback for a request that opened no transaction.
                let request_state = WriterTaskRequestState::SideEffectsUnknown;
                let _ = reply.send(Err(writer_task_terminated(request_state)));
                Some(request_state)
            }
        }
    }

    fn reply_error_after_begin(
        self: Box<Self>,
        err: StorageError,
        queue_wait: Duration,
        transaction_acquire: Duration,
    ) {
        let WriteRequest {
            reply, telemetry, ..
        } = *self;
        telemetry.finish(
            queue_wait,
            transaction_acquire,
            Duration::ZERO,
            Duration::ZERO,
        );
        let _ = reply.send(Err(err));
    }
}

impl<R: Send + 'static> AnyWriteRequest for WriteRequest<R> {
    fn execute_and_reply(self: Box<Self>, conn: &Connection) {
        let queue_wait = self.queue_wait();
        let _ = sealed::Sealed::execute_and_reply_reporting_terminal(
            self,
            conn,
            None,
            queue_wait,
            Duration::ZERO,
        );
    }

    fn execute_and_reply_top_level(self: Box<Self>, conn: &Connection) {
        let queue_wait = self.queue_wait();
        let _ =
            sealed::Sealed::execute_and_reply_top_level_reporting_terminal(self, conn, queue_wait);
    }

    fn reply_error(self: Box<Self>, err: StorageError) {
        let queue_wait = self.queue_wait();
        sealed::Sealed::reply_error_after_begin(self, err, queue_wait, Duration::ZERO);
    }

    fn is_top_level(&self) -> bool {
        self.top_level
    }

    fn queue_wait(&self) -> Duration {
        self.telemetry.queue_wait()
    }
}

fn writer_task_terminated(request_state: WriterTaskRequestState) -> StorageError {
    StorageError::WriterTaskTerminated { request_state }
}

/// Sender half of the write queue. Cheaply cloneable (wraps an
/// `mpsc::Sender`) — every migrated store that shares one writer task holds
/// a clone of this handle.
#[derive(Clone, Debug)]
pub struct WriterTaskHandle {
    tx: mpsc::Sender<Box<dyn AnyWriteRequest + Send>>,
    /// This handle's pool's writer-timeout sink identity (`timeout_sink::db_label`),
    /// captured at spawn so `send_with_timeout`'s `WriteQueueFull` path can
    /// report a `queue_saturation` sink row (ADR-136 D1 gate 6a) without
    /// needing a `&ConnectionPool` reference.
    db: String,
    /// Slow-write latency bound (`timeout_sink::slow_write_threshold`),
    /// resolved once at spawn. `None` disables slow-write rows. Captured
    /// here rather than read per send so the caller path pays no env lookup
    /// and tests get a deterministic value by setting the override before
    /// spawning their own writer task.
    slow_write_threshold: Option<std::time::Duration>,
    /// Default enqueue-capacity deadline for [`Self::send_bounded`] /
    /// [`Self::send_top_level_bounded`], captured from
    /// `PoolConfig::write_admission_deadline_ms` at [`spawn`] (ADR-131
    /// Decision 2). Bounds ONLY the wait for channel capacity, the same
    /// boundary `send_with_timeout` already documents — never the reply wait
    /// after acceptance (#1382).
    ///
    /// This is a dedicated admission authority, distinct from
    /// `PoolConfig::checkout_timeout` (reader/pool checkout). The two used
    /// to be conflated here before ADR-131 Decision 2 (#1382, #1643).
    ///
    /// ADR-131 Decision 2's outer-request-budget clamp ("when the caller's outer
    /// request deadline leaves less time remaining than the configured
    /// admission deadline, the admission deadline applied to that operation
    /// is the remaining outer budget instead") is explicitly DEFERRED: no
    /// outer request deadline is plumbed to `send_bounded` /
    /// `send_top_level_bounded`'s call sites as of #1737, so there is no
    /// budget to clamp against. When an outer deadline reaches this call
    /// path, clamp here rather than faking a shorter deadline against a
    /// nonexistent budget.
    enqueue_timeout: std::time::Duration,
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
    /// will either run to completion or receive a typed terminal error if an
    /// earlier request kills the task before this operation begins. The
    /// returned receiver must be awaited without a timeout; abandoning it
    /// here would silently drop the request's eventual result, not cancel the
    /// request itself.
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
        let telemetry = WriteTelemetry::new(
            self.db.clone(),
            self.queue_depth(),
            self.slow_write_threshold,
        );
        let request = WriteRequest {
            op: Box::new(op),
            reply: reply_tx,
            top_level,
            telemetry,
        };

        self.tx
            .send(Box::new(request))
            .await
            .map_err(|_| writer_task_terminated(WriterTaskRequestState::NotStarted))?;

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
        reply_rx
            .await
            .map_err(|_| writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown))?
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
                let timeout_ms = timeout.as_millis() as u64;
                crate::timeout_sink::emit_queue_saturation(&self.db, timeout_ms);
                return Err(StorageError::WriteQueueFull { timeout_ms });
            }
        };

        reply_rx
            .await
            .map_err(|_| writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown))?
    }

    /// Like [`Self::send`], but bounds the enqueue-capacity wait with this
    /// handle's configured `enqueue_timeout`
    /// (`PoolConfig::write_admission_deadline_ms` at spawn time, ADR-131
    /// Decision 2) instead of waiting indefinitely (#1382).
    ///
    /// Once the request is accepted onto the channel, the reply wait is
    /// unbounded by this method, identical to [`Self::send`] — this bounds
    /// only queue-capacity admission, never SQLite execution or reply
    /// latency. Callers that need a non-default deadline should use
    /// [`Self::send_with_timeout`] directly.
    pub async fn send_bounded<R, F>(&self, op: F) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        self.send_with_timeout(op, self.enqueue_timeout).await
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
        reply_rx
            .await
            .map_err(|_| writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown))?
    }

    /// Like [`Self::send_top_level`], but bounds the enqueue-capacity wait
    /// with this handle's configured `enqueue_timeout`, mirroring
    /// [`Self::send_bounded`] for top-level (transaction-skipping) requests
    /// (#1382).
    pub async fn send_top_level_bounded<R, F>(&self, op: F) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let reply_rx =
            match tokio::time::timeout(self.enqueue_timeout, self.enqueue_inner(op, true)).await {
                Ok(Ok(reply_rx)) => reply_rx,
                Ok(Err(e)) => return Err(e),
                Err(_elapsed) => {
                    let timeout_ms = self.enqueue_timeout.as_millis() as u64;
                    crate::timeout_sink::emit_queue_saturation(&self.db, timeout_ms);
                    return Err(StorageError::WriteQueueFull { timeout_ms });
                }
            };

        reply_rx
            .await
            .map_err(|_| writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown))?
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
/// Opens a dedicated standalone writer connection independent of the pool's
/// Mutex-guarded `writer()` connection used by unmigrated paths. That one-time
/// infrastructure open is uncounted; every dequeued top-level request or
/// successful `BEGIN IMMEDIATE` increments the writer-task acquisition class.
/// Returns the cloneable [`WriterTaskHandle`] sender half. The task normally
/// runs until every handle clone is dropped and the channel closes; a request
/// panic, failed rollback, or poisoned connection puts it into the permanent
/// terminal state documented below.
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
    // The lifetime connection is infrastructure, not one acquisition per
    // write. Each dequeued request is counted below at the task's actual
    // ownership boundary instead.
    let conn = pool.open_standalone_writer_untracked()?;
    let acquisition_counters = pool.writer_acquisition_counters();
    let origin = pool.origin();
    let db = crate::timeout_sink::db_label(pool);
    let (tx, rx) = mpsc::channel(capacity.max(1));
    let join = tokio::spawn(run_writer_task(
        conn,
        rx,
        origin,
        db.clone(),
        acquisition_counters,
    ));
    // Stored on the pool (not returned) so the handle's clone-and-share
    // contract stays untouched; see ConnectionPool::take_writer_task_join
    // for who awaits it and why.
    pool.set_writer_task_join(join);
    Ok(WriterTaskHandle {
        tx,
        db,
        slow_write_threshold: crate::timeout_sink::slow_write_threshold(),
        enqueue_timeout: std::time::Duration::from_millis(
            pool.config().write_admission_deadline_ms,
        ),
    })
}

/// Permanently close admission, then reply to every request that was already
/// accepted into the bounded channel without invoking its operation closure.
///
/// Closing before draining is load-bearing: draining an open receiver would
/// wait forever while [`WriterTaskHandle`] clones still exist, while dropping
/// the receiver immediately would discard buffered requests and their typed
/// reply senders.
async fn close_and_fail_queued_requests(rx: &mut mpsc::Receiver<Box<dyn AnyWriteRequest + Send>>) {
    rx.close();
    while let Some(request) = rx.recv().await {
        request.reply_error(writer_task_terminated(WriterTaskRequestState::NotStarted));
    }
}

/// Drain loop: the sole caller of `BEGIN IMMEDIATE` for write traffic routed
/// through the channel. A `BEGIN IMMEDIATE` failure replies the request's
/// error via [`AnyWriteRequest::reply_error`] without invoking the
/// request's closure; no retry — the connection tries fresh next request.
///
/// A request-operation panic is contained inside the concrete
/// [`WriteRequest<R>`] so its typed reply survives. A panic, failed rollback,
/// or otherwise poisoned connection makes the task terminal. The task then
/// closes admission, explicitly fails every already-queued request as
/// [`WriterTaskRequestState::NotStarted`], and exits permanently. There is no
/// supervisor or connection restart; later sends observe the closed channel.
/// See
/// `crates/khive-db/docs/api/writer-task.md` for the full failure matrix.
async fn run_writer_task(
    mut conn: Connection,
    mut rx: mpsc::Receiver<Box<dyn AnyWriteRequest + Send>>,
    origin: khive_storage::tx_registry::TxOrigin,
    db: String,
    acquisition_counters: Arc<WriterAcquisitionCounters>,
) {
    while let Some(request) = rx.recv().await {
        // The bounded-channel wait ends at this exact dequeue boundary.
        // Sampling inside the blocking closure would misattribute a saturated
        // Tokio blocking pool to writer-queue contention (#1849).
        let queue_wait = request.queue_wait();
        let origin = origin.clone();
        let acquisition_counters = Arc::clone(&acquisition_counters);
        let outcome = tokio::task::spawn_blocking(move || {
            // A top-level request deliberately skips BEGIN, so it would
            // silently join any transaction leaked by an earlier request.
            // Refuse every request before dispatch if the connection is not
            // demonstrably clean, then retire the writer task.
            if !conn.is_autocommit() {
                tracing::error!(
                    "writer task: connection is not in autocommit mode before request dispatch; \
                     retiring the poisoned writer without running the request"
                );
                let request_state = WriterTaskRequestState::NotStarted;
                request.reply_error(writer_task_terminated(request_state));
                return (conn, Some(request_state));
            }

            let terminal_state = if request.is_top_level() {
                // ADR-067 Component A:
                // no BEGIN IMMEDIATE for this request — some statements
                // (e.g. VACUUM) are rejected by SQLite inside any open
                // transaction. Still runs on this task's dedicated
                // connection and still serialized one-request-at-a-time by
                // this same drain loop, so the single-writer guarantee
                // holds; only the transaction wrap is skipped.
                acquisition_counters.record_writer_task_acquisition();
                sealed::Sealed::execute_and_reply_top_level_reporting_terminal(
                    request, &conn, queue_wait,
                )
            } else {
                let tx_span = khive_storage::tx_registry::register_scoped(
                    Some("writer_task_tx".to_string()),
                    origin,
                );
                let transaction_acquire_started = Instant::now();
                let begin_outcome = conn.execute_batch("BEGIN IMMEDIATE");
                let transaction_acquire = transaction_acquire_started.elapsed();
                match begin_outcome {
                    Ok(()) => {
                        acquisition_counters.record_writer_task_acquisition();
                        sealed::Sealed::execute_and_reply_reporting_terminal(
                            request,
                            &conn,
                            Some(tx_span),
                            queue_wait,
                            transaction_acquire,
                        )
                    }
                    Err(e) => {
                        // Do NOT run the request's operation: `conn` never
                        // entered a transaction, so executing the op's DML
                        // here would run in autocommit mode and land partial
                        // writes for a request the caller is about to be told
                        // failed.
                        tracing::warn!(
                            error = %e,
                            "writer task: BEGIN IMMEDIATE failed; replying an \
                             error without running the request's operation"
                        );
                        // Although BEGIN never opened a SQL transaction, the
                        // scoped attempt is observable in tx_registry. Drop
                        // it before waking the caller for the same reply-side
                        // lifecycle guarantee as successful requests (#1790).
                        drop(tx_span);
                        sealed::Sealed::reply_error_after_begin(
                            request,
                            StorageError::Pool {
                                operation: "writer_task_begin".into(),
                                message: e.to_string(),
                            },
                            queue_wait,
                            transaction_acquire,
                        );
                        None
                    }
                }
            };
            (conn, terminal_state)
        })
        .await;

        match outcome {
            Ok((returned_conn, None)) => conn = returned_conn,
            Ok((_returned_conn, Some(request_state))) => {
                tracing::error!(
                    request_state = %request_state,
                    "writer task reached a terminal request or connection state; closing and \
                     failing the queue without restarting"
                );
                crate::timeout_sink::emit_writer_task_retirement(
                    &db,
                    &format!("terminal request state: {request_state}"),
                );
                close_and_fail_queued_requests(&mut rx).await;
                return;
            }
            Err(join_err) => {
                tracing::error!(
                    error = %join_err,
                    "writer task blocking closure failed outside the request \
                     panic boundary; closing and failing the queue without restarting"
                );
                crate::timeout_sink::emit_writer_task_retirement(
                    &db,
                    &format!("blocking closure join failure: {join_err}"),
                );
                close_and_fail_queued_requests(&mut rx).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    use serial_test::serial;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc as std_mpsc;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::Duration;

    fn file_pool(path: &std::path::Path) -> ConnectionPool {
        let cfg = PoolConfig {
            path: Some(path.to_path_buf()),
            ..PoolConfig::default()
        };
        ConnectionPool::new(cfg).expect("pool open")
    }

    fn deny_commit_and_rollback(ctx: AuthContext<'_>) -> Authorization {
        match ctx.action {
            // SQLite reports COMMIT as the non-exhaustive Unknown transaction
            // operation in rusqlite 0.40; ROLLBACK has its own variant.
            AuthAction::Transaction {
                operation: TransactionOperation::Unknown | TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }

    fn deny_commit(ctx: AuthContext<'_>) -> Authorization {
        match ctx.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Unknown,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }

    fn deny_rollback(ctx: AuthContext<'_>) -> Authorization {
        match ctx.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }

    fn assert_writer_task_terminal_state<T: std::fmt::Debug>(
        result: Result<T, StorageError>,
        expected: WriterTaskRequestState,
    ) {
        match result {
            Err(StorageError::WriterTaskTerminated { request_state }) => {
                assert_eq!(request_state, expected)
            }
            other => panic!("expected WriterTaskTerminated({expected:?}), got {other:?}"),
        }
    }

    struct ParkedWake {
        entered: std_mpsc::SyncSender<()>,
        release: Mutex<std_mpsc::Receiver<()>>,
    }

    impl Wake for ParkedWake {
        fn wake(self: Arc<Self>) {
            self.entered
                .send(())
                .expect("reply sender must rendezvous with the test");
            self.release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("test must release the parked reply sender");
        }
    }

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn arm_parked_wake<F: Future>(
        mut future: Pin<&mut F>,
    ) -> (std_mpsc::Receiver<()>, std_mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = std_mpsc::sync_channel(0);
        let (release_tx, release_rx) = std_mpsc::channel();
        let waker = Waker::from(Arc::new(ParkedWake {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }));
        let mut context = Context::from_waker(&waker);
        assert!(
            matches!(future.as_mut().poll(&mut context), Poll::Pending),
            "writer send must remain pending until its operation replies"
        );
        (entered_rx, release_tx)
    }

    fn poll_ready<F: Future>(mut future: Pin<&mut F>) -> F::Output {
        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("reply wake must make the writer send ready"),
        }
    }

    fn database_tx_view(pool: &ConnectionPool) -> khive_storage::tx_registry::TxOriginFilter {
        match pool.origin() {
            khive_storage::tx_registry::TxOrigin::Database(identity) => {
                khive_storage::tx_registry::TxOriginFilter::Secondary(identity)
            }
            other => panic!("expected a file-backed database origin, got {other:?}"),
        }
    }

    async fn wait_for_writer_span_to_close(view: &khive_storage::tx_registry::TxOriginFilter) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while khive_storage::tx_registry::any_open_labeled(view, "writer_task_tx") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer task transaction span must eventually close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(tx_registry)]
    async fn successful_send_reply_waits_for_writer_tx_deregistration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_success_reply_lifecycle.db");
        let pool = file_pool(&path);
        let view = database_tx_view(&pool);
        let handle = spawn(&pool, 8).expect("writer task spawn");
        let (op_started_tx, op_started_rx) = std_mpsc::sync_channel(0);
        let (op_release_tx, op_release_rx) = std_mpsc::channel();

        let send = handle.send(move |_conn| {
            op_started_tx
                .send(())
                .expect("operation must rendezvous with the test");
            op_release_rx
                .recv()
                .expect("test must release the operation");
            Ok::<_, StorageError>(())
        });
        tokio::pin!(send);
        let (reply_entered_rx, reply_release_tx) = arm_parked_wake(send.as_mut());

        op_started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer operation must start");
        op_release_tx.send(()).expect("release writer operation");
        reply_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reply sender must wake the waiting caller");

        let reply = poll_ready(send.as_mut());
        let span_was_open_at_reply =
            khive_storage::tx_registry::any_open_labeled(&view, "writer_task_tx");

        reply_release_tx
            .send(())
            .expect("release parked reply sender");
        wait_for_writer_span_to_close(&view).await;

        reply.expect("committed operation reply");
        assert!(
            !span_was_open_at_reply,
            "a successful caller reply must not become observable while its committed writer_task_tx span remains registered"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(tx_registry)]
    async fn begin_failure_reply_waits_for_writer_tx_deregistration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_begin_reply_lifecycle.db");
        let cfg = PoolConfig {
            path: Some(path),
            busy_timeout: Duration::from_millis(150),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();
        let view = database_tx_view(&pool);
        let handle = spawn(&pool, 8).expect("writer task spawn");
        let lock_holder = pool.try_writer().expect("pool writer");
        lock_holder
            .conn()
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold database write lock");
        let op_ran = Arc::new(AtomicBool::new(false));
        let op_ran_in_request = Arc::clone(&op_ran);

        let send = handle.send(move |_conn| {
            op_ran_in_request.store(true, Ordering::SeqCst);
            Ok::<_, StorageError>(())
        });
        tokio::pin!(send);
        let (reply_entered_rx, reply_release_tx) = arm_parked_wake(send.as_mut());
        reply_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("BEGIN failure must wake the waiting caller");

        let reply = poll_ready(send.as_mut());
        let span_was_open_at_reply =
            khive_storage::tx_registry::any_open_labeled(&view, "writer_task_tx");

        reply_release_tx
            .send(())
            .expect("release parked reply sender");
        wait_for_writer_span_to_close(&view).await;
        lock_holder
            .conn()
            .execute_batch("ROLLBACK")
            .expect("release database write lock");

        assert!(
            matches!(
                &reply,
                Err(StorageError::Pool { operation, .. }) if operation == "writer_task_begin"
            ),
            "expected writer_task_begin failure, got {reply:?}"
        );
        assert!(!op_ran.load(Ordering::SeqCst));
        assert!(
            !span_was_open_at_reply,
            "a BEGIN-failure caller reply must not become observable while its writer_task_tx span remains registered"
        );
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

        let counters = pool.writer_acquisition_snapshot();
        assert_eq!(counters.acquisitions, 2);
        assert_eq!(counters.pooled_acquisitions, 1);
        assert_eq!(counters.standalone_acquisitions, 0);
        assert_eq!(counters.writer_task_acquisitions, 1);
        assert_eq!(counters.timeouts, 0);
    }

    #[test]
    fn spawn_fails_on_in_memory_pool() {
        // In-memory pools have no standalone-connection support
        // (the infrastructure-only standalone open) — `spawn` must surface
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
        let handle = WriterTaskHandle {
            tx,
            db: "test".to_string(),
            slow_write_threshold: None,
            enqueue_timeout: Duration::from_secs(5),
        };

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
        let handle = WriterTaskHandle {
            tx,
            db: "test".to_string(),
            slow_write_threshold: None,
            enqueue_timeout: Duration::from_secs(5),
        };

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

    #[tokio::test]
    async fn configured_enqueue_timeout_rejects_only_unaccepted_request() {
        // A real file-backed writer task: `send_bounded` reuses
        // `PoolConfig::write_admission_deadline_ms` (ADR-131 Decision 2) as
        // its enqueue deadline, captured at `spawn`, so this must exercise
        // the actual spawn path rather than a hand-built channel (#1382).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("configured_enqueue_timeout.db");
        let cfg = PoolConfig {
            path: Some(path.clone()),
            write_admission_deadline_ms: 100,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();
        let handle = spawn(&pool, 1).expect("writer task should spawn on a file-backed pool");

        // Request A: dequeued and running (inside `spawn_blocking`), blocked
        // on a test-controlled channel so the writer task's single drain
        // slot stays occupied deterministically — no sleeps.
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();
        let handle_a = handle.clone();
        let a_task = tokio::spawn(async move {
            handle_a
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    release_rx.recv().expect("test must release request A");
                    Ok::<(), StorageError>(())
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("request A did not start")
            .expect("request A dropped its start signal");

        // Request B: A has been dequeued (freeing the one channel slot), so
        // the private `enqueue` helper proves B is accepted and now occupies
        // that slot, without waiting for A to finish.
        let b_reply_rx = tokio::time::timeout(
            Duration::from_secs(5),
            handle.enqueue(|_conn| Ok::<(), StorageError>(())),
        )
        .await
        .expect("B must be accepted promptly")
        .expect("B must be accepted: the one channel slot is free while A drains");

        // Request C: the channel is now full (A draining, B queued behind
        // it) — `send_bounded` must reject C on the configured
        // `write_admission_deadline_ms` without ever running its closure.
        let c_ran = Arc::new(AtomicBool::new(false));
        let c_ran_in_op = Arc::clone(&c_ran);
        let c_result = handle
            .send_bounded(move |_conn| {
                c_ran_in_op.store(true, Ordering::SeqCst);
                Ok::<(), StorageError>(())
            })
            .await;
        match c_result {
            Err(StorageError::WriteQueueFull { .. }) => {}
            other => panic!("expected WriteQueueFull, got {other:?}"),
        }
        assert!(!c_ran.load(Ordering::SeqCst), "C must never run");

        // Release A; both A and B must then complete normally.
        release_tx.send(()).expect("release request A");
        tokio::time::timeout(Duration::from_secs(5), a_task)
            .await
            .expect("A did not complete")
            .expect("A task join")
            .expect("A must complete successfully");
        tokio::time::timeout(Duration::from_secs(5), b_reply_rx)
            .await
            .expect("B did not reply")
            .expect("B's reply channel must not be dropped")
            .expect("B must complete successfully");
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
    #[serial(tx_registry)]
    async fn operation_failure_with_successful_rollback_preserves_error_and_writer_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_operation_rollback.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
                .unwrap();
        }
        let handle = spawn(&pool, 8).expect("writer task spawn");

        let original_error = handle
            .send(|conn| -> Result<(), StorageError> {
                conn.execute("INSERT INTO t (id, v) VALUES (1, 'rolled-back')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_operation_error_insert".into(),
                        message: e.to_string(),
                    })?;
                Err(StorageError::Internal(
                    "intentional operation failure".into(),
                ))
            })
            .await;
        assert!(
            matches!(
                &original_error,
                Err(StorageError::Internal(message))
                    if message == "intentional operation failure"
            ),
            "a confirmed rollback must preserve the operation error, got {original_error:?}"
        );

        let affected = handle
            .send(|conn| {
                conn.execute("INSERT INTO t (id, v) VALUES (2, 'committed')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_operation_error_followup_insert".into(),
                        message: e.to_string(),
                    })
            })
            .await
            .expect("the writer must continue after a confirmed rollback");
        assert_eq!(affected, 1);

        let reader = pool.reader().expect("reader");
        let rolled_back: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        let committed: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id = 2", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rolled_back, 0);
        assert_eq!(committed, 1);
    }

    #[tokio::test]
    #[serial(tx_registry)]
    async fn commit_failure_with_successful_rollback_preserves_error_and_writer_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_commit_rollback.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
                .unwrap();
        }
        let handle = spawn(&pool, 8).expect("writer task spawn");

        let commit_error = handle
            .send(|conn| -> Result<usize, StorageError> {
                let affected = conn
                    .execute("INSERT INTO t (id, v) VALUES (1, 'rolled-back')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_commit_error_insert".into(),
                        message: e.to_string(),
                    })?;
                conn.authorizer(Some(deny_commit))
                    .map_err(|e| StorageError::Pool {
                        operation: "test_install_authorizer".into(),
                        message: e.to_string(),
                    })?;
                Ok(affected)
            })
            .await;
        assert!(
            matches!(
                &commit_error,
                Err(StorageError::Pool { operation, .. })
                    if operation == "writer_task_commit"
            ),
            "a confirmed rollback must preserve the commit error, got {commit_error:?}"
        );
        assert!(
            commit_error
                .as_ref()
                .expect_err("COMMIT must be denied")
                .is_retryable(),
            "the existing retryable commit-error contract must remain unchanged after a \
             confirmed rollback"
        );

        let affected = handle
            .send(|conn| {
                conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .map_err(|e| StorageError::Pool {
                        operation: "test_remove_authorizer".into(),
                        message: e.to_string(),
                    })?;
                conn.execute("INSERT INTO t (id, v) VALUES (2, 'committed')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_commit_error_followup_insert".into(),
                        message: e.to_string(),
                    })
            })
            .await
            .expect("the writer must continue after the failed COMMIT is rolled back");
        assert_eq!(affected, 1);

        let reader = pool.reader().expect("reader");
        let rolled_back: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        let committed: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id = 2", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rolled_back, 0);
        assert_eq!(committed, 1);
    }

    #[test]
    fn top_level_request_returning_with_open_transaction_reports_side_effects_unknown() {
        let conn = Connection::open_in_memory().expect("in-memory connection");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let request = WriteRequest {
            op: Box::new(|conn| -> Result<usize, StorageError> {
                conn.execute_batch("BEGIN IMMEDIATE")
                    .map_err(|e| StorageError::Pool {
                        operation: "test_top_level_begin".into(),
                        message: e.to_string(),
                    })?;
                conn.execute("INSERT INTO t (id) VALUES (1)", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_top_level_insert".into(),
                        message: e.to_string(),
                    })
            }),
            reply: reply_tx,
            top_level: true,
            telemetry: WriteTelemetry::new("test".to_string(), 0, None),
        };

        let terminal_state = sealed::Sealed::execute_and_reply_top_level_reporting_terminal(
            Box::new(request),
            &conn,
            Duration::ZERO,
        );
        assert_eq!(
            terminal_state,
            Some(WriterTaskRequestState::SideEffectsUnknown)
        );
        let reply = reply_rx
            .try_recv()
            .expect("active request must receive a typed terminal reply");
        assert_writer_task_terminal_state(reply, WriterTaskRequestState::SideEffectsUnknown);
        assert!(
            !conn.is_autocommit(),
            "the fixture must prove the post-request autocommit check observed an open transaction"
        );
    }

    #[test]
    fn commit_failure_with_failed_rollback_reports_side_effects_unknown() {
        let conn = Connection::open_in_memory().expect("in-memory connection");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE")
            .unwrap();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let request = WriteRequest {
            op: Box::new(|conn| -> Result<usize, StorageError> {
                let affected = conn
                    .execute("INSERT INTO t (id) VALUES (1)", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_insert_before_commit_failure".into(),
                        message: e.to_string(),
                    })?;
                conn.authorizer(Some(deny_commit_and_rollback))
                    .map_err(|e| StorageError::Pool {
                        operation: "test_install_authorizer".into(),
                        message: e.to_string(),
                    })?;
                Ok(affected)
            }),
            reply: reply_tx,
            top_level: false,
            telemetry: WriteTelemetry::new("test".to_string(), 0, None),
        };

        let terminal_state = sealed::Sealed::execute_and_reply_reporting_terminal(
            Box::new(request),
            &conn,
            None,
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(
            terminal_state,
            Some(WriterTaskRequestState::SideEffectsUnknown)
        );
        let reply = reply_rx
            .try_recv()
            .expect("active request must receive a typed terminal reply");
        assert_writer_task_terminal_state(reply, WriterTaskRequestState::SideEffectsUnknown);
        assert!(
            !conn.is_autocommit(),
            "the denied COMMIT and ROLLBACK must leave the test connection poisoned"
        );
    }

    #[tokio::test]
    #[serial(tx_registry)]
    async fn poisoned_connection_retires_before_queued_top_level_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_rollback_poison.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
                .unwrap();
        }
        let handle = spawn(&pool, 8).expect("writer task spawn");
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();

        let active = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .send(move |conn| -> Result<usize, StorageError> {
                        let affected = conn
                            .execute("INSERT INTO t (id, v) VALUES (1, 'active')", [])
                            .map_err(|e| StorageError::Pool {
                                operation: "test_active_insert".into(),
                                message: e.to_string(),
                            })?;
                        conn.authorizer(Some(deny_commit_and_rollback))
                            .map_err(|e| StorageError::Pool {
                                operation: "test_install_authorizer".into(),
                                message: e.to_string(),
                            })?;
                        let _ = started_tx.send(());
                        release_rx.recv().expect("test must release active op");
                        Ok(affected)
                    })
                    .await
            }
        });

        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("active request did not start")
            .expect("active request dropped its start signal");

        let queued_ran = Arc::new(AtomicBool::new(false));
        let queued_ran_in_op = Arc::clone(&queued_ran);
        let queued_top_level = handle
            .enqueue_inner(
                move |conn| {
                    queued_ran_in_op.store(true, Ordering::SeqCst);
                    conn.execute("INSERT INTO t (id, v) VALUES (2, 'queued')", [])
                        .map_err(|e| StorageError::Pool {
                            operation: "test_queued_top_level_insert".into(),
                            message: e.to_string(),
                        })
                },
                true,
            )
            .await
            .expect("top-level request must queue behind active request");
        release_tx.send(()).expect("release active op");

        let active_result = tokio::time::timeout(Duration::from_secs(5), active)
            .await
            .expect("active caller hung after rollback failure")
            .expect("active caller task join");
        assert_writer_task_terminal_state(
            active_result,
            WriterTaskRequestState::SideEffectsUnknown,
        );

        let queued_result = tokio::time::timeout(Duration::from_secs(5), queued_top_level)
            .await
            .expect("queued top-level caller hung after terminal failure")
            .expect("terminal drain must preserve queued typed reply");
        assert_writer_task_terminal_state(queued_result, WriterTaskRequestState::NotStarted);
        assert!(
            !queued_ran.load(Ordering::SeqCst),
            "a top-level request must never run on the poisoned connection"
        );

        let future_ran = Arc::new(AtomicBool::new(false));
        let future_ran_in_op = Arc::clone(&future_ran);
        let future_result = handle
            .send_top_level(move |_conn| {
                future_ran_in_op.store(true, Ordering::SeqCst);
                Ok::<(), StorageError>(())
            })
            .await;
        assert_writer_task_terminal_state(future_result, WriterTaskRequestState::NotStarted);
        assert!(!future_ran.load(Ordering::SeqCst));
    }

    #[test]
    fn operation_failure_with_failed_rollback_reports_side_effects_unknown() {
        let conn = Connection::open_in_memory().expect("in-memory connection");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE")
            .unwrap();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let request = WriteRequest {
            op: Box::new(|conn| -> Result<(), StorageError> {
                conn.authorizer(Some(deny_rollback))
                    .map_err(|e| StorageError::Pool {
                        operation: "test_install_authorizer".into(),
                        message: e.to_string(),
                    })?;
                Err(StorageError::Internal(
                    "intentional operation failure before denied rollback".into(),
                ))
            }),
            reply: reply_tx,
            top_level: false,
            telemetry: WriteTelemetry::new("test".to_string(), 0, None),
        };

        let terminal_state = sealed::Sealed::execute_and_reply_reporting_terminal(
            Box::new(request),
            &conn,
            None,
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(
            terminal_state,
            Some(WriterTaskRequestState::SideEffectsUnknown)
        );
        let reply = reply_rx
            .try_recv()
            .expect("active request must receive a typed terminal reply");
        assert_writer_task_terminal_state(reply, WriterTaskRequestState::SideEffectsUnknown);
        assert!(
            !conn.is_autocommit(),
            "the denied ROLLBACK must leave the test connection poisoned"
        );
    }

    #[test]
    fn wrapped_panic_with_failed_rollback_reports_side_effects_unknown() {
        let conn = Connection::open_in_memory().expect("in-memory connection");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE")
            .unwrap();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let request = WriteRequest {
            op: Box::new(|conn| -> Result<(), StorageError> {
                // Deliberately violate WriteOp's no-COMMIT contract to create
                // the otherwise rare but reachable rollback-failure state.
                // The row commits before the panic, so reporting a successful
                // rollback here would be dangerously false.
                conn.execute_batch("INSERT INTO t (id) VALUES (1); COMMIT")
                    .map_err(|e| StorageError::Pool {
                        operation: "test_force_rollback_failure".into(),
                        message: e.to_string(),
                    })?;
                panic!("intentional panic after illicit commit");
            }),
            reply: reply_tx,
            top_level: false,
            telemetry: WriteTelemetry::new("test".to_string(), 0, None),
        };

        let terminal_state = sealed::Sealed::execute_and_reply_reporting_terminal(
            Box::new(request),
            &conn,
            None,
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(
            terminal_state,
            Some(WriterTaskRequestState::SideEffectsUnknown)
        );
        let reply = reply_rx
            .try_recv()
            .expect("active request must receive a typed terminal reply");
        assert_writer_task_terminal_state(reply, WriterTaskRequestState::SideEffectsUnknown);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "the fixture's committed side effect proves why the state must be unknown"
        );
    }

    // `#[serial(tx_registry)]`: the active request holds a registered
    // `writer_task_tx` until its panic is caught and rolled back. Share the
    // process-wide registry key with the other writer-task transaction tests.
    #[tokio::test]
    #[serial(tx_registry)]
    async fn wrapped_panic_rolls_back_and_terminally_fails_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_wrapped_panic.db");
        let cfg = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            write_queue_capacity: 8,
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

        // Use the pool-owned OnceLock path rather than `spawn` directly so
        // the test also proves a terminal task is never silently replaced.
        let handle = pool
            .writer_task_handle()
            .expect("writer task lookup")
            .expect("file-backed queued pool must spawn its writer task");
        assert_eq!(pool.writer_task_spawn_count(), 1);

        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();
        let active = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .send(move |conn| -> Result<usize, StorageError> {
                        conn.execute("INSERT INTO t (id, v) VALUES (1, 'active')", [])
                            .map_err(|e| StorageError::Pool {
                                operation: "test_active_insert".into(),
                                message: e.to_string(),
                            })?;
                        let _ = started_tx.send(());
                        release_rx.recv().expect("test must release active op");
                        panic!("intentional wrapped writer request panic");
                    })
                    .await
            }
        });

        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("active request did not start")
            .expect("active request dropped its start signal");

        let queued_one_ran = Arc::new(AtomicBool::new(false));
        let queued_one_ran_in_op = Arc::clone(&queued_one_ran);
        let queued_one = handle
            .enqueue(move |conn| {
                queued_one_ran_in_op.store(true, Ordering::SeqCst);
                conn.execute("INSERT INTO t (id, v) VALUES (2, 'queued-one')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_queued_one_insert".into(),
                        message: e.to_string(),
                    })
            })
            .await
            .expect("first queued request must be accepted");

        let queued_two_ran = Arc::new(AtomicBool::new(false));
        let queued_two_ran_in_op = Arc::clone(&queued_two_ran);
        let queued_two = handle
            .enqueue(move |_conn| {
                queued_two_ran_in_op.store(true, Ordering::SeqCst);
                Ok::<String, StorageError>("queued-two-ran".to_string())
            })
            .await
            .expect("second queued request must be accepted");

        assert_eq!(
            handle.queue_depth(),
            2,
            "both heterogeneous requests must be buffered behind the active op"
        );
        release_tx.send(()).expect("release active op");

        let active_result = tokio::time::timeout(Duration::from_secs(5), active)
            .await
            .expect("active caller hung after panic")
            .expect("active caller task join");
        assert_writer_task_terminal_state(
            active_result,
            WriterTaskRequestState::TransactionRolledBack,
        );

        let queued_one_result = tokio::time::timeout(Duration::from_secs(5), queued_one)
            .await
            .expect("first queued caller hung after terminal failure")
            .expect("terminal drain must preserve first typed reply");
        assert_writer_task_terminal_state(queued_one_result, WriterTaskRequestState::NotStarted);

        let queued_two_result = tokio::time::timeout(Duration::from_secs(5), queued_two)
            .await
            .expect("second queued caller hung after terminal failure")
            .expect("terminal drain must preserve second typed reply");
        assert_writer_task_terminal_state(queued_two_result, WriterTaskRequestState::NotStarted);
        assert!(!queued_one_ran.load(Ordering::SeqCst));
        assert!(!queued_two_ran.load(Ordering::SeqCst));

        let future_ran = Arc::new(AtomicBool::new(false));
        let future_ran_in_op = Arc::clone(&future_ran);
        let future_result = handle
            .send(move |_conn| {
                future_ran_in_op.store(true, Ordering::SeqCst);
                Ok::<(), StorageError>(())
            })
            .await;
        assert_writer_task_terminal_state(future_result, WriterTaskRequestState::NotStarted);
        assert!(!future_ran.load(Ordering::SeqCst));

        let cached_after_failure = pool
            .writer_task_handle()
            .expect("cached writer task lookup")
            .expect("pool retains its terminal handle");
        assert_eq!(
            pool.writer_task_spawn_count(),
            1,
            "a terminal writer task must not be restarted behind callers' backs"
        );
        let cached_result = cached_after_failure
            .send(|_conn| Ok::<(), StorageError>(()))
            .await;
        assert_writer_task_terminal_state(cached_result, WriterTaskRequestState::NotStarted);

        let reader = pool.reader().expect("reader");
        let count: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "the active transaction must be rolled back and queued ops must never run"
        );
    }

    #[tokio::test]
    async fn top_level_panic_reports_unknown_and_fails_queue_without_running_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_top_level_panic.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
                .unwrap();
        }
        let handle = spawn(&pool, 8).expect("writer task spawn");

        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();
        let active = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .send_top_level(move |conn| -> Result<usize, StorageError> {
                        conn.execute("INSERT INTO t (id, v) VALUES (10, 'autocommitted')", [])
                            .map_err(|e| StorageError::Pool {
                                operation: "test_top_level_insert".into(),
                                message: e.to_string(),
                            })?;
                        let _ = started_tx.send(());
                        release_rx.recv().expect("test must release top-level op");
                        panic!("intentional top-level writer request panic");
                    })
                    .await
            }
        });

        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("top-level request did not start")
            .expect("top-level request dropped its start signal");

        let queued_ran = Arc::new(AtomicBool::new(false));
        let queued_ran_in_op = Arc::clone(&queued_ran);
        let queued = handle
            .enqueue(move |conn| {
                queued_ran_in_op.store(true, Ordering::SeqCst);
                conn.execute("INSERT INTO t (id, v) VALUES (11, 'queued')", [])
                    .map_err(|e| StorageError::Pool {
                        operation: "test_top_level_queued_insert".into(),
                        message: e.to_string(),
                    })
            })
            .await
            .expect("queued request must be accepted");
        assert_eq!(handle.queue_depth(), 1);
        release_tx.send(()).expect("release top-level op");

        let active_result = tokio::time::timeout(Duration::from_secs(5), active)
            .await
            .expect("top-level caller hung after panic")
            .expect("top-level caller task join");
        assert_writer_task_terminal_state(
            active_result,
            WriterTaskRequestState::SideEffectsUnknown,
        );

        let queued_result = tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .expect("queued caller hung after top-level panic")
            .expect("terminal drain must preserve queued typed reply");
        assert_writer_task_terminal_state(queued_result, WriterTaskRequestState::NotStarted);
        assert!(!queued_ran.load(Ordering::SeqCst));

        let reader = pool.reader().expect("reader");
        let active_count: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id = 10", [], |row| row.get(0))
            .unwrap();
        let queued_count: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id = 11", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            active_count, 1,
            "the completed top-level statement autocommits before the panic"
        );
        assert_eq!(queued_count, 0, "the queued request must never run");
    }

    #[tokio::test]
    async fn closed_receiver_rejects_all_send_surfaces_as_not_started() {
        // Simulates the writer task having terminated: its `rx` is gone, so
        // every send surface must fail deterministically without running the
        // supplied operation.
        let (tx, rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(4);
        drop(rx);

        let handle = WriterTaskHandle {
            tx,
            db: "test".to_string(),
            slow_write_threshold: None,
            enqueue_timeout: Duration::from_secs(5),
        };
        let send_result = handle.send(|_conn| Ok::<(), StorageError>(())).await;
        assert_writer_task_terminal_state(send_result, WriterTaskRequestState::NotStarted);

        let timed_result = handle
            .send_with_timeout(|_conn| Ok::<(), StorageError>(()), Duration::from_secs(1))
            .await;
        assert_writer_task_terminal_state(timed_result, WriterTaskRequestState::NotStarted);

        let top_level_result = handle
            .send_top_level(|_conn| Ok::<(), StorageError>(()))
            .await;
        assert_writer_task_terminal_state(top_level_result, WriterTaskRequestState::NotStarted);
    }

    #[tokio::test]
    async fn accepted_request_lost_reply_is_side_effects_unknown() {
        // A request can be accepted before an unexpected receiver/task loss.
        // The handle cannot prove whether its closure ran, so the oneshot
        // fallback must conservatively report SideEffectsUnknown.
        let (tx, mut rx) = mpsc::channel::<Box<dyn AnyWriteRequest + Send>>(1);
        let handle = WriterTaskHandle {
            tx,
            db: "test".to_string(),
            slow_write_threshold: None,
            enqueue_timeout: Duration::from_secs(5),
        };
        let request_ran = Arc::new(AtomicBool::new(false));
        let request_ran_in_op = Arc::clone(&request_ran);

        let dropper = tokio::spawn(async move {
            let request = rx.recv().await.expect("request must be accepted");
            drop(request);
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            handle.send(move |_conn| {
                request_ran_in_op.store(true, Ordering::SeqCst);
                Ok::<(), StorageError>(())
            }),
        )
        .await
        .expect("caller hung after accepted request was dropped");
        dropper.await.expect("dropper task join");

        assert_writer_task_terminal_state(result, WriterTaskRequestState::SideEffectsUnknown);
        assert!(!request_ran.load(Ordering::SeqCst));
    }

    /// #1849: a deliberately slow transaction body with no queue backlog or
    /// lock contention must be attributed to `body`, not flattened into one
    /// opaque send-to-reply duration.
    #[tokio::test]
    async fn writer_stage_sample_attributes_a_slow_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_stage_sample.db");
        let pool = file_pool(&path);
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
                .unwrap();
        }
        let handle = spawn(&pool, 8).unwrap();

        handle
            .send(|conn| {
                std::thread::sleep(Duration::from_millis(60));
                conn.execute("INSERT INTO t VALUES (1)", [])
                    .map_err(|error| StorageError::Pool {
                        operation: "writer_stage_sample".into(),
                        message: error.to_string(),
                    })
            })
            .await
            .unwrap();

        let sample = last_writer_stage_observation(&pool).expect("writer stage sample");
        assert!(
            sample.body_micros >= 50_000,
            "the synthetic delay must land in the body stage: {sample:?}"
        );
        assert!(
            sample.body_micros > sample.queue_wait_micros,
            "fast queueing must not receive the body's delay: {sample:?}"
        );
        assert!(
            sample.body_micros > sample.transaction_acquire_micros,
            "an uncontended BEGIN must not receive the body's delay: {sample:?}"
        );
        assert!(
            sample.body_micros > sample.commit_micros,
            "a fast COMMIT must not receive the body's delay: {sample:?}"
        );
        assert!(sample.observed_at_unix_ms > 0);
    }

    /// #1849: bounded-channel queue wait ends when the drain loop receives
    /// the request. Saturation in Tokio's blocking pool happens after that
    /// boundary and must remain in `total - named_stages`, never be relabeled
    /// as writer-queue contention.
    #[test]
    fn writer_queue_wait_excludes_blocking_pool_scheduling_delay() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("writer_dequeue_boundary.db");
            let pool = file_pool(&path);
            let handle = spawn(&pool, 8).unwrap();

            let (blocker_started_tx, blocker_started_rx) = std_mpsc::sync_channel(0);
            let (release_blocker_tx, release_blocker_rx) = std_mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                blocker_started_tx.send(()).unwrap();
                release_blocker_rx.recv().unwrap();
            });
            blocker_started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("sole blocking worker must be occupied");

            let reply = handle
                .enqueue(|_conn| Ok::<(), StorageError>(()))
                .await
                .expect("request must enter the bounded writer channel");
            let dequeue_deadline = Instant::now() + Duration::from_secs(1);
            while handle.queue_depth() != 0 {
                assert!(
                    Instant::now() < dequeue_deadline,
                    "writer drain never dequeued the accepted request"
                );
                tokio::task::yield_now().await;
            }

            let scheduling_delay = Duration::from_millis(150);
            tokio::time::sleep(scheduling_delay).await;
            release_blocker_tx.send(()).unwrap();
            blocker.await.unwrap();
            reply.await.unwrap().unwrap();

            let sample = last_writer_stage_observation(&pool).expect("writer stage sample");
            assert!(
                sample.total_micros.saturating_sub(sample.queue_wait_micros) >= 100_000,
                "the post-dequeue blocking-pool delay must not inflate queue_wait: {sample:?}"
            );
        });
    }
}
