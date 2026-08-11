//! Request-scoped cancellation and deadlines for read-only SQLite work.
//!
//! Tokio does not stop a [`tokio::task::spawn_blocking`] closure when its
//! awaiting future is dropped. Every async SQLite read therefore crosses the
//! boundary in this module: the async side can interrupt the exact registered
//! connection, while a connection-local progress callback is the backstop for
//! cancellation races and request deadlines. Write closures never register.

use std::cell::RefCell;
#[cfg(test)]
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant as WallInstant};

use khive_storage::error::StorageError;
use khive_storage::types::StorageResult;
use khive_storage::{
    capture_request_read_context, RequestReadContext, RequestReadStopReason, StorageCapability,
};

/// Post-deadline window for interrupted read tasks to finalize SQLite and
/// return their connection before a coordinator escalates async cancellation.
pub const DEFAULT_SQLITE_INTERRUPT_GRACE_MS: u64 = 500;

/// Resolve the validated SQLite interrupt-settlement grace.
pub fn sqlite_interrupt_grace_from_env() -> Duration {
    let millis = std::env::var("KHIVE_SQLITE_INTERRUPT_GRACE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| (10..=5_000).contains(millis))
        .unwrap_or(DEFAULT_SQLITE_INTERRUPT_GRACE_MS);
    Duration::from_millis(millis)
}

/// Second-stage bound applied once the grace window in
/// [`sqlite_interrupt_grace_from_env`] has already elapsed without the
/// worker settling. `spawn_blocking` cannot be force-aborted, so the only
/// way to prove pool admission was actually released before a caller is
/// told so is to keep joining the real worker up to this bound. Only a
/// worker that still has not settled after grace *and* this cap is detached
/// with admission left unrecovered (see the post-grace branch in
/// [`run_interruptible_read_inner`]).
pub const DEFAULT_SQLITE_INTERRUPT_HARD_CAP_MS: u64 = 5_000;

/// Resolve the validated hard cap on post-grace join waiting.
pub fn sqlite_interrupt_hard_cap_from_env() -> Duration {
    let millis = std::env::var("KHIVE_SQLITE_INTERRUPT_HARD_CAP_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| (100..=60_000).contains(millis))
        .unwrap_or(DEFAULT_SQLITE_INTERRUPT_HARD_CAP_MS);
    Duration::from_millis(millis)
}

#[cfg(test)]
#[derive(Clone, Default)]
struct DbReadTestContext {
    progress_probe: Option<Arc<std::sync::atomic::AtomicUsize>>,
    fail_progress_clear: bool,
}

#[cfg(test)]
tokio::task_local! {
    static DB_READ_TEST_CONTEXT: DbReadTestContext;
}

#[cfg(test)]
fn current_test_context() -> DbReadTestContext {
    DB_READ_TEST_CONTEXT
        .try_with(Clone::clone)
        .unwrap_or_default()
}

#[cfg(test)]
pub async fn scope_test_read_progress<F>(
    probe: Arc<std::sync::atomic::AtomicUsize>,
    future: F,
) -> F::Output
where
    F: Future,
{
    let mut context = current_test_context();
    context.progress_probe = Some(probe);
    DB_READ_TEST_CONTEXT.scope(context, future).await
}

#[cfg(test)]
pub(crate) async fn scope_test_read_cleanup_failure<F>(future: F) -> F::Output
where
    F: Future,
{
    let mut context = current_test_context();
    context.fail_progress_clear = true;
    DB_READ_TEST_CONTEXT.scope(context, future).await
}

const STOP_NONE: u8 = 0;
const STOP_ABANDONED: u8 = 1;
const STOP_REQUEST: u8 = 2;
const STOP_DEADLINE: u8 = 3;
const PHASE_WAITING: u8 = 0;
const PHASE_RUNNING: u8 = 1;
const PHASE_CLEANING: u8 = 2;
const PHASE_FINISHED: u8 = 3;

fn stop_reason_code(reason: RequestReadStopReason) -> u8 {
    match reason {
        RequestReadStopReason::Cancelled => STOP_REQUEST,
        RequestReadStopReason::Deadline => STOP_DEADLINE,
    }
}

enum RegistrationState {
    Waiting,
    Running(rusqlite::InterruptHandle),
    /// The statement/cursor is finalized and connection-local recovery is
    /// running. Late cancellation must not target the ROLLBACK or make the
    /// progress callback interrupt cleanup.
    Cleaning,
    Finished,
}

struct ReadControl {
    state: parking_lot::Mutex<RegistrationState>,
    /// Lock-free lifecycle mirror for SQLite's high-frequency progress path.
    /// The mutex remains authoritative for InterruptHandle ownership only.
    lifecycle: AtomicU8,
    stopped: AtomicBool,
    stop_reason: AtomicU8,
    registered: AtomicBool,
    /// Set once a raw-SQL work closure classifies its prepared statement as
    /// an admitted write/transaction-control statement and is about to run
    /// it. Before this is set (including the entire `prepare`/bind phase),
    /// cancellation must stay grace-bounded even though no read has
    /// registered yet — nothing has executed against SQLite, so abandoning
    /// the wait cannot strand a write mid-flight. After this is set, the
    /// worker must reach its real completion (ADR-005's completion-
    /// preserving write contract).
    write_committed: AtomicBool,
    cleanup_failed: AtomicBool,
    deadline: Option<WallInstant>,
    operation: &'static str,
    #[cfg(test)]
    progress_probe: Option<Arc<std::sync::atomic::AtomicUsize>>,
    #[cfg(test)]
    fail_progress_clear: bool,
}

impl ReadControl {
    fn new(context: &RequestReadContext, operation: &'static str) -> Arc<Self> {
        #[cfg(test)]
        let test_context = current_test_context();
        let control = Arc::new(Self {
            state: parking_lot::Mutex::new(RegistrationState::Waiting),
            lifecycle: AtomicU8::new(PHASE_WAITING),
            stopped: AtomicBool::new(false),
            stop_reason: AtomicU8::new(STOP_NONE),
            registered: AtomicBool::new(false),
            write_committed: AtomicBool::new(false),
            cleanup_failed: AtomicBool::new(false),
            deadline: context.deadline().map(|deadline| deadline.blocking_at()),
            operation,
            #[cfg(test)]
            progress_probe: test_context.progress_probe,
            #[cfg(test)]
            fail_progress_clear: test_context.fail_progress_clear,
        });
        if let Some(reason) = context.stop_reason() {
            control.cancel(stop_reason_code(reason));
        }
        control
    }

    fn cancel(&self, reason: u8) {
        if self.lifecycle.load(Ordering::Acquire) >= PHASE_CLEANING {
            return;
        }
        let state = self.state.lock();
        if matches!(
            *state,
            RegistrationState::Cleaning | RegistrationState::Finished
        ) {
            // Completion won the race. Never let a late task drop target a
            // connection that may already have been returned to its owner.
            return;
        }
        let _ = self.stop_reason.compare_exchange(
            STOP_NONE,
            reason,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.stopped.store(true, Ordering::Release);
        if let RegistrationState::Running(interrupt) = &*state {
            interrupt.interrupt();
        }
    }

    fn progress_should_stop(&self) -> bool {
        #[cfg(test)]
        if let Some(probe) = &self.progress_probe {
            probe.fetch_add(1, Ordering::Relaxed);
        }
        // This callback runs every 1,000 SQLite VM instructions. Never take
        // the lifecycle mutex here: it exists only to protect ownership of
        // the InterruptHandle used by asynchronous cancellers.
        if self.lifecycle.load(Ordering::Acquire) >= PHASE_CLEANING {
            return false;
        }
        if self
            .deadline
            .is_some_and(|deadline| WallInstant::now() >= deadline)
        {
            let _ = self.stop_reason.compare_exchange(
                STOP_NONE,
                STOP_DEADLINE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.stopped.store(true, Ordering::Release);
            return true;
        }
        self.stopped.load(Ordering::Acquire)
    }

    fn timeout_error(&self) -> StorageError {
        StorageError::Timeout {
            operation: self.operation.into(),
        }
    }

    fn register<'a>(
        self: &Arc<Self>,
        conn: &'a rusqlite::Connection,
        capability: StorageCapability,
    ) -> StorageResult<ActiveRead<'a>> {
        if self.progress_should_stop() {
            return Err(self.timeout_error());
        }
        {
            let mut state = self.state.lock();
            match &*state {
                RegistrationState::Waiting => {
                    *state = RegistrationState::Running(conn.get_interrupt_handle());
                    self.lifecycle.store(PHASE_RUNNING, Ordering::Release);
                    self.registered.store(true, Ordering::Release);
                }
                RegistrationState::Cleaning
                | RegistrationState::Finished
                | RegistrationState::Running(_) => {
                    return Err(StorageError::Internal(format!(
                        "{}: SQLite read scope registered more than once",
                        self.operation
                    )));
                }
            }
        }

        let callback = Arc::clone(self);
        if let Err(error) =
            conn.progress_handler(1_000, Some(move || callback.progress_should_stop()))
        {
            self.cleanup_failed.store(true, Ordering::Release);
            self.finish();
            return Err(StorageError::driver(capability, self.operation, error));
        }

        let previous = CURRENT_READ_CONTROL.with(|current| current.replace(Some(Arc::clone(self))));
        if self.progress_should_stop() {
            if let RegistrationState::Running(interrupt) = &*self.state.lock() {
                interrupt.interrupt();
            }
        }
        Ok(ActiveRead {
            conn,
            control: Arc::clone(self),
            previous,
            capability,
            finished: false,
        })
    }

    fn finish(&self) {
        self.lifecycle.store(PHASE_FINISHED, Ordering::Release);
        *self.state.lock() = RegistrationState::Finished;
    }

    fn begin_cleanup(&self) -> u8 {
        let mut state = self.state.lock();
        if matches!(*state, RegistrationState::Running(_)) {
            self.lifecycle.store(PHASE_CLEANING, Ordering::Release);
            *state = RegistrationState::Cleaning;
        }
        self.stop_reason.load(Ordering::Acquire)
    }
}

thread_local! {
    static CURRENT_READ_CONTROL: RefCell<Option<Arc<ReadControl>>> = const { RefCell::new(None) };
}

/// Predicate for a specialized SQLite progress callback (currently bounded
/// graph traversal) to compose with the common request signal.
pub(crate) fn current_read_should_interrupt() -> bool {
    CURRENT_READ_CONTROL.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|control| control.progress_should_stop())
    })
}

