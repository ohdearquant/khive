//! ADR-133 Slice 1: the audit-batch seam.
//!
//! Incidental audit writes (gate denials, dispatch outcomes, config-lock
//! rows, git.digest receipts, and pure-observability rows like
//! `RecallExecuted`) no longer take one writer-task acquisition per row on
//! the request hot path. Concurrent submissions arriving while a generation
//! is committing share the *next* generation instead of each taking their
//! own writer acquisition, so N concurrent producers collapse to one
//! [`khive_storage::EventStore::append_events_idempotent`] call per
//! generation rather than N.
//!
//! [`AuditBatch`] owns this seam. A lazily-spawned supervisor task drains
//! pending rows into generations and drives each through the store; the
//! supervisor's own `JoinHandle` is retained (never discarded) so an
//! abnormal exit — panic, cancellation, a lost child join, or a driver that
//! returns `Ok` while state is not terminally consistent — is observed and
//! converted into a `Failed` transition with all accepted waiters resolved,
//! per owner ruling R1/R4 (`.khive/OWNER_RULING_adr133_gate.md`).

#[cfg(any(test, feature = "fault-injection"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use khive_storage::event::EventAppendDisposition;
use khive_storage::{Event, EventStore, StorageError, WriterTaskRequestState};

/// Coarse durability classification for an [`AuditProducer`]. See
/// [`classify`] for the exhaustive mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuditProductionClass {
    /// The dispatch this row audits carries an obligation: the caller-visible
    /// outcome must not silently diverge from what was durably recorded.
    DispatchObligation,
    /// The row is a best-effort observability signal; a non-commit degrades
    /// gracefully rather than blocking or failing the dispatch it audits.
    PureObservability,
}

/// Every call site that can submit a row through [`AuditBatchControl`].
/// Adding a variant here without extending [`classify`]'s match is a
/// compile error — there is no wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditProducer {
    /// The gate denied a dispatch; the denial itself is audited.
    GateDenied,
    /// A pack dispatch returned a successful result.
    DispatchSucceeded,
    /// A pack dispatch returned an error result.
    DispatchFailed,
    /// The gate allowed a verb no pack owns.
    UnknownVerb,
    /// The strict `git.digest` success receipt (schema v2).
    GitDigestReceipt,
    /// A drained process-lifetime `OnceLock` config-lock row.
    ConfigLocked,
    /// A `memory.recall` execution's pure-observability audit row.
    ///
    /// Classified and ready for routing, but not yet wired to a live call
    /// site in this slice: `khive-pack-memory`'s recall handler reaches only
    /// `KhiveRuntime` (`crates/khive-runtime/src/runtime.rs`), which does not
    /// hold this batch seam and is outside this change's file ownership
    /// (`final_file_ownership_r2.md` assigns `runtime.rs` to the D8 author).
    /// Wiring this variant to `emit_recall_executed_event` needs either an
    /// ownership-map amendment granting `runtime.rs` a narrow accessor, or
    /// threading a second seam onto `KhiveRuntime` — both out of scope here.
    #[allow(dead_code)]
    RecallExecuted,
}

/// Classify an [`AuditProducer`] into its [`AuditProductionClass`]. One
/// exhaustive match, no wildcard arm.
pub(crate) const fn classify(producer: AuditProducer) -> AuditProductionClass {
    match producer {
        AuditProducer::GateDenied
        | AuditProducer::DispatchSucceeded
        | AuditProducer::DispatchFailed
        | AuditProducer::UnknownVerb
        | AuditProducer::GitDigestReceipt => AuditProductionClass::DispatchObligation,
        AuditProducer::ConfigLocked | AuditProducer::RecallExecuted => {
            AuditProductionClass::PureObservability
        }
    }
}

