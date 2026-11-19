//! Backend-neutral request cancellation and deadline propagation.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant as WallInstant};

use crate::{StorageError, StorageResult};

const SEARCH_PHASES: [SearchPhase; 25] = [
    SearchPhase::Authorization,
    SearchPhase::TextSearch,
    SearchPhase::VectorSearch,
    SearchPhase::VectorEmbedding,
    SearchPhase::Fusion,
    SearchPhase::EntityHydration,
    SearchPhase::NoteHydration,
    SearchPhase::SupersededFilter,
    SearchPhase::MailboxScopeHydration,
    SearchPhase::MailboxAuthorization,
    SearchPhase::TextReaderAdmission,
    SearchPhase::TextReaderCheckout,
    SearchPhase::TextBlockingRead,
    SearchPhase::VectorReaderAdmission,
    SearchPhase::VectorReaderCheckout,
    SearchPhase::VectorBlockingRead,
    SearchPhase::EntityReaderAdmission,
    SearchPhase::EntityReaderCheckout,
    SearchPhase::EntityBlockingRead,
    SearchPhase::NoteReaderAdmission,
    SearchPhase::NoteReaderCheckout,
    SearchPhase::NoteBlockingRead,
    SearchPhase::GraphReaderAdmission,
    SearchPhase::GraphReaderCheckout,
    SearchPhase::GraphBlockingRead,
];

/// A stage of one backend search, named in the deadline warning when that search times out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum SearchPhase {
    Authorization,
    TextSearch,
    VectorSearch,
    VectorEmbedding,
    Fusion,
    EntityHydration,
    NoteHydration,
    SupersededFilter,
    MailboxScopeHydration,
    MailboxAuthorization,
    TextReaderAdmission,
    TextReaderCheckout,
    TextBlockingRead,
    VectorReaderAdmission,
    VectorReaderCheckout,
    VectorBlockingRead,
    EntityReaderAdmission,
    EntityReaderCheckout,
    EntityBlockingRead,
    NoteReaderAdmission,
    NoteReaderCheckout,
    NoteBlockingRead,
    GraphReaderAdmission,
    GraphReaderCheckout,
    GraphBlockingRead,
}

impl SearchPhase {
    /// The phase name written in log fields.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authorization => "authorization",
            Self::TextSearch => "text_search",
            Self::VectorSearch => "vector_search",
            Self::VectorEmbedding => "vector_embedding",
            Self::Fusion => "fusion",
            Self::EntityHydration => "entity_hydration",
            Self::NoteHydration => "note_hydration",
            Self::SupersededFilter => "superseded_filter",
            Self::MailboxScopeHydration => "mailbox_scope_hydration",
            Self::MailboxAuthorization => "mailbox_authorization",
            Self::TextReaderAdmission => "text_reader_admission",
            Self::TextReaderCheckout => "text_reader_checkout",
            Self::TextBlockingRead => "text_blocking_read",
            Self::VectorReaderAdmission => "vector_reader_admission",
            Self::VectorReaderCheckout => "vector_reader_checkout",
            Self::VectorBlockingRead => "vector_blocking_read",
            Self::EntityReaderAdmission => "entity_reader_admission",
            Self::EntityReaderCheckout => "entity_reader_checkout",
            Self::EntityBlockingRead => "entity_blocking_read",
            Self::NoteReaderAdmission => "note_reader_admission",
            Self::NoteReaderCheckout => "note_reader_checkout",
            Self::NoteBlockingRead => "note_blocking_read",
            Self::GraphReaderAdmission => "graph_reader_admission",
            Self::GraphReaderCheckout => "graph_reader_checkout",
            Self::GraphBlockingRead => "graph_blocking_read",
        }
    }
}

struct SearchPhaseTraceInner {
    origin: std::time::Instant,
    active_since_ns: [AtomicU64; SEARCH_PHASES.len()],
    completed_ns: [AtomicU64; SEARCH_PHASES.len()],
    completed_order: [AtomicU64; SEARCH_PHASES.len()],
    entered: AtomicU64,
    next_completion: AtomicU64,
}