struct ActiveRead<'a> {
    conn: &'a rusqlite::Connection,
    control: Arc<ReadControl>,
    previous: Option<Arc<ReadControl>>,
    capability: StorageCapability,
    finished: bool,
}

impl ActiveRead<'_> {
    fn clear(&mut self) -> StorageResult<()> {
        #[cfg(test)]
        if self.control.fail_progress_clear {
            self.control.cleanup_failed.store(true, Ordering::Release);
            CURRENT_READ_CONTROL.with(|current| {
                current.replace(self.previous.take());
            });
            self.control.finish();
            self.finished = true;
            return Err(StorageError::Internal(format!(
                "{}: injected SQLite progress-handler clear failure",
                self.control.operation
            )));
        }
        let clear = self
            .conn
            .progress_handler(0, None::<fn() -> bool>)
            .map_err(|error| {
                self.control.cleanup_failed.store(true, Ordering::Release);
                StorageError::driver(self.capability, self.control.operation, error)
            });
        CURRENT_READ_CONTROL.with(|current| {
            current.replace(self.previous.take());
        });
        self.control.finish();
        self.finished = true;
        clear
    }
}

impl Drop for ActiveRead<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Panic/unwind fallback. The ordinary path calls `clear`
            // explicitly so a failure is returned to the caller. Marking the
            // control lets every connection owner fail closed instead of
            // recycling a connection whose callback could not be removed.
            if let Err(error) = self.clear() {
                tracing::warn!(
                    operation = self.control.operation,
                    error = %error,
                    "failed to clear SQLite read progress handler"
                );
            }
        }
    }
}