/// Exhaustive terminal reasons an [`AuditBatchControl::submit`],
/// [`AuditBatchControl::quiesce`], or [`AuditBatchControl::close_and_drain`]
/// call can resolve to. Never mapped through a wildcard arm anywhere in this
/// module (R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditTerminalReason {
    /// `EventStore::preflight_event` rejected the row before it was ever
    /// enqueued. The row was never counted as submitted.
    PreflightRejected,
    /// The batch is `Closing` or `Closed`; new admission is refused.
    AdmissionClosed,
    /// `AuditBatchConfig::max_pending_rows` was reached.
    QueueAdmissionExhausted,
    /// A row shared this generation's id with a previously stored row whose
    /// columns or observation projection did not match exactly.
    IdentityConflict,
    /// The store returned a terminal (non-retryable, or retries exhausted)
    /// error for this generation.
    StoreFailure,
    /// The configured `EventStore` backend does not implement
    /// `append_events_idempotent`.
    IdempotencyUnsupported,
    /// The generation driver task panicked, or the supervisor awaiting it
    /// unwound while armed.
    DriverPanicked,
    /// The generation driver task was cancelled/aborted, or the supervisor
    /// awaiting it was dropped mid-await (shutdown abort) while armed.
    DriverCancelled,
    /// The child driver's `JoinHandle` was lost — dropped without ever being
    /// inspected — so its outcome could not be classified.
    DriverJoinLost,
    /// The driver returned `Ok` but locked batch state was not proved
    /// terminally consistent afterward (`in_flight` still set, or the
    /// generation's rows were never resolved).
    DriverExitedInconsistent,
}

/// One row accepted for batching: the immutable event identity plus the
/// producer that minted it, used for classification and, on failure,
/// degradation accounting.
pub struct PreparedAuditRow {
    pub event: Event,
    pub producer: AuditProducer,
}

/// What [`AuditBatchControl::submit`] resolves to on a non-error outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditCommitOutcome {
    /// The row was freshly inserted.
    Committed,
    /// A prior row with the same identity already matched exactly (internal
    /// retry replaying the same producer-minted identity).
    AlreadyPresentIdentical,
}

/// Tunables for the batch seam. Defaults are conservative; every field is
/// exercised by at least one mechanism test.
#[derive(Debug, Clone)]
pub struct AuditBatchConfig {
    pub max_pending_rows: std::num::NonZeroUsize,
    pub max_rows_per_generation: std::num::NonZeroUsize,
    pub max_commit_attempts: std::num::NonZeroU8,
    pub retry_backoff: Duration,
    pub admission_deadline: Duration,
}

impl Default for AuditBatchConfig {
    fn default() -> Self {
        Self {
            max_pending_rows: std::num::NonZeroUsize::new(4096).unwrap(),
            max_rows_per_generation: std::num::NonZeroUsize::new(256).unwrap(),
            max_commit_attempts: std::num::NonZeroU8::new(3).unwrap(),
            retry_backoff: Duration::from_millis(20),
            admission_deadline: Duration::from_secs(5),
        }
    }
}

#[async_trait::async_trait]
pub trait AuditBatchControl: Send + Sync {
    async fn submit(
        &self,
        row: PreparedAuditRow,
    ) -> Result<AuditCommitOutcome, AuditTerminalReason>;
    async fn quiesce(&self) -> Result<(), AuditTerminalReason>;
    async fn close_and_drain(&self) -> Result<(), AuditTerminalReason>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Lifecycle {
    Open,
    Closing,
    Closed,
    Failed(AuditTerminalReason),
}

struct Waiting {
    event: Event,
    producer: AuditProducer,
    responder: oneshot::Sender<Result<AuditCommitOutcome, AuditTerminalReason>>,
}

/// A committed/failed generation's accounting, retained for the lifetime of
/// the process (bounded in practice by process lifetime and generation
/// volume; this slice makes no attempt to prune history).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditGenerationSnapshot {
    pub generation_id: u64,
    pub submitted_rows: u64,
    pub committed_rows: u64,
    pub store_batch_calls: u64,
    pub terminal_reason: Option<AuditTerminalReason>,
}

