//! Class admission and isolated execution for explicitly attributed background jobs.
//! Interactive dispatch does not enter this module. A job's lease, not its awaiter,
//! owns admission and tracking through the final actual blocking-child exit.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

mod priority;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkClass {
    Warm,
    Maintenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkClassLimits {
    warm: usize,
    maintenance: usize,
}
impl WorkClassLimits {
    pub fn validate(warm: usize, maintenance: usize) -> Result<Self, ResourceError> {
        for (class, value) in [
            (WorkClass::Warm, warm),
            (WorkClass::Maintenance, maintenance),
        ] {
            if value == 0 || value > Semaphore::MAX_PERMITS {
                return Err(ResourceError::InvalidLimit {
                    class,
                    value,
                    max: Semaphore::MAX_PERMITS,
                });
            }
        }
        Ok(Self { warm, maintenance })
    }
    pub fn warm(self) -> usize {
        self.warm
    }
    pub fn maintenance(self) -> usize {
        self.maintenance
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CleanupPolicy {
    grace: Duration,
}
impl CleanupPolicy {
    pub fn new(grace: Duration) -> Self {
        Self { grace }
    }
    pub fn grace(self) -> Duration {
        self.grace
    }
    pub fn deadline_from(self, now: Instant) -> Result<Instant, ResourceError> {
        now.checked_add(self.grace)
            .ok_or(ResourceError::DeadlineOverflow)
    }
}

#[derive(Clone, Debug)]
pub enum PriorityError {
    Unsupported {
        target: &'static str,
    },
    Os {
        operation: &'static str,
        code: Option<i32>,
    },
    Unexpected {
        operation: &'static str,
        value: i32,
    },
}
#[derive(Clone, Debug)]
pub enum ResourceError {
    InvalidLimit {
        class: WorkClass,
        value: usize,
        max: usize,
    },
    DeadlineOverflow,
    Closed,
    WrongDomain,
    Priority(PriorityError),
    ThreadStart(String),
    RuntimeStart(String),
    CoordinatorGone,
    JoinFailed(String),
}
#[derive(Debug)]
pub enum BackgroundJoinError {
    Cancelled,
    Panicked,
    Other(String),
}
macro_rules! display_error {
    ($($ty:ty),+ $(,)?) => {$ (
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{self:?}") }
        }
        impl std::error::Error for $ty {}
    )+ };
}
display_error!(PriorityError, ResourceError, BackgroundJoinError);

#[derive(Debug)]
pub enum BodyOutcome<T> {
    Finished(T),
    Cancelled,
}
#[derive(Debug)]
pub enum JobOutcome<T> {
    Completed(T),
    Cancelled,
    Refused(PriorityError),
    FinalizationTimedOut,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalizationOutcome {
    AttemptReturned,
    AttemptTimedOut,
    NoAttemptBeforeDeadline,
    NoTelemetryToken,
    AlreadyFinalized,
    DeadlineOverflow,
}
#[derive(Clone, Debug)]
pub struct ShutdownReport {
    pub workers_exited: bool,
    pub coordinator_joined: bool,
    pub outstanding_jobs: usize,
    pub incomplete_finalizations: usize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn shorten(slot: &mut Option<Instant>, deadline: Instant) {
    *slot = Some(slot.map_or(deadline, |old| old.min(deadline)));
}
struct StartGate {
    closed: bool,
    deadline: Option<Instant>,
}
struct JoinState {
    coordinator: Option<std::thread::JoinHandle<()>>,
    joining: bool,
    joined: Option<Result<(), ResourceError>>,
}
struct Domain {
    limits: WorkClassLimits,
    cleanup: CleanupPolicy,
    warm: Arc<Semaphore>,
    maintenance: Arc<Semaphore>,
    gate: Mutex<StartGate>,
    changed: watch::Sender<()>,
    supervisors: AtomicUsize,
    jobs: AtomicUsize,
    incomplete: AtomicUsize,
    worker_exit: Mutex<Option<Result<(), ResourceError>>>,
    join: Mutex<JoinState>,
    posture_failure: Mutex<Option<PriorityError>>,
    // Instance-local source-test seams at the real enqueue/publication boundary.
    #[cfg(test)]
    after_blocking_enqueue: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    blocking_dequeued: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}
impl Domain {
    fn new(limits: WorkClassLimits, cleanup: CleanupPolicy) -> Arc<Self> {
        let (changed, _) = watch::channel(());
        Arc::new(Self {
            limits,
            cleanup,
            warm: Arc::new(Semaphore::new(limits.warm)),
            maintenance: Arc::new(Semaphore::new(limits.maintenance)),
            gate: Mutex::new(StartGate {
                closed: false,
                deadline: None,
            }),
            changed,
            supervisors: AtomicUsize::new(0),
            jobs: AtomicUsize::new(0),
            incomplete: AtomicUsize::new(0),
            worker_exit: Mutex::new(None),
            join: Mutex::new(JoinState {
                coordinator: None,
                joining: false,
                joined: None,
            }),
            posture_failure: Mutex::new(None),
            #[cfg(test)]
            after_blocking_enqueue: Mutex::new(None),
            #[cfg(test)]
            blocking_dequeued: Mutex::new(None),
        })
    }
    fn notify(&self) {
        let _ = self.changed.send(());
    }
    fn close(&self, deadline: Instant) {
        {
            let mut gate = lock(&self.gate);
            gate.closed = true;
            shorten(&mut gate.deadline, deadline);
            self.warm.close();
            self.maintenance.close();
        }
        self.notify();
    }
    fn close_with_policy(&self) -> Result<(), ResourceError> {
        let now = Instant::now();
        match self.cleanup.deadline_from(now) {
            Ok(deadline) => {
                self.close(deadline);
                Ok(())
            }
            Err(error) => {
                self.close(now);
                Err(error)
            }
        }
    }
    fn closed_error(&self) -> ResourceError {
        lock(&self.posture_failure)
            .clone()
            .map_or(ResourceError::Closed, ResourceError::Priority)
    }
    fn posture_failed(&self, error: PriorityError) {
        tracing::error!(error = %error, "isolated background worker priority failed; closing domain");
        let mut failure = lock(&self.posture_failure);
        if failure.is_none() {
            *failure = Some(error);
        }
        drop(failure);
        let _ = self.close_with_policy();
    }
    fn report(&self) -> ShutdownReport {
        ShutdownReport {
            workers_exited: lock(&self.worker_exit).is_some(),
            coordinator_joined: lock(&self.join).joined.as_ref().is_some_and(Result::is_ok),
            outstanding_jobs: self.jobs.load(Ordering::SeqCst),
            incomplete_finalizations: self.incomplete.load(Ordering::SeqCst),
        }
    }
    async fn drive_until_closed(&self) {
        let mut changed = self.changed.subscribe();
        loop {
            let (closed, deadline) = {
                let gate = lock(&self.gate);
                (gate.closed, gate.deadline)
            };
            if closed {
                if self.supervisors.load(Ordering::SeqCst) == 0 {
                    return;
                }
                let remaining = deadline
                    .unwrap_or_else(Instant::now)
                    .saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return;
                }
                tokio::select! {
                    _ = changed.changed() => {},
                    _ = tokio::time::sleep(remaining) => {},
                }
            } else if changed.changed().await.is_err() {
                return;
            }
        }
    }
}

struct ExternalOwner {
    domain: Arc<Domain>,
    executor: Handle,
}
impl Drop for ExternalOwner {
    fn drop(&mut self) {
        let _ = self.domain.close_with_policy();
    }
}
#[derive(Clone)]
pub struct BackgroundRoot {
    owner: Arc<ExternalOwner>,
}
pub type BackgroundDomainOwner = BackgroundRoot;
#[derive(Clone)]
pub struct BackgroundDomainHandle {
    domain: Arc<Domain>,
    executor: Handle,
}
#[derive(Clone)]
pub struct BackgroundDomainControl {
    domain: Arc<Domain>,
}

impl BackgroundRoot {
    pub fn start(limits: WorkClassLimits, cleanup: CleanupPolicy) -> Result<Self, ResourceError> {
        Self::start_inner(limits, cleanup, None)
    }
    fn start_inner(
        limits: WorkClassLimits,
        cleanup: CleanupPolicy,
        blocking_threads: Option<usize>,
    ) -> Result<Self, ResourceError> {
        // Refuse a future deadline that cannot be represented before allocating workers.
        cleanup.deadline_from(Instant::now())?;
        let domain = Domain::new(limits, cleanup);
        let worker_domain = domain.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let coordinator = std::thread::Builder::new()
            .name("khive-background".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Err(error) = priority::lower_current_thread() {
                        let error = ResourceError::Priority(error);
                        let _ = ready_tx.send(Err(error.clone()));
                        return Err(error);
                    }
                    let hook_domain = worker_domain.clone();
                    let mut builder = tokio::runtime::Builder::new_current_thread();
                    if let Some(count) = blocking_threads {
                        builder.max_blocking_threads(count);
                    }
                    let runtime = match builder
                        .enable_all()
                        .on_thread_start(move || {
                            if let Err(error) = priority::lower_current_thread() {
                                hook_domain.posture_failed(error);
                            }
                        })
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let error = ResourceError::RuntimeStart(error.to_string());
                            let _ = ready_tx.send(Err(error.clone()));
                            return Err(error);
                        }
                    };
                    if ready_tx.send(Ok(runtime.handle().clone())).is_err() {
                        worker_domain.close(Instant::now());
                    }
                    runtime.block_on(worker_domain.drive_until_closed());
                    // Deliberately on this standard thread. Async tasks are dropped, but
                    // Runtime destruction still joins actual started blocking work.
                    drop(runtime);
                    Ok(())
                }))
                .unwrap_or_else(|_| Err(ResourceError::JoinFailed("coordinator panicked".into())));
                *lock(&worker_domain.worker_exit) = Some(result);
                worker_domain.notify();
            })
            .map_err(|error| ResourceError::ThreadStart(error.to_string()))?;
        lock(&domain.join).coordinator = Some(coordinator);
        let executor = ready_rx
            .recv()
            .map_err(|_| ResourceError::CoordinatorGone)??;
        Ok(Self {
            owner: Arc::new(ExternalOwner { domain, executor }),
        })
    }
    pub fn handle(&self) -> BackgroundDomainHandle {
        BackgroundDomainHandle {
            domain: self.owner.domain.clone(),
            executor: self.owner.executor.clone(),
        }
    }
    pub fn control(&self) -> BackgroundDomainControl {
        self.handle().control()
    }
    pub fn limits(&self) -> WorkClassLimits {
        self.owner.domain.limits
    }
    pub fn cleanup_policy(&self) -> CleanupPolicy {
        self.owner.domain.cleanup
    }
    pub fn same_domain(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner.domain, &other.owner.domain)
    }
}