/// Blocking-side handle used to register the exact SQLite connection after
/// checkout and before any read statement starts.
pub(crate) struct InterruptibleReadScope {
    control: Arc<ReadControl>,
    capability: StorageCapability,
}

struct QuarantinePooledReaderOnDrop<'guard, 'pool> {
    guard: &'guard mut crate::pool::ReaderGuard<'pool>,
    control: Arc<ReadControl>,
}

impl Drop for QuarantinePooledReaderOnDrop<'_, '_> {
    fn drop(&mut self) {
        if self.control.cleanup_failed.load(Ordering::Acquire) {
            self.guard.discard();
        }
    }
}

struct QuarantinePooledWriterOnDrop<'guard, 'pool_ref, 'guard_pool> {
    pool: &'pool_ref crate::pool::ConnectionPool,
    guard: &'guard crate::pool::WriterGuard<'guard_pool>,
    control: Arc<ReadControl>,
}

impl Drop for QuarantinePooledWriterOnDrop<'_, '_, '_> {
    fn drop(&mut self) {
        if self.control.cleanup_failed.load(Ordering::Acquire) {
            self.pool.retire_pooled_writer(self.guard.conn());
        }
    }
}

impl InterruptibleReadScope {
    /// Refuse to acquire another read resource after this request stopped.
    pub(crate) fn ensure_active(&self) -> StorageResult<()> {
        if self.control.progress_should_stop() {
            Err(self.control.timeout_error())
        } else {
            Ok(())
        }
    }