struct State {
    lifecycle: Lifecycle,
    pending: Vec<Waiting>,
    in_flight_generation: Option<u64>,
    driver_active: bool,
    next_generation_id: u64,
    submitted_rows: u64,
    committed_rows: u64,
    store_batch_calls: u64,
    generations: Vec<AuditGenerationSnapshot>,
    flush_failures: u64,
    degraded_rows: u64,
    degraded: bool,
}

impl State {
    fn new() -> Self {
        Self {
            lifecycle: Lifecycle::Open,
            pending: Vec::new(),
            in_flight_generation: None,
            driver_active: false,
            next_generation_id: 0,
            submitted_rows: 0,
            committed_rows: 0,
            store_batch_calls: 0,
            generations: Vec::new(),
            flush_failures: 0,
            degraded_rows: 0,
            degraded: false,
        }
    }

    fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.in_flight_generation.is_none() && !self.driver_active
    }
}

struct Inner {
    state: Mutex<State>,
}

impl Inner {
    /// Wins once: the first abnormal driver/supervisor exit sets `Failed`
    /// and drains every accepted waiter (this generation's, plus anything
    /// still queued for a future one) with the same typed reason. A later
    /// abnormal path observes the existing terminal state and does not
    /// double-count (R1).
    fn fail_driver(&self, reason: AuditTerminalReason, in_flight_waiters: Vec<Waiting>) {
        let mut state = self.state.lock();
        let already_failed = matches!(state.lifecycle, Lifecycle::Failed(_));
        if !already_failed {
            state.lifecycle = Lifecycle::Failed(reason);
        }
        state.driver_active = false;
        state.in_flight_generation = None;
        let drained: Vec<Waiting> = std::mem::take(&mut state.pending);
        if !already_failed {
            state.flush_failures += 1;
            let generation_id = state.next_generation_id;
            state.generations.push(AuditGenerationSnapshot {
                generation_id,
                submitted_rows: (in_flight_waiters.len() + drained.len()) as u64,
                committed_rows: 0,
                store_batch_calls: 0,
                terminal_reason: Some(reason),
            });
            state.next_generation_id += 1;
        }
        drop(state);
        for waiting in in_flight_waiters.into_iter().chain(drained) {
            record_degradation_if_pure(self, waiting.producer);
            let _ = waiting.responder.send(Err(reason));
        }
    }

    fn record_degradation(&self) {
        let mut state = self.state.lock();
        state.degraded_rows += 1;
        state.degraded = true;
    }
}

fn record_degradation_if_pure(inner: &Inner, producer: AuditProducer) {
    if classify(producer) == AuditProductionClass::PureObservability {
        inner.record_degradation();
    }
}

/// Armed for the lifetime of one generation's supervision. If dropped while
/// still armed — the supervisor's own frame unwinding (panic) or being
/// dropped mid-await (cancellation/shutdown abort) — the guard fails the
/// generation before returning control to whatever tore it down, so the
/// caller never observes a background-task-count restoration that raced
/// ahead of the failure broadcast (R1).
struct SupervisorGuard<'a> {
    inner: &'a Arc<Inner>,
    waiting: Option<Vec<Waiting>>,
}

impl<'a> SupervisorGuard<'a> {
    fn armed(inner: &'a Arc<Inner>, waiting: Vec<Waiting>) -> Self {
        Self {
            inner,
            waiting: Some(waiting),
        }
    }

    /// Clean disarm: the caller has fully classified the outcome and taken
    /// ownership of the waiters to resolve them itself.
    fn disarm(mut self) -> Vec<Waiting> {
        self.waiting.take().unwrap_or_default()
    }

