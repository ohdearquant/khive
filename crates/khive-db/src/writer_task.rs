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
//! exactly one of the rows below. Request-path store writes, runtime
//! merge/symmetric-edge transactions,
//! and `khive_storage::SqlAccess` (`SqlBridge::writer`/`atomic_unit`) are subject
//! to `PoolConfig::write_queue_enabled` / `write_routing_strict` routing.
//! Everything else here is an
//! explicit, intentional exemption — not an implicit gap left over from an
//! incomplete migration.
//!
//! | Writer | Connection | Request-path write | Routing |
//! | --- | --- | --- | --- |
//! | Request-path writes (`create`, `update`, batch upserts, …) | `WriterTaskHandle` (queue-first) or a standalone/pool-mutex connection on degrade | Yes | Queue-first (ADR-136 D1 gate 1); strict routing fails closed on degrade |
//! | Startup / schema migrations (`migrations::run_migrations`, `apply_schema_plan`) | The pool's own writer-mutex connection (`ConnectionPool::new`, before the `WriterTask` is ever spawned) | No | Exempt — runs once at boot, before any queue handle exists to route through |
//! | Checkpointing (`checkpoint::CheckpointConnection`) | Dedicated standalone connection, opened once at task startup (ADR-091 Amendment 5, #1652) | No | Exempt by design — the whole point of that fix was moving checkpoint I/O OFF the pool writer's admission path; routing it back through the write queue would reintroduce the contention it removed |
//! | Recovery (`walpin` beacon/sidecar bookkeeping) | None — file-level bookkeeping (PID beacons, heartbeats) alongside the database, not a SQL connection against it | No | N/A — never acquires a database writer connection |
//! | Top-level maintenance (`VACUUM` via `execute_script_top_level`/`WriterTaskHandle::send_top_level`) | `WriterTaskHandle`, skipping the per-request `BEGIN IMMEDIATE`/`COMMIT` wrap only | Yes | Routed through the SAME single writer owner as every other queued request — only the transaction wrap is skipped, never the queue |
//!
//! Store constructors may run outside Tokio and temporarily cache no handle;
//! every store request-path write refreshes through
//! `ConnectionPool::writer_task_for_write` at execution time. Strict routing
//! refuses a missing handle, while the explicit compatibility fallback emits
//! its store-specific violation at the actual direct-writer seam. Runtime
//! entity/note merges and symmetric edge updates use the same routing policy
//! through a closed operation adapter. The strict default itself remains gated
//! by ADR-135 F2 / ADR-136 D2 evidence.
//!
//! A `direct_route_violation` sink row (`timeout_sink::emit_direct_route_violation`,
//! ADR-136 D1 gate 6c) is emitted ONLY for the first row's degrade path — a
//! request-path write that bypassed an enabled queue. The other four
//! rows are exempt by design and never emit that row.

use rusqlite::Connection;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

use khive_storage::error::{StorageError, WriterTaskRequestState};

use crate::error::SqliteError;
use crate::pool::{ConnectionPool, WriterAcquisitionCounters};
use crate::writer_admission_metrics::{
    AdmissionObservation, Snapshot, Unavailable, WriterAdmissionMetrics,
};

#[path = "writer_task_labels.rs"]
mod labels;

pub(crate) use labels::{collect_statement_labels, MAX_WRITE_LABELS};
use labels::{dedup_labels, SubmitLabels};

/// Retry delays after a busy/locked `BEGIN IMMEDIATE` refusal. The operation
/// closure remains owned by the request until one BEGIN succeeds, so these
/// are the only retries this path may perform. Persistent contention is
/// bounded to three total BEGIN attempts and 15 ms of explicit backoff, and
/// the attempts together share one configured `busy_timeout` acquisition
/// budget rather than each waiting out a full window of their own.
const WRITER_BEGIN_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(5), Duration::from_millis(10)];

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
    /// Distinct labels the drained request ran, in first-seen order,
    /// deduplicated and bounded at `MAX_WRITE_LABELS`. Resolved only for a
    /// write that crossed the slow-write threshold, so empty for every write
    /// below it; also empty when no statement in the request carried a label
    /// and the submit site had no name in hand — e.g. an opaque closure from
    /// a caller with no `&'static str` operation.
    pub labels: Vec<String>,
    /// Distinct labels dropped past `MAX_WRITE_LABELS`, when the request
    /// named more; `None` when nothing was dropped.
    pub labels_truncated: Option<u64>,
}

static WRITER_STAGE_OBSERVATIONS: OnceLock<
    Mutex<HashMap<Option<PathBuf>, WriterStageObservation>>,