    /// Predicate for cooperative reader-pool checkout.
    pub(crate) fn should_stop(&self) -> bool {
        self.control.progress_should_stop()
    }

    /// Raw-SQL call sites call this exactly once, after classifying a
    /// prepared statement as an admitted write/transaction-control
    /// statement and before executing it. It tells the async boundary that
    /// this worker is no longer safe to bound-wait on: SQLite state is
    /// about to change and the completion-preserving contract (ADR-005)
    /// applies from this point on, even though no interrupt target was
    /// ever registered for it.
    pub(crate) fn mark_write_committed(&self) {
        self.control.write_committed.store(true, Ordering::Release);
    }

    /// Run one read closure with interrupt and progress-handler ownership.
    pub(crate) fn run<R, F>(&self, conn: &rusqlite::Connection, read: F) -> StorageResult<R>
    where
        F: FnOnce() -> StorageResult<R>,
    {
        self.run_with_interrupted_cleanup(conn, read, || Ok(()))
    }

    /// Run a read with connection-local recovery that must occur after the
    /// statement/cursor is finalized but before the progress callback and
    /// interrupt target are cleared. Cached explicit read transactions use
    /// this seam to roll back an interrupted snapshot in teardown order.
    pub(crate) fn run_with_interrupted_cleanup<R, F, C>(
        &self,
        conn: &rusqlite::Connection,
        read: F,
        interrupted_cleanup: C,
    ) -> StorageResult<R>
    where
        F: FnOnce() -> StorageResult<R>,
        C: FnOnce() -> StorageResult<()>,
    {
        let mut active = self.control.register(conn, self.capability)?;
        let result = read();
        let stop_reason = self.control.begin_cleanup();
        let cleanup = if stop_reason != STOP_NONE {
            interrupted_cleanup()
        } else {
            Ok(())
        };
        if cleanup.is_err() {
            self.control.cleanup_failed.store(true, Ordering::Release);
        }
        active.clear()?;
        cleanup?;
        match result {
            Err(error) if stop_reason != STOP_NONE && storage_error_is_sqlite_interrupt(&error) => {
                Err(self.control.timeout_error())
            }
            Ok(_) if stop_reason != STOP_NONE => Err(self.control.timeout_error()),
            other => other,
        }
    }