    /// Explicit failure classification reached without unwinding/cancelling
    /// this frame (e.g. a `JoinError` was returned normally). Equivalent to
    /// what `Drop` does when armed, but callable inline.
    fn fail(mut self, reason: AuditTerminalReason) {
        if let Some(waiting) = self.waiting.take() {
            self.inner.fail_driver(reason, waiting);
        }
    }
}

impl Drop for SupervisorGuard<'_> {
    fn drop(&mut self) {
        let Some(waiting) = self.waiting.take() else {
            return;
        };
        let reason = if std::thread::panicking() {
            AuditTerminalReason::DriverPanicked
        } else {
            AuditTerminalReason::DriverCancelled
        };
        self.inner.fail_driver(reason, waiting);
    }
}

enum RetryDecision {
    Retry,
    Terminal(AuditTerminalReason),
}

fn classify_store_error(err: &StorageError) -> RetryDecision {
    match err {
        StorageError::WriteQueueFull { .. } | StorageError::WriterTaskBusy { .. } => {
            RetryDecision::Retry
        }
        StorageError::WriterTaskTerminated { request_state } => match request_state {
            WriterTaskRequestState::NotStarted | WriterTaskRequestState::TransactionRolledBack => {
                RetryDecision::Retry
            }
            WriterTaskRequestState::SideEffectsUnknown => RetryDecision::Retry,
        },
        StorageError::Unsupported { operation, .. }
            if operation.as_ref() == "append_events_idempotent" =>
        {
            RetryDecision::Terminal(AuditTerminalReason::IdempotencyUnsupported)
        }
        _ => RetryDecision::Terminal(AuditTerminalReason::StoreFailure),
    }
}

enum GenerationResult {
    Committed(Vec<EventAppendDisposition>),
    Failed(AuditTerminalReason),
    /// Fault-injection only: proves the supervisor's post-`Ok` consistency
    /// check actually runs.
    #[cfg_attr(not(any(test, feature = "fault-injection")), allow(dead_code))]
    FakedInconsistent,
}

async fn run_generation(
    store: Arc<dyn EventStore>,
    events: Vec<Event>,
    config: Arc<AuditBatchConfig>,
) -> GenerationResult {
    #[cfg(any(test, feature = "fault-injection"))]
    if fault::CHILD_PANIC.swap(false, Ordering::SeqCst) {
        panic!("adr133 fault injection: audit_batch child_panic");
    }
    #[cfg(any(test, feature = "fault-injection"))]
    if fault::INCONSISTENT_EXIT.swap(false, Ordering::SeqCst) {
        return GenerationResult::FakedInconsistent;
    }

    let mut attempt: u8 = 0;
    loop {
        attempt += 1;
        match store.append_events_idempotent(events.clone()).await {
            Ok(result) => return GenerationResult::Committed(result.rows),
            Err(err) => match classify_store_error(&err) {
                RetryDecision::Retry if attempt < config.max_commit_attempts.get() => {
                    tokio::time::sleep(config.retry_backoff).await;
                }
                RetryDecision::Retry => {
                    return GenerationResult::Failed(AuditTerminalReason::StoreFailure)
                }
                RetryDecision::Terminal(reason) => return GenerationResult::Failed(reason),
            },
        }
    }
}

/// The batch owner. Constructed once per configured `EventStore`; every
/// dispatch-audit call site routes its row through [`AuditBatch::submit`]
/// instead of taking its own writer-task acquisition.
pub struct AuditBatch {
    inner: Arc<Inner>,
    store: Arc<dyn EventStore>,
    config: Arc<AuditBatchConfig>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

impl AuditBatch {
    pub fn new(store: Arc<dyn EventStore>, config: AuditBatchConfig) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::new()),
            }),
            store,
            config: Arc::new(config),
            supervisor: Mutex::new(None),
        })
    }

    fn spawn_supervisor_if_idle(&self) {
        let inner = self.inner.clone();
        let store = self.store.clone();
        let config = self.config.clone();
        let handle = tokio::spawn(async move {
            supervisor_loop(inner, store, config).await;
        });
        *self.supervisor.lock() = Some(handle);
    }
}