> = OnceLock::new();

fn writer_stage_observations() -> &'static Mutex<HashMap<Option<PathBuf>, WriterStageObservation>> {
    WRITER_STAGE_OBSERVATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn writer_db_key_from_path(path: Option<&Path>) -> Option<PathBuf> {
    path.map(Path::to_path_buf)
}

fn writer_db_key(pool: &ConnectionPool) -> Option<PathBuf> {
    writer_db_key_from_path(pool.canonical_path())
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
    writer_stage_observations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&writer_db_key(pool))
        .cloned()
}

struct WriteTelemetry {
    backend_key: Option<PathBuf>,
    db: String,
    submitted_at: Instant,
    queue_depth_at_entry: usize,
    slow_write_threshold: Option<Duration>,
    labels: SubmitLabels,
}

impl WriteTelemetry {
    fn new(
        backend_key: Option<PathBuf>,
        db: String,
        queue_depth_at_entry: usize,
        slow_write_threshold: Option<Duration>,
        labels: SubmitLabels,
    ) -> Self {
        Self {
            backend_key,
            db,
            submitted_at: Instant::now(),
            queue_depth_at_entry,
            slow_write_threshold,
            labels,
        }
    }

    fn queue_wait(&self) -> Duration {
        self.submitted_at.elapsed()
    }

    /// The labels named on this request, bounded to [`MAX_WRITE_LABELS`]
    /// plus the count of distinct labels dropped. Computed only on the slow
    /// path, since only [`Self::finish`] calls it: the fast path never turns
    /// a [`SubmitLabels::Site`] name into an allocation.
    fn resolve_labels(&self) -> (Vec<String>, Option<u64>) {
        match &self.labels {
            SubmitLabels::None => (Vec::new(), None),
            SubmitLabels::Site(site) => (vec![(*site).to_owned()], None),
            SubmitLabels::Statements(labels) => {
                if labels.len() <= MAX_WRITE_LABELS {
                    (labels.clone(), None)
                } else {
                    let dropped = (labels.len() - MAX_WRITE_LABELS) as u64;
                    (labels[..MAX_WRITE_LABELS].to_vec(), Some(dropped))
                }
            }
        }
    }

    fn finish(
        self,
        queue_wait: Duration,
        transaction_acquire: Duration,
        body: Duration,
        commit: Duration,
    ) {
        let total = self.submitted_at.elapsed();
        let slow = self
            .slow_write_threshold
            .is_some_and(|threshold| total >= threshold);
        // Labels are resolved only for a write that crossed the slow
        // threshold, so a fast write never turns a site name into an
        // allocation; its observation carries no labels.
        let (labels, labels_truncated) = if slow {
            self.resolve_labels()
        } else {
            (Vec::new(), None)
        };
        let observation = WriterStageObservation {
            queue_wait_micros: duration_micros(queue_wait),
            transaction_acquire_micros: duration_micros(transaction_acquire),
            body_micros: duration_micros(body),
            commit_micros: duration_micros(commit),
            total_micros: duration_micros(total),
            queue_depth_at_entry: self.queue_depth_at_entry as u64,
            observed_at_unix_ms: observed_at_unix_ms(),
            labels,
            labels_truncated,
        };
        writer_stage_observations()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.backend_key, observation.clone());

        if slow {
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
    /// Only the typed checkpoint entry point may bypass disk admission.
    checkpoint_bypass: bool,
    vacuum_copy_headroom: bool,
    telemetry: WriteTelemetry,
    admission: Option<AdmissionObservation>,
}

mod sealed {
    /// Restricts [`super::AnyWriteRequest`] to implementations defined in
    /// this module — only [`super::WriteRequest<R>`] implements it — and
    /// carries the drain loop's internal terminal-state reporting methods
    /// without changing the public execution-method signatures.
    pub trait Sealed {
        fn start_execution(&mut self);

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

    fn is_checkpoint_bypass(&self) -> bool;