    /// Whether removing the connection-global progress callback failed.
    /// Connection owners consult this after a read and discard/quarantine the
    /// connection even though the cleanup error itself is already returned.
    pub(crate) fn cleanup_failed(&self) -> bool {
        self.control.cleanup_failed.load(Ordering::Acquire)
    }

    /// Run against a checked-out pooled reader and ensure a progress-handler
    /// cleanup failure makes that exact connection non-reusable.
    pub(crate) fn run_pooled_reader<R, F>(
        &self,
        guard: &mut crate::pool::ReaderGuard<'_>,
        read: F,
    ) -> StorageResult<R>
    where
        F: FnOnce(&rusqlite::Connection) -> StorageResult<R>,
    {
        self.with_pooled_reader(guard, |conn| self.run(conn, || read(conn)))
    }

    /// Keep pooled-reader cleanup quarantine armed across arbitrary raw SQL
    /// classification and execution, including panic unwinding before the
    /// caller can inspect `cleanup_failed`.
    pub(crate) fn with_pooled_reader<R, F>(
        &self,
        guard: &mut crate::pool::ReaderGuard<'_>,
        read: F,
    ) -> R
    where
        F: FnOnce(&rusqlite::Connection) -> R,
    {
        let quarantine = QuarantinePooledReaderOnDrop {
            guard,
            control: Arc::clone(&self.control),
        };
        let conn = quarantine.guard.conn();
        read(conn)
    }

    /// Keep the degraded/shared writer quarantiner armed across raw reader
    /// operations. Writes themselves never register this read control, but a
    /// SELECT issued through `SqlReader` on the writer can install the common
    /// progress callback and must retire the writer if cleanup fails or
    /// unwinds.
    pub(crate) fn with_pooled_writer<R, F>(
        &self,
        pool: &crate::pool::ConnectionPool,
        guard: &crate::pool::WriterGuard<'_>,
        read: F,
    ) -> R
    where
        F: FnOnce(&rusqlite::Connection) -> R,
    {
        let quarantine = QuarantinePooledWriterOnDrop {
            pool,
            guard,
            control: Arc::clone(&self.control),
        };
        read(quarantine.guard.conn())
    }
}

fn storage_error_is_sqlite_interrupt(error: &StorageError) -> bool {
    let StorageError::Driver { source, .. } = error else {
        return false;
    };
    if let Some(error) = source.downcast_ref::<rusqlite::Error>() {
        return error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted);
    }
    source
        .downcast_ref::<crate::error::SqliteError>()
        .and_then(|error| match error {
            crate::error::SqliteError::Rusqlite(error) => error.sqlite_error_code(),
            _ => None,
        })
        == Some(rusqlite::ErrorCode::OperationInterrupted)
}

struct CancelReadOnDrop {
    control: Arc<ReadControl>,
    armed: bool,
}

impl Drop for CancelReadOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.control.cancel(STOP_ABANDONED);
        }
    }
}