impl BackgroundDomainControl {
    /// Observe domain closure without retaining its external root owner.
    /// Subscribe before the state read so a close between the read and await
    /// cannot be lost. This wait never mutates admission or exposes a token.
    pub async fn closed(&self) {
        let mut changed = self.domain.changed.subscribe();
        while !self.is_closed() {
            // self retains the sender, so the channel cannot close while this
            // wait is live. A version change triggers another state read.
            let _ = changed.changed().await;
        }
    }
    pub fn close(&self, deadline: Instant) {
        self.domain.close(deadline);
    }
    pub fn close_with_policy(&self) -> Result<(), ResourceError> {
        self.domain.close_with_policy()
    }
    pub fn is_closed(&self) -> bool {
        lock(&self.domain.gate).closed
    }
    fn ensure_join_started(&self) -> Result<(), ResourceError> {
        let mut join = lock(&self.domain.join);
        if join.joining || join.joined.is_some() {
            return Ok(());
        }
        let Some(coordinator) = join.coordinator.take() else {
            return Err(ResourceError::CoordinatorGone);
        };
        // The envelope returns ownership on thread-creation failure. A detached
        // reaper, not the requesting async future, owns completion publication.
        let envelope = Arc::new(Mutex::new(Some(coordinator)));
        let envelope_worker = envelope.clone();
        let domain = self.domain.clone();
        join.joining = true;
        let spawned = std::thread::Builder::new()
            .name("khive-background-join".into())
            .spawn(move || {
                let coordinator = lock(&envelope_worker)
                    .take()
                    .expect("single reaper owns coordinator");
                let result = coordinator
                    .join()
                    .map_err(|_| ResourceError::JoinFailed("coordinator panicked".into()))
                    .and_then(|()| {
                        lock(&domain.worker_exit)
                            .clone()
                            .unwrap_or(Err(ResourceError::CoordinatorGone))
                    });
                let mut join = lock(&domain.join);
                join.joined = Some(result);
                join.joining = false;
                drop(join);
                domain.notify();
            });
        match spawned {
            Ok(reaper) => {
                drop(reaper);
                Ok(())
            }
            Err(error) => {
                join.coordinator = lock(&envelope).take();
                join.joining = false;
                Err(ResourceError::ThreadStart(error.to_string()))
            }
        }
    }
    /// Observe actual infrastructure exit using only this caller's budget.
    /// May start the independently owned reaper, but never closes admission,
    /// requests cancellation, or changes any domain/job cleanup deadline.
    pub async fn observe_exit_until(
        &self,
        deadline: Instant,
    ) -> Result<ShutdownReport, ResourceError> {
        self.ensure_join_started()?;
        let mut changed = self.domain.changed.subscribe();
        loop {
            let joined = { lock(&self.domain.join).joined.clone() };
            if let Some(result) = joined {
                result?;
                return Ok(self.domain.report());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(self.domain.report());
            }
            tokio::select! {
                _ = changed.changed() => {},
                _ = tokio::time::sleep(remaining) => {},
            }
        }
    }
    pub async fn shutdown(&self, deadline: Instant) -> Result<ShutdownReport, ResourceError> {
        self.close(deadline);
        self.ensure_join_started()?;
        let mut changed = self.domain.changed.subscribe();
        loop {
            let joined = { lock(&self.domain.join).joined.clone() };
            if let Some(result) = joined {
                result?;
                return Ok(self.domain.report());
            }
            // Every observer shares the first/shortest cleanup deadline. A
            // notification must re-read it, never restart the caller's budget.
            let effective = lock(&self.domain.gate).deadline.unwrap_or(deadline);
            let remaining = effective.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(self.domain.report());
            }
            tokio::select! {
                _ = changed.changed() => {},
                _ = tokio::time::sleep(remaining) => {},
            }
        }
    }
}