    fn needs_vacuum_headroom(&self) -> bool;

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
///
/// This worker/owned adapter catches callback panics, attempts rollback, and
/// returns a typed terminal outcome. In contrast, [`crate::pool::WriterGuard::transaction`]
/// propagates callback panics and settles its owner during unwinding.
pub(crate) fn execute_wrapped_transaction<R, F>(
    conn: &Connection,
    commit_operation: &'static str,
    operation: F,
) -> (Result<R, StorageError>, Option<WriterTaskRequestState>)
where
    F: FnOnce(&Connection) -> Result<R, StorageError>,
{
    let profiled =
        execute_wrapped_transaction_profiled(conn, commit_operation, None, false, operation);
    (profiled.result, profiled.terminal_state)
}

/// The standalone SQL bridge shares settlement rules while retaining its
/// native commit error source and timeout/full observation site.
pub(crate) fn execute_wrapped_sql_bridge_transaction<R, F>(
    conn: &Connection,
    commit_operation: &'static str,
    db: &str,
    operation: F,
) -> (Result<R, StorageError>, Option<WriterTaskRequestState>)
where
    F: FnOnce(&Connection) -> Result<R, StorageError>,
{
    let profiled =
        execute_wrapped_transaction_profiled(conn, commit_operation, Some(db), true, operation);
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
    db: Option<&str>,
    direct_sql_bridge: bool,
    operation: F,
) -> ProfiledWrappedTransaction<R>
where
    F: FnOnce(&Connection) -> Result<R, StorageError>,
{
    let body_started = Instant::now();
    // Keep panic-to-terminal conversion local to this adapter; the public
    // WriterGuard transaction intentionally propagates the callback's unwind.
    let operation_outcome = catch_unwind(AssertUnwindSafe(|| {
        operation(conn).map_err(crate::error::statement_failure)
    }));
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
                Err(commit_error) => {
                    if let Some(db) = db {
                        if direct_sql_bridge {
                            crate::timeout_sink::maybe_emit_busy(
                                db,
                                crate::timeout_sink::Site::StandaloneSqlBridge,
                                &commit_error,
                            );
                        } else {
                            crate::timeout_sink::maybe_emit_sqlite_full(db, &commit_error);
                        }
                    }
                    match rollback_after_failure(conn, "commit failure") {
                        RollbackDisposition::RolledBack => ProfiledWrappedTransaction {
                            result: Err(StorageError::WriterTaskRequestFailed {
                                request_state: WriterTaskRequestState::TransactionRolledBack,
                                source: Box::new({
                                    let failure = crate::error::native_write_failure(
                                        &commit_error,
                                        khive_storage::error::SqliteWriteStage::Commit,
                                    );
                                    let error = if direct_sql_bridge {
                                        StorageError::driver(
                                            khive_storage::StorageCapability::Sql,
                                            commit_operation,
                                            commit_error,
                                        )
                                    } else {
                                        StorageError::Pool {
                                            operation: commit_operation.into(),
                                            message: commit_error.to_string(),
                                        }
                                    };
                                    match failure {
                                        Some(failure) => error.with_sqlite_write_failure(failure),
                                        None => error,
                                    }
                                }),
                            }),
                            terminal_state: None,
                            body,
                            commit,
                        },
                        RollbackDisposition::SideEffectsUnknown => {
                            let request_state = WriterTaskRequestState::SideEffectsUnknown;
                            ProfiledWrappedTransaction {
                                result: Err(writer_task_terminated_with_cause(
                                    request_state,
                                    &commit_error,
                                    khive_storage::error::SqliteWriteStage::Commit,
                                )),
                                terminal_state: Some(request_state),
                                body,
                                commit,
                            }
                        }
                    }
                }
            }
        }
        Ok(Err(operation_error)) => {
            match rollback_after_failure(conn, "request operation failure") {
                RollbackDisposition::RolledBack => ProfiledWrappedTransaction {
                    result: Err(StorageError::WriterTaskRequestFailed {
                        request_state: WriterTaskRequestState::TransactionRolledBack,
                        source: Box::new(operation_error),
                    }),
                    terminal_state: None,
                    body,
                    commit: Duration::ZERO,
                },
                RollbackDisposition::SideEffectsUnknown => {
                    // SQLite may have already rolled back a failed transaction,
                    // leaving no transaction for the explicit ROLLBACK.
                    // Escalate the original cause once; retain native FULL evidence below.
                    if let Some(db) = db {
                        crate::timeout_sink::maybe_emit_sqlite_full(db, &operation_error);
                    }
                    let request_state = WriterTaskRequestState::SideEffectsUnknown;
                    ProfiledWrappedTransaction {
                        result: Err(writer_task_terminated_with_cause(
                            request_state,
                            &operation_error,
                            khive_storage::error::SqliteWriteStage::Statement,
                        )),
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

fn reply_with_admission<R>(
    reply: oneshot::Sender<Result<R, StorageError>>,
    mut admission: Option<AdmissionObservation>,
    result: Result<R, StorageError>,
) {
    if let Some(observation) = admission.as_mut() {
        observation.reply_attempt();
    }
    let _ = reply.send(result);
}

impl<R: Send + 'static> sealed::Sealed for WriteRequest<R> {
    fn start_execution(&mut self) {
        if let Some(observation) = self.admission.as_mut() {
            observation.start_execution();
        }
    }

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
            admission,
            ..
        } = *self;
        let profiled = execute_wrapped_transaction_profiled(
            conn,
            "writer_task_commit",
            Some(&telemetry.db),
            false,
            op,
        );
        if let Err(error) = &profiled.result {
            crate::timeout_sink::maybe_emit_sqlite_full(&telemetry.db, error);
        }
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
        reply_with_admission(reply, admission, profiled.result);
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
            admission,
            ..
        } = *self;
        let body_started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            op(conn).map_err(crate::error::statement_failure)
        }));
        let body = body_started.elapsed();
        let telemetry_db = telemetry.db.clone();
        telemetry.finish(queue_wait, Duration::ZERO, body, Duration::ZERO);
        match outcome {
            Ok(outcome) if conn.is_autocommit() => {
                // No COMMIT/ROLLBACK here: this request explicitly did not
                // open a transaction, so there is nothing to close.
                if let Err(error) = &outcome {
                    crate::timeout_sink::maybe_emit_sqlite_full(&telemetry_db, error);
                }
                reply_with_admission(reply, admission, outcome);
                None
            }
            Ok(outcome) => {
                tracing::error!(
                    "writer task: top-level request returned with an open transaction; request \
                     side effects are unknown"
                );
                let request_state = WriterTaskRequestState::SideEffectsUnknown;
                let error = match outcome {
                    Err(error) => writer_task_terminated_with_cause(
                        request_state,
                        &error,
                        khive_storage::error::SqliteWriteStage::Statement,
                    ),
                    Ok(_) => writer_task_terminated(request_state),
                };
                reply_with_admission(reply, admission, Err(error));
                Some(request_state)
            }
            Err(_panic_payload) => {
                // Statements completed before a top-level panic may already
                // have autocommitted. Report the ambiguity; never invent a
                // rollback for a request that opened no transaction.
                let request_state = WriterTaskRequestState::SideEffectsUnknown;
                reply_with_admission(reply, admission, Err(writer_task_terminated(request_state)));
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
            reply,
            telemetry,
            admission,
            ..
        } = *self;
        telemetry.finish(
            queue_wait,
            transaction_acquire,
            Duration::ZERO,
            Duration::ZERO,
        );
        reply_with_admission(reply, admission, Err(err));
    }
}