/// Run blocking read work under the current request cancellation/deadline.
async fn run_interruptible_read_inner<R, F>(
    capability: StorageCapability,
    operation: &'static str,
    declared_read_only: bool,
    work: F,
) -> StorageResult<R>
where
    R: Send + 'static,
    F: FnOnce(&InterruptibleReadScope) -> StorageResult<R> + Send + 'static,
{
    let context = capture_request_read_context();
    let control = ReadControl::new(&context, operation);
    let worker_control = Arc::clone(&control);
    let mut worker = tokio::task::spawn_blocking(move || {
        let scope = InterruptibleReadScope {
            control: Arc::clone(&worker_control),
            capability,
        };
        let result = work(&scope);
        worker_control.finish();
        result
    });
    let mut drop_guard = CancelReadOnDrop {
        control: Arc::clone(&control),
        armed: true,
    };

    let result = tokio::select! {
        joined = &mut worker => joined
            .map_err(|error| StorageError::driver(capability, operation, error))?,
        reason = context.wait_for_stop() => {
            control.cancel(stop_reason_code(reason));
            // Registered reads translate their causally recorded
            // SQLITE_INTERRUPT inside `InterruptibleReadScope::run`. A
            // statement that has committed to running as an admitted write
            // (`mark_write_committed`) deliberately never registers, so
            // cancellation must await and return its real completion result
            // rather than inventing an ambiguous timeout. Everything else —
            // including work still classifying its statement, which has not
            // touched SQLite yet — stays bounded.
            if control.registered.load(Ordering::Acquire)
                || declared_read_only
                || !control.write_committed.load(Ordering::Acquire)
            {
                match tokio::time::timeout(sqlite_interrupt_grace_from_env(), &mut worker).await {
                    Ok(joined) => joined
                        .map_err(|error| StorageError::driver(capability, operation, error))?,
                    Err(_) => {
                        // `spawn_blocking` cannot be force-aborted once it has
                        // started, so the only way to prove admission was
                        // actually released before answering the caller is to
                        // keep joining the real worker. Escalate to a second,
                        // longer bound instead of detaching immediately: the
                        // progress flag remains latched and the InterruptHandle
                        // was already fired, so ordinary work settles well
                        // inside this window and the caller still gets a
                        // typed timeout, just later. Only work that ignores
                        // the interrupt entirely (a hostile callback/UDF) can
                        // exhaust the hard cap below.
                        tracing::error!(
                            operation,
                            grace_ms = sqlite_interrupt_grace_from_env().as_millis(),
                            hard_cap_ms = sqlite_interrupt_hard_cap_from_env().as_millis(),
                            "interrupted SQLite read did not settle within grace; \
                             escalating to a bounded join before reporting a timeout"
                        );
                        match tokio::time::timeout(sqlite_interrupt_hard_cap_from_env(), &mut worker)
                            .await
                        {
                            Ok(joined) => {
                                tracing::warn!(
                                    operation,
                                    "interrupted SQLite read settled after grace within the \
                                     hard cap; admission was released before this response"
                                );
                                joined.map_err(|error| {
                                    StorageError::driver(capability, operation, error)
                                })?
                            }
                            Err(_) => {
                                // Truly did not settle. Detach: the worker
                                // still owns its connection and admission
                                // until it eventually exits on its own, which
                                // this boundary cannot force or observe.
                                worker.abort();
                                tracing::error!(
                                    operation,
                                    hard_cap_ms = sqlite_interrupt_hard_cap_from_env().as_millis(),
                                    "interrupted SQLite read exceeded the hard cap; detaching \
                                     worker — its pool admission will not recover until it exits"
                                );
                                Err(control.timeout_error())
                            }
                        }
                    }
                }
            } else {
                // An admitted write/transaction-control statement is running.
                // It must reach its real completion/rollback boundary.
                worker.await
                    .map_err(|error| StorageError::driver(capability, operation, error))?
            }
        }
    };
    drop_guard.armed = false;
    result
}

/// Run work that must classify its prepared statement before it is safe to
/// interrupt. Raw SQL entry points use this path so DML-with-RETURNING and
/// transaction control remain admitted, non-interruptible writes.
pub(crate) async fn run_interruptible_read<R, F>(
    capability: StorageCapability,
    operation: &'static str,
    work: F,
) -> StorageResult<R>
where
    R: Send + 'static,
    F: FnOnce(&InterruptibleReadScope) -> StorageResult<R> + Send + 'static,
{
    run_interruptible_read_inner(capability, operation, false, work).await
}