#[derive(Clone)]
pub struct TrackedJob {
    _guard: Arc<crate::daemon::BackgroundTaskGuard>,
}
impl TrackedJob {
    pub fn register(name: &'static str) -> Self {
        Self {
            _guard: Arc::new(crate::daemon::register_background_task(name)),
        }
    }
}
struct JobCancellation {
    domain: Arc<Domain>,
    requested: AtomicBool,
    deadline: Mutex<Option<Instant>>,
    deadline_overflow: AtomicBool,
    // 0 unclaimed, 1 in progress, 2 returned/skipped, 3 incomplete.
    finalization: AtomicU8,
}
impl JobCancellation {
    fn request(&self) {
        let gate = lock(&self.domain.gate);
        if !self.requested.swap(true, Ordering::SeqCst) {
            let now = Instant::now();
            let deadline = self.domain.cleanup.deadline_from(now).unwrap_or_else(|_| {
                self.deadline_overflow.store(true, Ordering::SeqCst);
                now
            });
            shorten(&mut lock(&self.deadline), deadline);
        }
        drop(gate);
        self.domain.notify();
    }
    fn is_cancelled(&self) -> bool {
        lock(&self.domain.gate).closed || self.requested.load(Ordering::SeqCst)
    }
    fn effective_deadline(&self) -> Result<Instant, ResourceError> {
        let gate = lock(&self.domain.gate);
        let mut own = lock(&self.deadline);
        if own.is_none() {
            *own = Some(self.domain.cleanup.deadline_from(Instant::now())?);
        }
        Ok(gate
            .deadline
            .map_or(own.expect("deadline initialized"), |d| {
                d.min(own.expect("deadline initialized"))
            }))
    }
}
struct Lease {
    _permit: OwnedSemaphorePermit,
    _tracking: TrackedJob,
    cancel: Arc<JobCancellation>,
    executor: Handle,
    class: WorkClass,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.cancel.domain.jobs.fetch_sub(1, Ordering::SeqCst);
        self.cancel.domain.notify();
    }
}
#[derive(Clone)]
pub struct AdmittedJob {
    lease: Arc<Lease>,
}