impl<R: Send + 'static> AnyWriteRequest for WriteRequest<R> {
    fn execute_and_reply(mut self: Box<Self>, conn: &Connection) {
        let queue_wait = self.queue_wait();
        sealed::Sealed::start_execution(self.as_mut());
        let _ = sealed::Sealed::execute_and_reply_reporting_terminal(
            self,
            conn,
            None,
            queue_wait,
            Duration::ZERO,
        );
    }

    fn execute_and_reply_top_level(mut self: Box<Self>, conn: &Connection) {
        let queue_wait = self.queue_wait();
        sealed::Sealed::start_execution(self.as_mut());
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

    fn is_checkpoint_bypass(&self) -> bool {
        self.checkpoint_bypass
    }

    fn needs_vacuum_headroom(&self) -> bool {
        self.vacuum_copy_headroom
    }

    fn queue_wait(&self) -> Duration {
        self.telemetry.queue_wait()
    }
}

fn writer_task_terminated(request_state: WriterTaskRequestState) -> StorageError {
    StorageError::writer_task_terminated(request_state)
}

fn writer_task_terminated_with_cause(
    request_state: WriterTaskRequestState,
    error: &(dyn std::error::Error + 'static),
    stage: khive_storage::error::SqliteWriteStage,
) -> StorageError {
    let mut source = Some(error);
    let mut sqlite_full_codes = None;
    while let Some(current) = source {
        if let Some(rusqlite::Error::SqliteFailure(code, _)) =
            current.downcast_ref::<rusqlite::Error>()
        {
            if code.code == rusqlite::ErrorCode::DiskFull {
                sqlite_full_codes = Some((code.extended_code & 0xff, code.extended_code));
                break;
            }
        }
        source = current.source();
    }
    // The original error was already escalated at the failing boundary.
    // A source-free pair preserves caller evidence without duplicating that sink event.
    let terminal = StorageError::WriterTaskTerminated {
        request_state,
        sqlite_full_codes,
    };
    match crate::error::native_write_failure(error, stage) {
        Some(mut failure) => {
            failure.settlement_unknown =
                request_state == WriterTaskRequestState::SideEffectsUnknown;
            terminal.with_sqlite_write_failure(failure)
        }
        None => terminal,
    }
}

fn writer_task_begin_error(error: rusqlite::Error, busy_timeout: Duration) -> StorageError {
    let failure =
        crate::error::native_write_failure(&error, khive_storage::error::SqliteWriteStage::Begin);
    let mapped = if crate::timeout_sink::is_busy_or_locked(&error) {
        StorageError::WriterTaskBusy {
            timeout_ms: u64::try_from(busy_timeout.as_millis()).unwrap_or(u64::MAX),
        }
    } else {
        StorageError::Pool {
            operation: "writer_task_begin".into(),
            message: error.to_string(),
        }
    };
    match failure {
        Some(failure) => mapped.with_sqlite_write_failure(failure),
        None => mapped,
    }
}

/// Sender half of the write queue. Cheaply cloneable (wraps an
/// `mpsc::Sender`) — every migrated store that shares one writer task holds
/// a clone of this handle.
#[derive(Clone, Debug)]
pub struct WriterTaskHandle {
    tx: mpsc::Sender<Box<dyn AnyWriteRequest + Send>>,
    /// Exact canonical backend identity used by the in-memory stage registry.
    /// Kept separate from `db`, whose lossy display form is logging-only.
    backend_key: Option<PathBuf>,
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
    admission_metrics: Arc<WriterAdmissionMetrics>,
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
        self.enqueue_inner(op, false, false, false, SubmitLabels::None)
            .await
    }

    /// Shared enqueue path for both transaction-wrapped ([`Self::enqueue`])
    /// and top-level ([`Self::send_top_level`]) requests — `top_level`
    /// controls which [`AnyWriteRequest`] method the drain loop invokes.
    /// `labels` is what the submit site knows about this request's
    /// statements; see [`SubmitLabels`].
    async fn enqueue_inner<R, F>(
        &self,
        op: F,
        top_level: bool,
        checkpoint_bypass: bool,
        vacuum_copy_headroom: bool,
        labels: SubmitLabels,
    ) -> Result<oneshot::Receiver<Result<R, StorageError>>, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        // Normalize here (once per request) so every submit path — including
        // the public `send_labelled` entry points a caller can hand a raw
        // list — records distinct labels in first-seen order. Bounding stays
        // in `resolve_labels`, which runs only on the slow path.
        let labels = match labels {
            SubmitLabels::Statements(labels) => {
                SubmitLabels::Statements(dedup_labels(labels.iter().map(String::as_str)))
            }
            other => other,
        };
        let telemetry = WriteTelemetry::new(
            self.backend_key.clone(),
            self.db.clone(),
            self.queue_depth(),
            self.slow_write_threshold,
            labels,
        );
        let mut request = Box::new(WriteRequest {
            op: Box::new(op),
            reply: reply_tx,
            top_level,
            checkpoint_bypass,
            vacuum_copy_headroom,
            telemetry,
            admission: None,
        });
        request.admission = Some(self.admission_metrics.offer());
        let permit = self
            .tx
            .reserve()
            .await
            .map_err(|_| writer_task_terminated(WriterTaskRequestState::NotStarted))?;
        if let Some(observation) = request.admission.as_mut() {
            observation.accepted();
        }
        permit.send(request);

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
        self.send_with_timeout_labelled(op, timeout, SubmitLabels::None)
            .await
    }

    /// Shared body of [`Self::send_with_timeout`] and its labelled variants.
    /// `labels` is carried onto the request's writer-stage observation and
    /// `slow_write` row; see [`SubmitLabels`].
    async fn send_with_timeout_labelled<R, F>(
        &self,
        op: F,
        timeout: std::time::Duration,
        labels: SubmitLabels,
    ) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let reply_rx = match tokio::time::timeout(
            timeout,
            self.enqueue_inner(op, false, false, false, labels),
        )
        .await
        {
            Ok(Ok(reply_rx)) => reply_rx,
            Ok(Err(e)) => return Err(e),
            Err(_elapsed) => {
                let timeout_ms = timeout.as_millis() as u64;
                self.admission_metrics.record_capacity_deadline();
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
        let reply_rx = self
            .enqueue_inner(op, true, false, false, SubmitLabels::None)
            .await?;
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
        self.send_top_level_bounded_inner(op, false, false, SubmitLabels::None)
            .await
    }

    /// Checkpoint-only top-level dispatch. It remains serialized by the
    /// writer task but never waits for the capacity lease or floor probe.
    pub(crate) async fn send_checkpoint_bounded<R, F>(&self, op: F) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        self.send_top_level_bounded_inner(op, true, false, SubmitLabels::Site("wal_checkpoint"))
            .await
    }

    /// Typed VACUUM dispatch with a copy-sized metadata estimate at dequeue.
    pub(crate) async fn send_vacuum_bounded<R, F>(&self, op: F) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        self.send_top_level_bounded_inner(op, false, true, SubmitLabels::Site("vacuum"))
            .await
    }

    async fn send_top_level_bounded_inner<R, F>(
        &self,
        op: F,
        checkpoint_bypass: bool,
        vacuum_copy_headroom: bool,
        labels: SubmitLabels,
    ) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let reply_rx = match tokio::time::timeout(
            self.enqueue_timeout,
            self.enqueue_inner(op, true, checkpoint_bypass, vacuum_copy_headroom, labels),
        )
        .await
        {
            Ok(Ok(reply_rx)) => reply_rx,
            Ok(Err(e)) => return Err(e),
            Err(_elapsed) => {
                let timeout_ms = self.enqueue_timeout.as_millis() as u64;
                self.admission_metrics.record_capacity_deadline();
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

    pub(crate) fn admission_snapshot(&self) -> Result<Snapshot, Unavailable> {
        self.admission_metrics.snapshot()
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
/// # Terminal settlement
/// Retiring an unsettled writer, including when its blocking owner unwinds,
/// releases the volume lease only after cleanup restored autocommit or the
/// connection closed. If neither is possible, path, volume, and errors are
/// reported to stderr and tracing and the pool is poisoned: that request
/// reports [`SqliteError::WriterSettlementUnknown`] and every later write is
/// refused with [`SqliteError::WriterPoisoned`] before it starts.
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
    pool.spawn_writer_task(capacity)
}

/// Consume a verified lifetime connection inside its admission-owning task.
/// Only the pool's private connection factory opens that connection.
pub(crate) fn spawn_with_connection(
    pool: &ConnectionPool,
    conn: Connection,
    capacity: usize,
) -> Result<WriterTaskHandle, SqliteError> {
    // The lifetime connection is infrastructure, not one acquisition per
    // write. Each dequeued request is counted below at the task's actual
    // ownership boundary instead.
    let acquisition_counters = pool.writer_acquisition_counters();
    let write_admission = pool.write_admission();
    let busy_timeout = pool.config().busy_timeout;
    let origin = pool.origin();
    let backend_key = writer_db_key(pool);
    let db = crate::timeout_sink::db_label(pool);
    let (tx, rx) = mpsc::channel(capacity.max(1));
    let join = tokio::spawn(run_writer_task(
        (conn, pool.timeout_sink_writer_lease()),
        rx,
        origin,
        db.clone(),
        acquisition_counters,
        write_admission,
        busy_timeout,
    ));
    // Stored on the pool (not returned) so the handle's clone-and-share
    // contract stays untouched; see ConnectionPool::take_writer_task_join
    // for who awaits it and why.
    pool.set_writer_task_join(join);
    Ok(WriterTaskHandle {
        tx,
        backend_key,
        db,
        slow_write_threshold: crate::timeout_sink::slow_write_threshold(),
        enqueue_timeout: std::time::Duration::from_millis(
            pool.config().write_admission_deadline_ms,
        ),
        admission_metrics: WriterAdmissionMetrics::new(),
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

/// Acquire the request transaction without multiplying its busy-timeout budget.
/// The setter is supplied so tests can fail SQLite's timeout update while still
/// executing real BEGIN statements; production uses `Connection::busy_timeout`.
fn begin_immediate_with_retry(
    conn: &Connection,
    acquisition_counters: &WriterAcquisitionCounters,
    busy_timeout: Duration,
    mut set_busy_timeout: impl FnMut(&Connection, Duration) -> rusqlite::Result<()>,
) -> (rusqlite::Result<()>, Duration, u32) {
    let transaction_acquire_started = Instant::now();
    let mut begin_attempt = 1_u32;
    let mut retry_delays = WRITER_BEGIN_RETRY_DELAYS.into_iter();
    let mut busy_timeout_lowered = false;
    let begin_outcome = loop {
        match conn.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => break Ok(()),
            Err(error) if crate::timeout_sink::is_busy_or_locked(&error) => {
                // Every busy/locked refusal is counted here,
                // whether or not it goes on to be retried: this
                // is the pre-PR meaning of the counter, and the
                // outer match below no longer counts the final
                // refusal a second time.
                acquisition_counters.record_writer_task_begin_busy();
                let Some(delay) = retry_delays.next() else {
                    break Err(error);
                };
                // The whole retry sequence shares one
                // `busy_timeout` budget: each SQLite attempt is
                // itself bounded by `busy_timeout`, so without a
                // shrinking budget three attempts could each
                // wait out a full window and multiply the
                // serialized writer's contention window instead
                // of bounding it.
                let remaining_budget =
                    busy_timeout.saturating_sub(transaction_acquire_started.elapsed());
                if remaining_budget.is_zero() {
                    break Err(error);
                }
                if let Err(set_err) = set_busy_timeout(conn, remaining_budget) {
                    tracing::warn!(
                        error = %set_err,
                        "writer task: failed to lower busy_timeout for BEGIN \
                         retry; surfacing the original busy refusal"
                    );
                    // A retry with the old timeout could exceed the shared budget.
                    break Err(error);
                }
                busy_timeout_lowered = true;
                // Count only refusals that will actually be retried. A failed
                // timeout reduction leaves this refusal visible to the caller.
                acquisition_counters.record_writer_task_begin_busy_absorbed();
                tracing::debug!(
                    attempt = begin_attempt,
                    backoff_ms = delay.as_millis() as u64,
                    budget_remaining_ms = remaining_budget.as_millis() as u64,
                    "writer task: BEGIN IMMEDIATE refused busy; retrying before \
                     request execution"
                );
                std::thread::sleep(delay);
                begin_attempt = begin_attempt.saturating_add(1);
            }
            Err(error) => break Err(error),
        }
    };
    let transaction_acquire = transaction_acquire_started.elapsed();
    // Restore the pool-configured busy_timeout only if a retry
    // actually lowered it, so the common uncontended request
    // never pays for an extra pragma write; the next request
    // dequeued on this same connection must still start from
    // the full configured budget.
    if busy_timeout_lowered {
        if let Err(restore_err) = set_busy_timeout(conn, busy_timeout) {
            tracing::warn!(
                error = %restore_err,
                "writer task: failed to restore busy_timeout after a BEGIN retry \
                 sequence"
            );
        }
    }
    (begin_outcome, transaction_acquire, begin_attempt)
}

struct BlockingWriterConnection {
    conn: Option<Connection>,
    admission: Arc<crate::pool::WriteAdmission>,
    volume_lease: Option<crate::disk_guard::VolumeLease>,
}

impl BlockingWriterConnection {
    fn new(conn: Connection, admission: Arc<crate::pool::WriteAdmission>) -> Self {
        Self {
            conn: Some(conn),
            admission,
            volume_lease: None,
        }
    }

    fn finish(
        mut self,
        state: Option<WriterTaskRequestState>,
    ) -> (Option<Connection>, Option<WriterTaskRequestState>) {
        if state.is_some() {
            self.retire();
            (None, state)
        } else {
            (self.conn.take(), None)
        }
    }

    fn retire(&mut self) {
        if let Some(conn) = self.conn.take() {
            // A terminal failure has poisoned the admission; the request that
            // retired this writer already carries its terminal state.
            let _ = self.admission.close_retired_connection(conn);
        }
    }
}

impl Drop for BlockingWriterConnection {
    fn drop(&mut self) {
        self.retire();
    }
}

impl std::ops::Deref for BlockingWriterConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("blocking writer owns its connection until settlement")
    }
}

/// Drain loop: the sole caller of `BEGIN IMMEDIATE` for write traffic routed
/// through the channel. Busy/locked `BEGIN IMMEDIATE` refusals receive the
/// bounded retry above before a final failure replies the request's error via
/// [`AnyWriteRequest::reply_error`] without invoking the request's closure.
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
    mut owner: (Connection, Option<crate::timeout_sink::WriterLease>),
    mut rx: mpsc::Receiver<Box<dyn AnyWriteRequest + Send>>,
    origin: khive_storage::tx_registry::TxOrigin,
    db: String,
    acquisition_counters: Arc<WriterAcquisitionCounters>,
    write_admission: Arc<crate::pool::WriteAdmission>,
    busy_timeout: Duration,
) {
    while let Some(mut request) = rx.recv().await {
        // The bounded-channel wait ends at this exact dequeue boundary.
        // Sampling inside the blocking closure would misattribute a saturated
        // Tokio blocking pool to writer-queue contention (#1849).
        let queue_wait = request.queue_wait();
        sealed::Sealed::start_execution(request.as_mut());
        let origin = origin.clone();
        let blocking_counters = Arc::clone(&acquisition_counters);
        let blocking_admission = Arc::clone(&write_admission);
        let blocking_db = db.clone();
        // Tuple field order drops the connection before its sink lease, both
        // while idle and when a cancelled parent discards a native result.
        let conn = owner.0;
        let blocking_sink_lease = owner.1.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let outcome = (|| {
                let mut conn = BlockingWriterConnection::new(conn, Arc::clone(&blocking_admission));
                let acquisition_counters = blocking_counters;
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
                    return conn.finish(Some(request_state));
                }

                // The lease precedes SQLite writer acquisition and remains held
                // until the request has committed, rolled back, or returned to
                // autocommit. Checkpoint requests deliberately skip both steps.
                conn.volume_lease = if request.is_checkpoint_bypass() {
                    None
                } else {
                    match blocking_admission.acquire() {
                        Ok(lease) => lease,
                        Err(error) => {
                            request.reply_error(error.into_storage_error(
                                khive_storage::StorageCapability::Sql,
                                "writer_task_admission",
                            ));
                            return conn.finish(None);
                        }
                    }
                };

                let terminal_state = if request.is_top_level() {
                    if !request.is_checkpoint_bypass() {
                        let admission = if request.needs_vacuum_headroom() {
                            blocking_admission.check_for_vacuum()
                        } else {
                            blocking_admission.check()
                        };
                        if let Err(error) = admission {
                            request.reply_error(error.into_storage_error(
                                khive_storage::StorageCapability::Sql,
                                "writer_task_admission",
                            ));
                            return conn.finish(None);
                        }
                    }
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
                    let (begin_outcome, transaction_acquire, begin_attempt) =
                        begin_immediate_with_retry(
                            &conn,
                            &acquisition_counters,
                            busy_timeout,
                            Connection::busy_timeout,
                        );
                    match begin_outcome {
                        Ok(()) => {
                            if let Err(error) = blocking_admission.check() {
                                let request_state =
                                    match rollback_after_failure(&conn, "capacity admission") {
                                        RollbackDisposition::RolledBack => None,
                                        RollbackDisposition::SideEffectsUnknown => {
                                            Some(WriterTaskRequestState::SideEffectsUnknown)
                                        }
                                    };
                                drop(tx_span);
                                let error = if let Some(state) = request_state {
                                    writer_task_terminated(state)
                                } else {
                                    error.into_storage_error(
                                        khive_storage::StorageCapability::Sql,
                                        "writer_task_admission",
                                    )
                                };
                                sealed::Sealed::reply_error_after_begin(
                                    request,
                                    error,
                                    queue_wait,
                                    transaction_acquire,
                                );
                                return conn.finish(request_state);
                            }
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
                            crate::timeout_sink::maybe_emit_sqlite_full(&blocking_db, &e);
                            tracing::warn!(
                                error = %e,
                                attempts = begin_attempt,
                                "writer task: BEGIN IMMEDIATE failed; replying an \
                                 error without running the request's operation"
                            );
                            // Although BEGIN never opened a SQL transaction, the
                            // scoped attempt is observable in tx_registry. Drop
                            // it before waking the caller for the same reply-side
                            // lifecycle guarantee as successful requests (#1790).
                            drop(tx_span);
                            // A busy/locked refusal was already counted inside
                            // the retry loop above, including this final one —
                            // classifying and counting it again here would
                            // double it. Only a non-busy BEGIN failure (which
                            // never enters the retry loop's busy arm) still
                            // needs its counter recorded at this seam.
                            let begin_error = writer_task_begin_error(e, busy_timeout);
                            if !matches!(
                                begin_error.without_sqlite_write_stage(),
                                StorageError::WriterTaskBusy { .. }
                            ) {
                                acquisition_counters.record_writer_task_begin_error();
                            }
                            sealed::Sealed::reply_error_after_begin(
                                request,
                                begin_error,
                                queue_wait,
                                transaction_acquire,
                            );
                            None
                        }
                    }
                };
                conn.finish(terminal_state)
            })();
            (outcome, blocking_sink_lease)
        })
        .await;

        match outcome {
            Ok(((Some(returned_conn), None), _sink_lease)) => owner.0 = returned_conn,
            Ok(((_returned_conn, Some(request_state)), _sink_lease)) => {
                acquisition_counters.record_writer_task_request_failure();
                if request_state == WriterTaskRequestState::SideEffectsUnknown {
                    acquisition_counters.record_writer_task_side_effects_unknown();
                }
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
            Ok(((None, None), _sink_lease)) => {
                unreachable!("a reusable writer returns its connection")
            }
            Err(join_err) => {
                acquisition_counters.record_writer_task_request_failure();
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
#[path = "writer_task_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "writer_task_lease_close_tests.rs"]
mod volume_lease_close_tests;

#[cfg(test)]
#[path = "writer_task_slow_write_label_tests.rs"]
mod slow_write_label_tests;

#[cfg(all(test, any(unix, windows)))]
#[path = "writer_task_identity_tests.rs"]
mod identity_tests;