/// Which phases one backend search has entered and finished, shared by the search and the code
/// that reports its deadline. Marking a phase only touches atomics, so a reader never waits on a
/// stalled phase.
#[derive(Clone)]
pub struct SearchPhaseTrace(Arc<SearchPhaseTraceInner>);

/// Keeps one phase active until dropped; dropping it adds the elapsed time to that phase.
pub struct SearchPhaseGuard {
    trace: SearchPhaseTrace,
    phase: SearchPhase,
    started_ns: u64,
}

/// A trace read at one instant, formatted for the deadline warning fields.
#[derive(Clone)]
pub struct SearchPhaseSnapshot {
    pub in_flight_phase: String,
    pub phase_elapsed_ms: u64,
    pub active_phases: String,
    pub completed_phases: String,
}

tokio::task_local! {
    static SEARCH_PHASE_TRACE: SearchPhaseTrace;
}

impl SearchPhaseTrace {
    /// An empty trace with no phase entered.
    pub fn new() -> Self {
        Self(Arc::new(SearchPhaseTraceInner {
            origin: std::time::Instant::now(),
            active_since_ns: std::array::from_fn(|_| AtomicU64::new(0)),
            completed_ns: std::array::from_fn(|_| AtomicU64::new(0)),
            completed_order: std::array::from_fn(|_| AtomicU64::new(0)),
            entered: AtomicU64::new(0),
            next_completion: AtomicU64::new(0),
        }))
    }

    /// Mark `phase` active from now until the returned guard drops. A phase entered again before
    /// its earlier guard drops reports the later entry.
    pub fn enter(&self, phase: SearchPhase) -> SearchPhaseGuard {
        let started_ns = self.elapsed_ns();
        let index = phase as usize;
        self.0.active_since_ns[index].store(started_ns.saturating_add(1), Ordering::Release);
        self.0.entered.fetch_or(1u64 << index, Ordering::Relaxed);
        SearchPhaseGuard {
            trace: self.clone(),
            phase,
            started_ns,
        }
    }