struct SupervisorGuard {
    cancel: Arc<JobCancellation>,
    finished: bool,
}
impl Drop for SupervisorGuard {
    fn drop(&mut self) {
        let phase = self.cancel.finalization.load(Ordering::SeqCst);
        if (!self.finished && phase < 2) || phase == 1 {
            self.cancel.domain.incomplete.fetch_add(1, Ordering::SeqCst);
        }
        self.cancel
            .domain
            .supervisors
            .fetch_sub(1, Ordering::SeqCst);
        self.cancel.domain.notify();
    }
}
fn start_decision(cancel: &JobCancellation) -> Result<(), JobOutcome<()>> {
    let gate = lock(&cancel.domain.gate);
    start_decision_locked(cancel, &gate)
}
fn start_decision_locked(cancel: &JobCancellation, gate: &StartGate) -> Result<(), JobOutcome<()>> {
    if let Some(error) = lock(&cancel.domain.posture_failure).clone() {
        return Err(JobOutcome::Refused(error));
    }
    if gate.closed || cancel.requested.load(Ordering::SeqCst) {
        return Err(JobOutcome::Cancelled);
    }
    if let Err(error) = priority::check_current_thread() {
        return Err(JobOutcome::Refused(error));
    }
    // Returning Ok linearizes Started before releasing the gate. A later cancel
    // cannot revoke this body; its actual closure owns the lease until exit.
    Ok(())
}
fn refusal<T>(outcome: JobOutcome<()>) -> JobOutcome<T> {
    match outcome {
        JobOutcome::Cancelled => JobOutcome::Cancelled,
        JobOutcome::Refused(error) => JobOutcome::Refused(error),
        _ => unreachable!("start decision only refuses cancellation or posture"),
    }
}
impl BackgroundDomainHandle {
    /// Read-only domain-close observation, including final root-owner drop.
    pub async fn closed(&self) {
        self.control().closed().await;
    }
    pub fn limits(&self) -> WorkClassLimits {
        self.domain.limits
    }
    pub fn control(&self) -> BackgroundDomainControl {
        BackgroundDomainControl {
            domain: self.domain.clone(),
        }
    }
    pub async fn admit(
        &self,
        class: WorkClass,
        tracking: &TrackedJob,
    ) -> Result<AdmittedJob, ResourceError> {
        let semaphore = match class {
            WorkClass::Warm => self.domain.warm.clone(),
            WorkClass::Maintenance => self.domain.maintenance.clone(),
        };
        let permit = semaphore
            .acquire_owned()
            .await
            .map_err(|_| self.domain.closed_error())?;
        let gate = lock(&self.domain.gate);
        if gate.closed {
            return Err(self.domain.closed_error());
        }
        self.domain.jobs.fetch_add(1, Ordering::SeqCst);
        let cancel = Arc::new(JobCancellation {
            domain: self.domain.clone(),
            requested: AtomicBool::new(false),
            deadline: Mutex::new(None),
            deadline_overflow: AtomicBool::new(false),
            finalization: AtomicU8::new(0),
        });
        Ok(AdmittedJob {
            lease: Arc::new(Lease {
                _permit: permit,
                _tracking: tracking.clone(),
                cancel,
                executor: self.executor.clone(),
                class,
            }),
        })
    }
    pub fn spawn_supervised<F, Fut, T>(
        &self,
        job: AdmittedJob,
        make: F,
    ) -> Result<BackgroundJob<T>, ResourceError>
    where
        F: FnOnce(AdmittedJob) -> Fut + Send + 'static,
        Fut: Future<Output = JobOutcome<T>> + Send + 'static,
        T: Send + 'static,
    {
        if !Arc::ptr_eq(&self.domain, &job.lease.cancel.domain) {
            return Err(ResourceError::WrongDomain);
        }
        let gate = lock(&self.domain.gate);
        if gate.closed {
            return Err(self.domain.closed_error());
        }
        let cancel = job.lease.cancel.clone();
        self.domain.supervisors.fetch_add(1, Ordering::SeqCst);
        let guard = SupervisorGuard {
            cancel: cancel.clone(),
            finished: false,
        };
        let task = self.executor.spawn(async move {
            let mut guard = guard;
            let execution_hold = job.clone();
            let outcome = match start_decision(&job.lease.cancel) {
                Ok(()) => make(job).await,
                Err(outcome) => refusal(outcome),
            };
            guard.finished = true;
            drop(execution_hold);
            outcome
        });
        drop(gate);
        Ok(BackgroundJob {
            task: Some(task),
            cancel,
            cancel_on_drop: true,
        })
    }
}
impl AdmittedJob {
    pub fn class(&self) -> WorkClass {
        self.lease.class
    }
    pub fn is_cancelled(&self) -> bool {
        self.lease.cancel.is_cancelled()
    }
    pub fn request_cancel(&self) {
        self.lease.cancel.request();
    }
    pub async fn cancelled(&self) {
        let mut changed = self.lease.cancel.domain.changed.subscribe();
        while !self.is_cancelled() {
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
    pub async fn run_body<F: Future>(&self, body: F) -> BodyOutcome<F::Output> {
        if self.is_cancelled() {
            return BodyOutcome::Cancelled;
        }
        tokio::select! {
            biased;
            result = body => BodyOutcome::Finished(result),
            _ = self.cancelled() => BodyOutcome::Cancelled,
        }
    }
    pub async fn finalize<F: Future<Output = ()>>(&self, append: F) -> FinalizationOutcome {
        let cancel = &self.lease.cancel;
        if cancel
            .finalization
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return FinalizationOutcome::AlreadyFinalized;
        }
        let mut changed = cancel.domain.changed.subscribe();
        let outcome = if cancel.deadline_overflow.load(Ordering::SeqCst) {
            FinalizationOutcome::DeadlineOverflow
        } else {
            tokio::pin!(append);
            let mut polled = false;
            loop {
                let deadline = match cancel.effective_deadline() {
                    Ok(deadline) => deadline,
                    Err(_) => break FinalizationOutcome::DeadlineOverflow,
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break if polled {
                        FinalizationOutcome::AttemptTimedOut
                    } else {
                        FinalizationOutcome::NoAttemptBeforeDeadline
                    };
                }
                // Claiming the attempt precedes its first poll; deadline wakeups
                // only shorten this same future, never construct a retry.
                polled = true;
                tokio::select! {
                    biased;
                    _ = &mut append => break FinalizationOutcome::AttemptReturned,
                    _ = changed.changed() => {},
                    _ = tokio::time::sleep(remaining) => {},
                }
            }
        };
        let incomplete = !matches!(outcome, FinalizationOutcome::AttemptReturned);
        cancel
            .finalization
            .store(if incomplete { 3 } else { 2 }, Ordering::SeqCst);
        if incomplete {
            cancel.domain.incomplete.fetch_add(1, Ordering::SeqCst);
        }
        outcome
    }
    pub fn no_telemetry(&self) -> FinalizationOutcome {
        if self
            .lease
            .cancel
            .finalization
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            FinalizationOutcome::NoTelemetryToken
        } else {
            FinalizationOutcome::AlreadyFinalized
        }
    }
    pub fn spawn_blocking<F, T>(&self, body: F) -> Result<BackgroundJob<T>, ResourceError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let domain = self.lease.cancel.domain.clone();
        let gate = lock(&domain.gate);
        if gate.closed {
            return Err(domain.closed_error());
        }
        let cancel = self.lease.cancel.clone();
        // Tokio can enqueue before panicking on worker creation. Only this
        // recoverable escrow owns the user data and child lease. The inert
        // queued wrapper owns neither after failed handoff reclamation.
        let escrow = Arc::new(Mutex::new(BlockingEscrow {
            published: false,
            work: Some(BlockingWork {
                body,
                child: self.clone(),
            }),
        }));
        let queued_escrow = escrow.clone();
        let queued_domain = domain.clone();
        // Catch *inside* the gate: a dequeued worker must not start user code
        // between submission unwind and failed-publication/reclamation.
        let submitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let task = self.lease.executor.spawn_blocking(move || {
                #[cfg(test)]
                if let Some(dequeued) = lock(&queued_domain.blocking_dequeued).take() {
                    let _ = dequeued.send(());
                }
                let gate = lock(&queued_domain.gate);
                let work = {
                    let mut escrow = lock(&queued_escrow);
                    if escrow.published {
                        escrow.work.take()
                    } else {
                        None
                    }
                };
                let Some(BlockingWork { body, child }) = work else {
                    // Failed submit already reclaimed all user/lease ownership.
                    return JobOutcome::Cancelled;
                };
                let decision = start_decision_locked(&child.lease.cancel, &gate);
                drop(gate);
                // Run AND drop user data outside the gate: destructors may
                // reenter domain control. Started bodies keep the actual hold.
                let outcome = match decision {
                    Ok(()) => JobOutcome::Completed(body()),
                    Err(outcome) => {
                        drop(body);
                        refusal(outcome)
                    }
                };
                drop(child);
                outcome
            });
            #[cfg(test)]
            {
                let fault = lock(&domain.after_blocking_enqueue).take();
                if let Some(fault) = fault {
                    fault();
                }
            }
            task
        }));
        match submitted {
            Ok(task) => {
                lock(&escrow).published = true;
                drop(gate);
                Ok(BackgroundJob {
                    task: Some(task),
                    cancel,
                    cancel_on_drop: true,
                })
            }
            Err(payload) => {
                let unstarted = lock(&escrow).work.take();
                // published remains false while holding the start gate. The
                // worker cannot take the reclaimed work after we unlock.
                drop(gate);
                drop(unstarted);
                // Panic payloads may also have user-defined destructors.
                drop(payload);
                Err(ResourceError::ThreadStart(
                    "blocking task submission panicked before handoff".into(),
                ))
            }
        }
    }
}