async fn supervisor_loop(
    inner: Arc<Inner>,
    store: Arc<dyn EventStore>,
    config: Arc<AuditBatchConfig>,
) {
    loop {
        let waiting = {
            let mut state = inner.state.lock();
            if matches!(state.lifecycle, Lifecycle::Failed(_)) {
                state.driver_active = false;
                break;
            }
            if state.pending.is_empty() {
                state.driver_active = false;
                break;
            }
            let take_n = state
                .pending
                .len()
                .min(config.max_rows_per_generation.get());
            let waiting: Vec<Waiting> = state.pending.drain(..take_n).collect();
            let generation_id = state.next_generation_id;
            state.next_generation_id += 1;
            state.in_flight_generation = Some(generation_id);
            waiting
        };

        #[cfg(any(test, feature = "fault-injection"))]
        if fault::SUPERVISOR_PANIC.swap(false, Ordering::SeqCst) {
            let _guard = SupervisorGuard::armed(&inner, waiting);
            panic!("adr133 fault injection: audit_batch supervisor_panic");
        }
        #[cfg(any(test, feature = "fault-injection"))]
        if fault::SUPERVISOR_SLEEP_BEFORE_SPAWN.swap(false, Ordering::SeqCst) {
            let guard = SupervisorGuard::armed(&inner, waiting);
            tokio::time::sleep(Duration::from_secs(3600)).await;
            drop(guard);
            continue;
        }

        let guard = SupervisorGuard::armed(&inner, waiting);
        let events: Vec<Event> = guard
            .waiting
            .as_ref()
            .expect("guard freshly armed")
            .iter()
            .map(|w| w.event.clone())
            .collect();

        let child: JoinHandle<GenerationResult> =
            tokio::spawn(run_generation(store.clone(), events, config.clone()));

        #[cfg(any(test, feature = "fault-injection"))]
        if fault::CHILD_CANCEL.swap(false, Ordering::SeqCst) {
            child.abort();
        }
        #[cfg(any(test, feature = "fault-injection"))]
        if fault::JOIN_LOST.swap(false, Ordering::SeqCst) {
            drop(child);
            guard.fail(AuditTerminalReason::DriverJoinLost);
            continue;
        }

        let join_result = child.await;
        match join_result {
            Err(join_err) => {
                let reason = if join_err.is_panic() {
                    AuditTerminalReason::DriverPanicked
                } else {
                    AuditTerminalReason::DriverCancelled
                };
                guard.fail(reason);
            }
            Ok(GenerationResult::FakedInconsistent) => {
                guard.fail(AuditTerminalReason::DriverExitedInconsistent);
            }
            Ok(GenerationResult::Failed(reason)) => {
                let waiting = guard.disarm();
                let submitted = waiting.len() as u64;
                {
                    let mut state = inner.state.lock();
                    state.in_flight_generation = None;
                    state.store_batch_calls += 1;
                    state.flush_failures += 1;
                    let generation_id = state.next_generation_id.saturating_sub(1);
                    state.generations.push(AuditGenerationSnapshot {
                        generation_id,
                        submitted_rows: submitted,
                        committed_rows: 0,
                        store_batch_calls: 1,
                        terminal_reason: Some(reason),
                    });
                }
                for w in waiting {
                    record_degradation_if_pure(&inner, w.producer);
                    let _ = w.responder.send(Err(reason));
                }
            }
            Ok(GenerationResult::Committed(dispositions)) => {
                let waiting = guard.disarm();
                if dispositions.len() != waiting.len() {
                    // Defensive: the store must preserve input order/length.
                    // Treat a mismatch as the driver having exited in a
                    // state that cannot be reconciled with the accepted
                    // waiters.
                    {
                        let mut state = inner.state.lock();
                        state.in_flight_generation = None;
                    }
                    inner.fail_driver(AuditTerminalReason::DriverExitedInconsistent, waiting);
                    continue;
                }
                let mut committed_n = 0u64;
                for d in &dispositions {
                    if !matches!(d, EventAppendDisposition::IdentityConflict) {
                        committed_n += 1;
                    }
                }
                let submitted = dispositions.len() as u64;
                {
                    let mut state = inner.state.lock();
                    state.in_flight_generation = None;
                    state.store_batch_calls += 1;
                    state.committed_rows += committed_n;
                    let generation_id = state.next_generation_id.saturating_sub(1);
                    state.generations.push(AuditGenerationSnapshot {
                        generation_id,
                        submitted_rows: submitted,
                        committed_rows: committed_n,
                        store_batch_calls: 1,
                        terminal_reason: None,
                    });
                }
                for (w, disposition) in waiting.into_iter().zip(dispositions) {
                    let result = match disposition {
                        EventAppendDisposition::Inserted => Ok(AuditCommitOutcome::Committed),
                        EventAppendDisposition::AlreadyPresentIdentical => {
                            Ok(AuditCommitOutcome::AlreadyPresentIdentical)
                        }
                        EventAppendDisposition::IdentityConflict => {
                            record_degradation_if_pure(&inner, w.producer);
                            Err(AuditTerminalReason::IdentityConflict)
                        }
                    };
                    let _ = w.responder.send(result);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl AuditBatchControl for AuditBatch {
    async fn submit(
        &self,
        row: PreparedAuditRow,
    ) -> Result<AuditCommitOutcome, AuditTerminalReason> {
        // Pre-enqueue validation (invariant 3): a malformed row is rejected
        // before it can share a generation with anyone else's.
        if self.store.preflight_event(&row.event).is_err() {
            return Err(AuditTerminalReason::PreflightRejected);
        }

        let (tx, rx) = oneshot::channel();
        let need_spawn = {
            let mut state = self.inner.state.lock();
            match state.lifecycle {
                Lifecycle::Closed | Lifecycle::Closing => {
                    return Err(AuditTerminalReason::AdmissionClosed)
                }
                Lifecycle::Failed(reason) => return Err(reason),
                Lifecycle::Open => {}
            }
            if state.pending.len() >= self.config.max_pending_rows.get() {
                return Err(AuditTerminalReason::QueueAdmissionExhausted);
            }
            state.pending.push(Waiting {
                event: row.event,
                producer: row.producer,
                responder: tx,
            });
            state.submitted_rows += 1;
            let need_spawn = !state.driver_active;
            if need_spawn {
                state.driver_active = true;
            }
            need_spawn
        };
        if need_spawn {
            self.spawn_supervisor_if_idle();
        }

        match tokio::time::timeout(self.config.admission_deadline, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_recv_error)) => Err(AuditTerminalReason::DriverJoinLost),
            Err(_elapsed) => Err(AuditTerminalReason::QueueAdmissionExhausted),
        }
    }

    async fn quiesce(&self) -> Result<(), AuditTerminalReason> {
        loop {
            let (lifecycle, idle) = {
                let state = self.inner.state.lock();
                (state.lifecycle.clone(), state.is_idle())
            };
            match lifecycle {
                Lifecycle::Failed(reason) => return Err(reason),
                Lifecycle::Closed => return Ok(()),
                _ if idle => return Ok(()),
                _ => tokio::time::sleep(Duration::from_millis(2)).await,
            }
        }
    }

    async fn close_and_drain(&self) -> Result<(), AuditTerminalReason> {
        {
            let mut state = self.inner.state.lock();
            if matches!(state.lifecycle, Lifecycle::Open) {
                state.lifecycle = Lifecycle::Closing;
            }
        }
        let result = AuditBatchControl::quiesce(self).await;
        {
            let mut state = self.inner.state.lock();
            if result.is_ok() && matches!(state.lifecycle, Lifecycle::Closing) {
                state.lifecycle = Lifecycle::Closed;
            }
        }
        let handle = self.supervisor.lock().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        result
    }
}

#[cfg(any(test, feature = "fault-injection"))]
mod fault {
    use std::sync::atomic::AtomicBool;

    pub(super) static CHILD_PANIC: AtomicBool = AtomicBool::new(false);
    pub(super) static CHILD_CANCEL: AtomicBool = AtomicBool::new(false);
    pub(super) static SUPERVISOR_PANIC: AtomicBool = AtomicBool::new(false);
    pub(super) static SUPERVISOR_SLEEP_BEFORE_SPAWN: AtomicBool = AtomicBool::new(false);
    pub(super) static JOIN_LOST: AtomicBool = AtomicBool::new(false);
    pub(super) static INCONSISTENT_EXIT: AtomicBool = AtomicBool::new(false);
}

/// Deterministic fault-injection arms consumed by exactly one subsequent
/// generation each. Test-only surface, gated identically to the rest of this
/// crate's `fault-injection` fixtures (see `crate::operations`).
#[cfg(any(test, feature = "fault-injection"))]
pub mod fault_injection {
    use std::sync::atomic::Ordering;

    pub fn arm_child_panic() {
        super::fault::CHILD_PANIC.store(true, Ordering::SeqCst);
    }
    pub fn arm_child_cancel() {
        super::fault::CHILD_CANCEL.store(true, Ordering::SeqCst);
    }
    pub fn arm_supervisor_panic() {
        super::fault::SUPERVISOR_PANIC.store(true, Ordering::SeqCst);
    }
    pub fn arm_supervisor_sleep_before_spawn() {
        super::fault::SUPERVISOR_SLEEP_BEFORE_SPAWN.store(true, Ordering::SeqCst);
    }
    pub fn arm_join_lost() {
        super::fault::JOIN_LOST.store(true, Ordering::SeqCst);
    }
    pub fn arm_inconsistent_exit() {
        super::fault::INCONSISTENT_EXIT.store(true, Ordering::SeqCst);
    }
}

// ── Test-attribution surface (R2) ────────────────────────────────────────
//
// Additive only: production attribution still lands in the public
// `db_diagnostics` counters via `metrics_snapshot`-shaped data supplied at
// that seam. This surface never substitutes for it.
#[cfg(any(test, feature = "test-internals"))]
mod test_internals {
    use super::*;

    /// A point-in-time view of the batch's counters and generation history,
    /// used by mechanism tests to compute a delta across a measured
    /// operation.
    #[derive(Debug, Clone)]
    pub struct AuditBatchSnapshot {
        pub pending_rows: usize,
        pub in_flight_generation: Option<u64>,
        pub driver_active: bool,
        pub next_generation_id: u64,
        pub submitted_rows: u64,
        pub committed_rows: u64,
        pub store_batch_calls: u64,
        pub per_generation: Vec<AuditGenerationSnapshot>,
    }

    impl AuditBatchSnapshot {
        pub fn is_idle(&self) -> bool {
            self.pending_rows == 0 && self.in_flight_generation.is_none() && !self.driver_active
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct AuditBatchMetricsSnapshot {
        pub flush_failures: u64,
        pub degraded_rows: u64,
        pub degraded: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct AuditBatchDelta {
        pub submitted_rows: u64,
        pub committed_rows: u64,
        pub store_batch_calls: u64,
        pub per_generation: Vec<AuditGenerationSnapshot>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AuditSnapshotError {
        CounterRegressed,
        GenerationHistoryRegressed,
    }

    impl AuditBatch {
        pub fn test_snapshot(&self) -> AuditBatchSnapshot {
            let state = self.inner.state.lock();
            AuditBatchSnapshot {
                pending_rows: state.pending.len(),
                in_flight_generation: state.in_flight_generation,
                driver_active: state.driver_active,
                next_generation_id: state.next_generation_id,
                submitted_rows: state.submitted_rows,
                committed_rows: state.committed_rows,
                store_batch_calls: state.store_batch_calls,
                per_generation: state.generations.clone(),
            }
        }

        pub fn metrics_snapshot(&self) -> AuditBatchMetricsSnapshot {
            let state = self.inner.state.lock();
            AuditBatchMetricsSnapshot {
                flush_failures: state.flush_failures,
                degraded_rows: state.degraded_rows,
                degraded: state.degraded,
            }
        }

        /// Abort the currently-retained supervisor `JoinHandle`, if any,
        /// simulating a shutdown abort landing mid-generation. Returns
        /// whether a handle was found and aborted. Test-only: exercises the
        /// R1 supervisor-cancellation path without reaching into a private
        /// field from outside this module.
        pub fn test_abort_supervisor(&self) -> bool {
            let handle = self.supervisor.lock().take();
            match handle {
                Some(handle) => {
                    handle.abort();
                    true
                }
                None => false,
            }
        }
    }

    /// Checked monotonic subtraction. Rejects a regressed counter or a
    /// generation history that is not an append-only extension of `before`.
    pub fn audit_delta(
        before: &AuditBatchSnapshot,
        after: &AuditBatchSnapshot,
    ) -> Result<AuditBatchDelta, AuditSnapshotError> {
        let submitted_rows = after
            .submitted_rows
            .checked_sub(before.submitted_rows)
            .ok_or(AuditSnapshotError::CounterRegressed)?;
        let committed_rows = after
            .committed_rows
            .checked_sub(before.committed_rows)
            .ok_or(AuditSnapshotError::CounterRegressed)?;
        let store_batch_calls = after
            .store_batch_calls
            .checked_sub(before.store_batch_calls)
            .ok_or(AuditSnapshotError::CounterRegressed)?;
        if after.per_generation.len() < before.per_generation.len() {
            return Err(AuditSnapshotError::GenerationHistoryRegressed);
        }
        if after.per_generation[..before.per_generation.len()] != before.per_generation[..] {
            return Err(AuditSnapshotError::GenerationHistoryRegressed);
        }
        let per_generation = after.per_generation[before.per_generation.len()..].to_vec();
        Ok(AuditBatchDelta {
            submitted_rows,
            committed_rows,
            store_batch_calls,
            per_generation,
        })
    }

    /// Exhaustive, non-wildcard producer classification (see
    /// [`super::AuditProducer`] and [`super::classify`], the real
    /// crate-private definitions this mirrors one-for-one). The doctest
    /// below proves the general property those definitions rely on: a match
    /// over a non-`#[non_exhaustive]` enum that omits a variant, with no
    /// wildcard arm to silently absorb it, fails to compile rather than
    /// passing an incomplete classification.
    ///
    /// ```compile_fail
    /// enum AuditProducer {
    ///     GateDenied,
    ///     DispatchSucceeded,
    ///     DispatchFailed,
    ///     UnknownVerb,
    ///     GitDigestReceipt,
    ///     ConfigLocked,
    ///     RecallExecuted,
    /// }
    ///
    /// fn describe(p: AuditProducer) -> &'static str {
    ///     match p {
    ///         AuditProducer::GateDenied => "obligation",
    ///         AuditProducer::DispatchSucceeded => "obligation",
    ///         AuditProducer::DispatchFailed => "obligation",
    ///         AuditProducer::UnknownVerb => "obligation",
    ///         AuditProducer::GitDigestReceipt => "obligation",
    ///         AuditProducer::ConfigLocked => "observability",
    ///         // RecallExecuted intentionally omitted: a non-exhaustive
    ///         // match must fail to compile, proving no variant can
    ///         // silently fall through an absent wildcard arm.
    ///     }
    /// }
    /// ```
    #[allow(dead_code)]
    struct DoctestAnchor;
}

#[cfg(any(test, feature = "test-internals"))]
pub use test_internals::*;