    /// The most recently entered active phase and its time so far, every active phase, and the
    /// finished phases in completion order with their total time.
    pub fn snapshot(&self) -> SearchPhaseSnapshot {
        let now_ns = self.elapsed_ns();
        let mut active = Vec::new();
        let mut completed = Vec::new();
        for phase in SEARCH_PHASES {
            let index = phase as usize;
            let active_since = self.0.active_since_ns[index].load(Ordering::Acquire);
            if active_since != 0 {
                active.push((active_since - 1, phase));
            }
            let order = self.0.completed_order[index].load(Ordering::Acquire);
            if order != 0 {
                completed.push((
                    order,
                    phase,
                    self.0.completed_ns[index].load(Ordering::Relaxed),
                ));
            }
        }
        active.sort_unstable_by_key(|(started_ns, _)| *started_ns);
        completed.sort_unstable_by_key(|(order, _, _)| *order);

        let (in_flight_phase, phase_elapsed_ms) = match active.last() {
            Some((started_ns, phase)) => (
                phase.as_str().to_string(),
                nanos_to_millis(now_ns.saturating_sub(*started_ns)),
            ),
            None if self.0.entered.load(Ordering::Relaxed) == 0 => ("not_entered".into(), 0),
            None => ("between_phases".into(), 0),
        };
        let active_phases = if active.is_empty() {
            "none".to_string()
        } else {
            active
                .iter()
                .map(|(started_ns, phase)| {
                    format!(
                        "{}={}ms",
                        phase.as_str(),
                        nanos_to_millis(now_ns.saturating_sub(*started_ns))
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        let completed_phases = if completed.is_empty() {
            "none".to_string()
        } else {
            completed
                .iter()
                .map(|(_, phase, elapsed_ns)| {
                    format!("{}={}ms", phase.as_str(), nanos_to_millis(*elapsed_ns))
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        SearchPhaseSnapshot {
            in_flight_phase,
            phase_elapsed_ms,
            active_phases,
            completed_phases,
        }
    }

    fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.0.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

impl Default for SearchPhaseTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SearchPhaseGuard {
    fn drop(&mut self) {
        let index = self.phase as usize;
        let elapsed_ns = self.trace.elapsed_ns().saturating_sub(self.started_ns);
        self.trace.0.completed_ns[index].fetch_add(elapsed_ns, Ordering::Relaxed);
        let order = self.trace.0.next_completion.fetch_add(1, Ordering::Relaxed) + 1;
        self.trace.0.completed_order[index].store(order, Ordering::Release);
        self.trace.0.active_since_ns[index].store(0, Ordering::Release);
    }
}

fn nanos_to_millis(nanos: u64) -> u64 {
    nanos / 1_000_000
}

/// Run `future` with `trace` as the current task's search phase trace.
pub async fn scope_search_phase_trace<F: Future>(trace: SearchPhaseTrace, future: F) -> F::Output {
    SEARCH_PHASE_TRACE.scope(trace, future).await
}

/// Mark `phase` on the current task's trace; `None` when no trace is in scope.
pub fn enter_search_phase(phase: SearchPhase) -> Option<SearchPhaseGuard> {
    SEARCH_PHASE_TRACE.try_with(|trace| trace.enter(phase)).ok()
}

/// The current task's trace, for work that runs on another thread.
pub fn capture_search_phase_trace() -> Option<SearchPhaseTrace> {
    SEARCH_PHASE_TRACE.try_with(Clone::clone).ok()
}

/// Default nonzero ceiling for request-owned read work, including compose.
pub const DEFAULT_REQUEST_READ_TIMEOUT_SECS: u64 = 30;

/// Largest accepted request-read ceiling: one day, so a trusted local batch
/// (a whole-archive ingest, a long serial model run) can hold one deadline.
pub const MAX_REQUEST_READ_TIMEOUT_SECS: u64 = 86_400;

/// Resolve the operator-visible request-read ceiling from
/// `KHIVE_REQUEST_READ_TIMEOUT_SECS`.
///
/// Unset uses the default. A value above [`MAX_REQUEST_READ_TIMEOUT_SECS`] is
/// clamped to it; zero or a non-integer uses the default, so the guard is never
/// disabled. Either correction logs a warning the first time it is made in a
/// process, naming the value that was set and the ceiling in force.
pub fn request_read_timeout_from_env() -> Duration {
    let raw = std::env::var("KHIVE_REQUEST_READ_TIMEOUT_SECS").ok();
    let (seconds, correction) = resolve_request_read_timeout_secs(raw.as_deref());
    if let Some(correction) = correction {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                value = raw.as_deref().unwrap_or_default(),
                ceiling_secs = seconds,
                max_secs = MAX_REQUEST_READ_TIMEOUT_SECS,
                "KHIVE_REQUEST_READ_TIMEOUT_SECS {correction}"
            );
        });
    }
    Duration::from_secs(seconds)
}

/// The ceiling in seconds for a raw setting, and the correction applied when
/// the setting was not used as written.
fn resolve_request_read_timeout_secs(raw: Option<&str>) -> (u64, Option<&'static str>) {
    let Some(raw) = raw else {
        return (DEFAULT_REQUEST_READ_TIMEOUT_SECS, None);
    };
    match raw.trim().parse::<u64>() {
        Ok(seconds) if (1..=MAX_REQUEST_READ_TIMEOUT_SECS).contains(&seconds) => (seconds, None),
        Ok(seconds) if seconds > MAX_REQUEST_READ_TIMEOUT_SECS => (
            MAX_REQUEST_READ_TIMEOUT_SECS,
            Some("is above the maximum; clamped to the maximum"),
        ),
        _ => (
            DEFAULT_REQUEST_READ_TIMEOUT_SECS,
            Some("is not a whole number of seconds from 1 to the maximum; using the default"),
        ),
    }
}

/// One absolute request deadline represented on async and blocking clocks.
///
/// Tokio's instant is authoritative for async timeout selection, including
/// paused-clock tests. The wall instant lets blocking backends enforce the
/// same non-renewing deadline without depending on Tokio's runtime clock.
#[derive(Clone, Copy, Debug)]
pub struct RequestReadDeadline {
    async_at: tokio::time::Instant,
    blocking_at: WallInstant,
}

impl RequestReadDeadline {
    /// Create a deadline `duration` from now.
    pub fn after(duration: Duration) -> Self {
        Self {
            async_at: tokio::time::Instant::now() + duration,
            blocking_at: WallInstant::now() + duration,
        }
    }

    /// Tokio-clock instant used by request coordinators and timeout selection.
    pub fn async_at(self) -> tokio::time::Instant {
        self.async_at
    }

    /// Wall-clock instant used by blocking backend cancellation checks.
    pub fn blocking_at(self) -> WallInstant {
        self.blocking_at
    }

    fn earlier(self, other: Self) -> Self {
        if self.async_at <= other.async_at {
            self
        } else {
            other
        }
    }
}

/// The first request-level condition that stopped read-only work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestReadStopReason {
    /// An explicit cancellation signal fired or its sender disappeared.
    Cancelled,
    /// The request's absolute deadline elapsed.
    Deadline,
}

/// Opaque snapshot of the current request read context.
///
/// Backend implementations capture this once before crossing into blocking
/// work so nested scopes and spawned children share one absolute deadline.
#[derive(Clone, Default)]
pub struct RequestReadContext {
    cancellations: Arc<[tokio::sync::watch::Receiver<bool>]>,
    deadline: Option<RequestReadDeadline>,
    store_acquisition_operation: Option<&'static str>,
}

impl RequestReadContext {
    /// Opt one blocking store-constructor call into request-aware admission.
    /// Ordinary accessors under a read context remain outside this scope.
    /// The previous context is restored even if the constructor panics.
    pub fn scope_store_acquisition<T>(
        mut self,
        operation: &'static str,
        work: impl FnOnce() -> T,
    ) -> T {
        self.store_acquisition_operation = Some(operation);
        REQUEST_READ_CONTEXT.sync_scope(self, work)
    }

    /// The explicitly scoped constructor operation, absent for ordinary callers.
    pub fn store_acquisition_operation(&self) -> Option<&'static str> {
        self.store_acquisition_operation
    }

    /// Observe cancellation and the original wall-clock deadline in blocking work.
    pub fn blocking_stop_reason(&self) -> Option<RequestReadStopReason> {
        if self.cancellations.iter().any(receiver_cancelled) {
            Some(RequestReadStopReason::Cancelled)
        } else if self
            .deadline
            .is_some_and(|deadline| WallInstant::now() >= deadline.blocking_at)
        {
            Some(RequestReadStopReason::Deadline)
        } else {
            None
        }
    }

    /// Return the request's absolute deadline, when one is installed.
    pub fn deadline(&self) -> Option<RequestReadDeadline> {
        self.deadline
    }

    /// Return the currently observed stop cause without waiting.
    pub fn stop_reason(&self) -> Option<RequestReadStopReason> {
        if self.cancellations.iter().any(receiver_cancelled) {
            Some(RequestReadStopReason::Cancelled)
        } else if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at)
        {
            Some(RequestReadStopReason::Deadline)
        } else {
            None
        }
    }

    /// Wait until one merged cancellation source fires or the deadline elapses.
    pub async fn wait_for_stop(self) -> RequestReadStopReason {
        let cancellation_receivers = self.cancellations;
        let request_deadline = self.deadline;
        let cancellation = async move {
            match cancellation_receivers.len() {
                0 => std::future::pending::<()>().await,
                1 => wait_for_receiver_cancellation(cancellation_receivers[0].clone()).await,
                2 => {
                    tokio::select! {
                        _ = wait_for_receiver_cancellation(cancellation_receivers[0].clone()) => {},
                        _ = wait_for_receiver_cancellation(cancellation_receivers[1].clone()) => {},
                    }
                }
                _ => wait_for_receiver_set(cancellation_receivers).await,
            }
        };
        let deadline = async move {
            match request_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.async_at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = cancellation => RequestReadStopReason::Cancelled,
            _ = deadline => RequestReadStopReason::Deadline,
        }
    }
}

tokio::task_local! {
    static REQUEST_READ_CONTEXT: RequestReadContext;
}

/// Capture the current request context for a backend operation.
pub fn capture_request_read_context() -> RequestReadContext {
    REQUEST_READ_CONTEXT
        .try_with(Clone::clone)
        .unwrap_or_default()
}

/// Constrain `candidate` by the current request's deadline without installing
/// a new scope.
pub fn effective_request_read_deadline(candidate: RequestReadDeadline) -> RequestReadDeadline {
    capture_request_read_context()
        .deadline
        .map_or(candidate, |existing| existing.earlier(candidate))
}

/// Scope `future` to an explicit read-cancellation signal.
///
/// Backends observe the signal only for work they classify as read-only.
pub async fn scope_request_read_cancellation<F>(
    cancellation: tokio::sync::watch::Receiver<bool>,
    future: F,
) -> F::Output
where
    F: Future,
{
    let mut context = capture_request_read_context();
    let mut cancellations = Vec::with_capacity(context.cancellations.len() + 1);
    cancellations.extend(context.cancellations.iter().cloned());
    cancellations.push(cancellation);
    context.cancellations = cancellations.into();
    REQUEST_READ_CONTEXT.scope(context, future).await
}

/// Scope `future` to one relative read deadline, preserving an earlier outer
/// deadline instead of renewing it.
pub async fn scope_request_read_deadline<F>(duration: Duration, future: F) -> F::Output
where
    F: Future,
{
    scope_request_read_deadline_at(RequestReadDeadline::after(duration), future).await
}

/// Scope `future` to an already-created absolute read deadline.
pub async fn scope_request_read_deadline_at<F>(
    deadline: RequestReadDeadline,
    future: F,
) -> F::Output
where
    F: Future,
{
    let mut context = capture_request_read_context();
    context.deadline = Some(match context.deadline {
        Some(existing) => existing.earlier(deadline),
        None => deadline,
    });
    REQUEST_READ_CONTEXT.scope(context, future).await
}

/// Capture the current request read context for a spawned child future.
pub fn inherit_request_read_context<F>(future: F) -> impl Future<Output = F::Output> + Send
where
    F: Future + Send,
{
    let inherited = REQUEST_READ_CONTEXT.try_with(Clone::clone).ok();
    async move {
        match inherited {
            Some(context) => REQUEST_READ_CONTEXT.scope(context, future).await,
            None => future.await,
        }
    }
}

/// Intention-revealing alias for cancellation-only child tasks.
pub fn inherit_request_read_cancellation<F>(future: F) -> impl Future<Output = F::Output> + Send
where
    F: Future + Send,
{
    inherit_request_read_context(future)
}

/// Return whether the current request has stopped its read-only work.
pub fn request_read_is_cancelled() -> bool {
    capture_request_read_context().stop_reason().is_some()
}

/// Refuse to begin another request-owned read phase after cancellation.
pub fn ensure_request_read_active(operation: &'static str) -> StorageResult<()> {
    if request_read_is_cancelled() {
        Err(StorageError::Timeout {
            operation: operation.into(),
        })
    } else {
        Ok(())
    }
}

/// Wait until the current request's read-only work is cancelled or timed out.
pub async fn wait_for_request_read_cancellation() {
    let _ = capture_request_read_context().wait_for_stop().await;
}

/// Await one request-owned read phase under the current absolute deadline.
pub async fn await_request_read_phase<F>(
    operation: &'static str,
    future: F,
) -> StorageResult<F::Output>
where
    F: Future,
{
    ensure_request_read_active(operation)?;
    tokio::pin!(future);
    tokio::select! {
        biased;
        _ = wait_for_request_read_cancellation() => Err(StorageError::Timeout {
            operation: operation.into(),
        }),
        output = &mut future => {
            ensure_request_read_active(operation)?;
            Ok(output)
        }
    }
}

fn receiver_cancelled(receiver: &tokio::sync::watch::Receiver<bool>) -> bool {
    *receiver.borrow() || receiver.has_changed().is_err()
}

async fn wait_for_receiver_cancellation(mut receiver: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *receiver.borrow_and_update() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_for_receiver_set(receivers: Arc<[tokio::sync::watch::Receiver<bool>]>) {
    let mut waits: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = receivers
        .iter()
        .cloned()
        .map(|receiver| {
            Box::pin(wait_for_receiver_cancellation(receiver))
                as Pin<Box<dyn Future<Output = ()> + Send>>
        })
        .collect();
    std::future::poll_fn(move |cx| {
        if waits
            .iter_mut()
            .any(|wait| wait.as_mut().poll(cx).is_ready())
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

#[cfg(test)]
#[path = "request_context/tests.rs"]
mod tests;