struct BlockingWork<F> {
    // Field drop order keeps the child hold until captured user data is gone.
    body: F,
    child: AdmittedJob,
}
struct BlockingEscrow<F> {
    // Accessed only under the domain start gate, then the escrow mutex.
    published: bool,
    work: Option<BlockingWork<F>>,
}

pub struct BackgroundJob<T> {
    task: Option<JoinHandle<JobOutcome<T>>>,
    cancel: Arc<JobCancellation>,
    cancel_on_drop: bool,
}
impl<T> BackgroundJob<T> {
    pub fn request_cancel(&self) {
        self.cancel.request();
    }
    pub async fn join(mut self) -> Result<JobOutcome<T>, BackgroundJoinError> {
        let result = self.task.as_mut().expect("join consumes one task").await;
        self.cancel_on_drop = false;
        result.map_err(|error| {
            if error.is_cancelled() {
                BackgroundJoinError::Cancelled
            } else if error.is_panic() {
                BackgroundJoinError::Panicked
            } else {
                BackgroundJoinError::Other(error.to_string())
            }
        })
    }
    pub fn detach(mut self) {
        self.cancel_on_drop = false;
    }
}
impl<T> Drop for BackgroundJob<T> {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            self.cancel.request();
        }
        // Dropping Tokio's JoinHandle detaches. It must not abort the phase
        // supervisor that still owns the bounded terminal-emission attempt.
    }
}