/// Run a statically read-only store operation.
///
/// Unlike raw SQL, typed store reads may return at the interrupt-grace bound
/// even if cancellation arrives while opening/checking out the reader, before
/// SQLite registration. Their blocking closure remains responsible for the
/// connection until it observes the latched stop and exits without executing
/// a statement.
pub(crate) async fn run_declared_interruptible_read<R, F>(
    capability: StorageCapability,
    operation: &'static str,
    work: F,
) -> StorageResult<R>
where
    R: Send + 'static,
    F: FnOnce(&InterruptibleReadScope) -> StorageResult<R> + Send + 'static,
{
    run_interruptible_read_inner(capability, operation, true, work).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_storage::scope_request_read_cancellation;

    #[test]
    fn sqlite_progress_fast_path_never_locks_registration_state() {
        let source = include_str!("read_cancellation.rs");
        let body = source
            .split("fn progress_should_stop(&self) -> bool {")
            .nth(1)
            .and_then(|tail| tail.split("fn timeout_error(&self)").next())
            .expect("progress callback body remains discoverable");
        assert!(
            !body.contains("state.lock"),
            "SQLite's per-1,000-op progress callback must stay lock-free"
        );
    }

    /// Regression for the PR #1897 review's HIGH finding: raw SQL prepares
    /// its statement before deciding whether it is a cancellable read, so a
    /// cancellation arriving during that window has no interrupt target
    /// registered yet. Before this fix, `run_interruptible_read_inner` read
    /// that as "must be an in-flight write" and took the fully unbounded
    /// `worker.await` branch — abandoned work still classifying its
    /// statement could hold the async caller forever. This closure never
    /// registers and never calls `mark_write_committed`, simulating exactly
    /// that window; the assertion is that cancellation still returns within
    /// the grace+hard-cap bound rather than waiting for the closure's real
    /// 6-second completion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_unclassified_work_stays_bounded() {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let started = std::time::Instant::now();
        let call = scope_request_read_cancellation(cancel_rx, async {
            run_interruptible_read(StorageCapability::Sql, "test_unclassified", |_scope| {
                std::thread::sleep(Duration::from_millis(6_000));
                Ok(1i64)
            })
            .await
        });
        let handle = tokio::spawn(call);
        tokio::task::yield_now().await;
        cancel_tx.send(true).unwrap();

        let result = tokio::time::timeout(Duration::from_millis(5_800), handle)
            .await
            .expect(
                "cancellation before registration/write-commit must stay grace+hard-cap \
                 bounded instead of waiting for the worker's real completion",
            )
            .unwrap();
        assert!(
            matches!(result, Err(StorageError::Timeout { .. })),
            "an abandoned, still-classifying read must surface a typed timeout; got {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(6_000),
            "must return before the closure's own 6s completion"
        );
    }

    /// Companion to the above: once a raw-SQL work closure has classified
    /// its statement as an admitted write and calls `mark_write_committed`,
    /// cancellation must NOT bound-wait — ADR-005's completion-preserving
    /// contract for writes still applies even though this closure never
    /// registered an interrupt target either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_after_write_commit_awaits_real_completion() {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let started = std::time::Instant::now();
        let call = scope_request_read_cancellation(cancel_rx, async {
            run_interruptible_read(StorageCapability::Sql, "test_write_committed", |scope| {
                scope.mark_write_committed();
                std::thread::sleep(Duration::from_millis(900));
                Ok(42i64)
            })
            .await
        });
        let handle = tokio::spawn(call);
        tokio::task::yield_now().await;
        cancel_tx.send(true).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("an admitted write must still complete")
            .unwrap();
        assert_eq!(
            result.ok(),
            Some(42),
            "a committed write must return its real result, never a fabricated timeout"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "a committed write must not return before its real completion"
        );
    }
}
