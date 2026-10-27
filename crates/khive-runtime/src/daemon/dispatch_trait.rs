use super::*;

// ── dispatch trait ────────────────────────────────────────────────────────────

/// Transport-agnostic dispatch interface for the daemon server.
///
/// The MCP crate implements this by dispatching through the shared request body
/// while honoring [`DaemonRequestFrame::from_wire`] (so subhandler visibility is
/// gated by request origin, not by transport); any future transport can do the
/// same.
#[cfg(unix)]
#[async_trait]
pub trait DaemonDispatch: Clone + Send + Sync + 'static {
    /// Explicit host-owned background domain, absent for registry-only test dispatchers.
    fn background_resources(&self) -> Option<crate::resource_admission::BackgroundDomainControl> {
        None
    }

    /// Named retained resources or unknown inventory that prevents idle exit.
    /// An implementor must explicitly account for its resources before retiring.
    fn idle_retirement_blockers(&self) -> Vec<String> {
        vec!["dispatcher_resource_inventory_unknown".to_owned()]
    }

    /// Describe syntax and loaded catalog membership without dispatching.
    fn plan(&self, ops: &str) -> String;

    /// Dispatch a verb-DSL request string and return the rendered result.
    ///
    /// `from_wire` carries the origin discriminator from
    /// [`DaemonRequestFrame::from_wire`]: when `true`, the implementor enforces
    /// verb visibility (rejects `Visibility::Subhandler` verbs); when `false`,
    /// the request is from a trusted operator surface and subhandlers pass.
    ///
    /// `identity` is the per-request identity context threaded from the frame
    /// (ADR-096): `Some(..)` when serving a request forwarded over the
    /// daemon socket (built from `frame.namespace` / `frame.actor_id` /
    /// `frame.visible_namespaces` by the connection handler), `None` for any
    /// other dispatch path. Implementors should mint the storage/gate token from
    /// `identity` when present and fall back to their own construction-baked
    /// identity when absent, so pure local (non-daemon) dispatch is unchanged.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<RequestIdentity>,
    ) -> Result<String, String>;

    /// Read-deadline ceiling for one request. The default is the operator
    /// ceiling; an implementor that understands the request may grant a
    /// longer bounded allowance (a long poll's declared wait plus a margin).
    fn request_read_timeout(&self, _ops: &str) -> std::time::Duration {
        khive_storage::request_read_timeout_from_env()
    }

    /// Preserve structured dispatch errors without breaking string-only implementors.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_with_error_detail(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<RequestIdentity>,
    ) -> Result<String, DaemonDispatchError> {
        self.dispatch(
            ops,
            presentation,
            presentation_per_op,
            format,
            format_per_op,
            from_wire,
            identity,
        )
        .await
        .map_err(|message| DaemonDispatchError::new(message, None))
    }

    /// The startup gate for the events daemon this daemon supervises, if any
    /// ([`crate::events_split::EventsReadiness`]). A request that dispatches
    /// verbs waits at it before dispatch and is refused, with nothing
    /// committed, when it does not open; probe, plan, metrics and handover
    /// frames never wait. `None`, the default, dispatches without waiting.
    fn events_readiness(&self) -> Option<crate::events_split::EventsReadiness> {
        None
    }

    /// Warm every pack's in-memory state (ANN indexes, etc.).
    async fn warm_all(&self);

    /// The namespace this dispatcher was configured for.
    fn namespace(&self) -> &str;

    /// Fingerprint of this dispatcher's resolved runtime config (packs, db
    /// target, embedders). Used to reject forwarded requests from clients whose
    /// config differs, so a restricted client cannot dispatch through a broader
    /// daemon.
    fn config_id(&self) -> &str;

    /// Return the pool to use for background WAL checkpointing, if available.
    ///
    /// Implementors backed by a file-based SQLite database should return
    /// `Some(pool_arc)`. In-memory or test dispatchers that have no pool
    /// return `None` and the checkpoint task is not spawned.
    ///
    /// The default implementation returns `None`.
    fn pool_for_checkpoint(&self) -> Option<Arc<ConnectionPool>> {
        None
    }

    /// File-backed backend pools beyond [`Self::pool_for_checkpoint`]'s pool
    /// (ADR-091 Amendment 3): one checkpoint task is spawned per entry here,
    /// in addition to the one spawned for the primary pool, so a
    /// multi-backend deployment gets PASSIVE/TRUNCATE checkpointing and
    /// sidecar enumeration on every file-backed backend it wired, not only
    /// the main one.
    ///
    /// The default implementation returns an empty `Vec` — an implementor
    /// with only one backend (or none) needs no override.
    fn secondary_pools_for_checkpoint(&self) -> Vec<Arc<ConnectionPool>> {
        Vec::new()
    }

    /// Return the audit `EventStore` the checkpoint task should append
    /// ADR-094 lifecycle events (`CheckpointOutcomeRecorded`) to, if any.
    ///
    /// Mirrors [`Self::pool_for_checkpoint`]'s default-`None` shape: an
    /// implementor with no configured event store (or no pool at all) simply
    /// gets a checkpoint task that never appends events — the checkpoint
    /// task itself remains fully functional either way.
    ///
    /// The default implementation returns `None`.
    fn event_store_for_checkpoint(&self) -> Option<Arc<dyn khive_storage::EventStore>> {
        None
    }
}

#[cfg(unix)]
pub(super) struct CheckpointTaskSpec {
    pub(super) pool: Arc<ConnectionPool>,
    pub(super) lifecycle_owner: Option<CheckpointLifecycleOwner>,
    pub(super) is_main: bool,
}

/// Build checkpoint-task fan-out and designate one lifecycle owner.
///
/// The main checkpoint task owns lifecycle emission when it exists. If the
/// main backend is in-memory and therefore has no checkpoint task, the first
/// file-backed secondary owns emission instead. All remaining tasks are
/// explicit non-owners.
#[cfg(unix)]
pub(super) fn checkpoint_task_specs(
    main_pool: Option<Arc<ConnectionPool>>,
    secondary_pools: Vec<Arc<ConnectionPool>>,
    event_store: Option<Arc<dyn khive_storage::EventStore>>,
    namespace: String,
) -> Vec<CheckpointTaskSpec> {
    let mut tasks = Vec::with_capacity(usize::from(main_pool.is_some()) + secondary_pools.len());
    if let Some(pool) = main_pool {
        tasks.push(CheckpointTaskSpec {
            pool,
            lifecycle_owner: None,
            is_main: true,
        });
    }
    tasks.extend(secondary_pools.into_iter().map(|pool| CheckpointTaskSpec {
        pool,
        lifecycle_owner: None,
        is_main: false,
    }));

    if let (Some(task), Some(event_store)) = (tasks.first_mut(), event_store) {
        task.lifecycle_owner = Some(CheckpointLifecycleOwner::new(event_store, namespace));
    }
    tasks
}

// ── tracked background tasks ─────────────────────────────────────────────────
//
// Pack handlers (e.g. memory.recall's ADR-081 serve-ledger append) fire
// fire-and-forget `tokio::spawn`ed work off the response path so the caller
// never waits on a cross-pack dispatch or a SQL write. Left untracked, that
// work is invisible to `drain()`: a SIGTERM landing between the response
// returning and the spawned task completing can abort it mid-flight with no
// log and no row. `track_background_task` gives such spawns a process-wide
// presence that `drain()` waits on, exactly like the `active` counter does
// for in-flight connections: the caller still only pays for the spawn +
// counter increment, never the task's own work.
/// Set once by the boot path that takes the daemon role, and never cleared: a
/// process that is not the warm daemon has no path to becoming one except exec.
pub(super) static WARM_INDEX_HOST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Declare this process the warm index host. Called by the serve path as soon as
/// the daemon role is decided, before any runtime is built, so nothing warms
/// under the wrong answer.
pub fn mark_warm_index_host() {
    WARM_INDEX_HOST.store(true, std::sync::atomic::Ordering::Release);
}

/// Whether this process is the warm index host.
///
/// Building an ANN index from the full corpus is minutes of CPU and hundreds of
/// megabytes of segment rewrite, and it pays for itself only across a process
/// that outlives the request. A short-lived client that does it pays the whole
/// cost, discards the result at exit, and publishes a checkpoint that every
/// other reader on the root must then re-read. Consumers use this to decide
/// whether to build or to serve degraded and let the daemon build.
pub fn is_warm_index_host() -> bool {
    WARM_INDEX_HOST.load(std::sync::atomic::Ordering::Acquire)
}

pub(super) static BACKGROUND_TASKS: std::sync::OnceLock<Arc<std::sync::atomic::AtomicUsize>> =
    std::sync::OnceLock::new();

pub(super) fn background_tasks() -> &'static Arc<std::sync::atomic::AtomicUsize> {
    BACKGROUND_TASKS.get_or_init(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
}

/// Decrements the shared background-task counter from `Drop`, so the count
/// comes back down whether the tracked future returns normally, panics, or
/// is cancelled — a plain post-`await` `fetch_sub` only covers the return
/// path and leaks the count forever on a panic, since unwinding skips every
/// statement after the panic point.
// ── outstanding background-task names ────────────────────────────────────────
//
// Names of the tasks the background counter is currently holding, so a drain
// timeout can say which ones held it open instead of printing a bare count.
// Registered and released at exactly the points the counter is incremented and
// decremented, and in the order that keeps the counter authoritative: the name
// goes in after the increment and comes out before the decrement, so a reported
// name always belongs to a task the counter already holds. The reverse ordering
// would let the warning name a task that had already finished, which is the one
// reading that would send someone looking in the wrong place.
pub(super) static BACKGROUND_TASK_NAMES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<&'static str, usize>>,
> = std::sync::OnceLock::new();

pub(super) fn background_task_names_registry(
) -> &'static std::sync::Mutex<std::collections::HashMap<&'static str, usize>> {
    BACKGROUND_TASK_NAMES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Name recorded for tasks spawned through the unnamed entry points, which
/// stay on the public API of a published crate.
pub const UNNAMED_BACKGROUND_TASK: &str = "unnamed";

pub(super) fn register_background_task_name(name: &'static str) {
    let mut names = background_task_names_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *names.entry(name).or_insert(0) += 1;
}

pub(super) fn release_background_task_name(name: &'static str) {
    let mut names = background_task_names_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(count) = names.get_mut(name) {
        *count -= 1;
        if *count == 0 {
            names.remove(name);
        }
    }
}

/// Names of the in-flight tracked background tasks, sorted and deduplicated.
/// A diagnostic beside [`background_task_count`], never a substitute for it:
/// the count is what drain waits on.
pub fn background_task_names() -> Vec<String> {
    let names = background_task_names_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out: Vec<String> = names.keys().map(|name| (*name).to_string()).collect();
    out.sort();
    out
}

#[cfg(unix)]
pub(super) fn idle_retirement_blockers<D: DaemonDispatch>(dispatcher: &D) -> Vec<String> {
    let mut blockers = dispatcher.idle_retirement_blockers();
    if !khive_storage::tx_registry::snapshot().is_empty() {
        blockers.push("open_sql_transaction".to_owned());
    }
    blockers.extend(
        active_phase_names()
            .into_iter()
            .map(|name| format!("active_phase:{name}")),
    );
    let count = background_task_count();
    let names = background_task_names_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if names.values().sum::<usize>() != count {
        blockers.push("tracked_worker_inventory_unsettled".to_owned());
    }
    // Only these inspected loops maintain replaceable caches/checkpoint state.
    // Their tracked lifetime still participates in the final drain.
    for name in names.keys() {
        if !matches!(
            *name,
            "wal_checkpoint" | "memory_ann_rotation_watch" | "knowledge_ann_rotation_watch"
        ) {
            blockers.push(format!("unsettled_worker:{name}"));
        }
    }
    blockers.sort();
    blockers.dedup();
    blockers
}

#[cfg(unix)]
pub(super) async fn wait_for_idle<D: DaemonDispatch>(dispatcher: &D, lifecycle: &DaemonLifecycle) {
    if lifecycle.options.lifetime == DaemonLifetime::Persistent {
        std::future::pending::<()>().await;
    }
    loop {
        if lifecycle.try_idle(|| idle_retirement_blockers(dispatcher)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub(crate) struct BackgroundTaskGuard {
    pub(super) counter: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) name: &'static str,
}

impl Drop for BackgroundTaskGuard {
    fn drop(&mut self) {
        release_background_task_name(self.name);
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Spawn a task that daemon shutdown's `drain()` waits for and return its join
/// handle. Retaining the handle lets boot coordinators form an explicit barrier;
/// dropping it deliberately detaches the task while the background counter still
/// keeps daemon drain aware of its lifetime. The decrement happens via
/// `BackgroundTaskGuard`'s `Drop`, including panic and cancellation paths.
pub fn spawn_tracked_task<F, T>(fut: F) -> tokio::task::JoinHandle<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    spawn_named_tracked_task(UNNAMED_BACKGROUND_TASK, fut)
}

/// [`spawn_tracked_task`] with a name that a drain timeout can print.
///
/// The name is a short static string describing the task, never a formatted or
/// caller-supplied value: it is read by an operator staring at a shutdown that
/// would not finish, so it is a label for a call site, not a record of one
/// occurrence.
pub fn spawn_named_tracked_task<F, T>(name: &'static str, fut: F) -> tokio::task::JoinHandle<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let guard = register_background_task(name);
    tokio::spawn(async move {
        let _guard = guard;
        fut.await
    })
}

/// Register one real tracked job before its executor/queue handoff.
/// Shared wrappers may hold this same RAII guard through actual child exit.
pub(crate) fn register_background_task(name: &'static str) -> BackgroundTaskGuard {
    background_tasks().fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    register_background_task_name(name);
    BackgroundTaskGuard {
        counter: background_tasks().clone(),
        name,
    }
}

/// Spawn a fire-and-forget task through [`spawn_tracked_task`].
///
/// Callers that need a boot or shutdown barrier should retain and await the
/// returned handle from [`spawn_tracked_task`] instead of detaching it here.
pub fn track_background_task<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    track_named_background_task(UNNAMED_BACKGROUND_TASK, fut);
}

/// [`track_background_task`] with a name that a drain timeout can print. See
/// [`spawn_named_tracked_task`] for what belongs in the name.
pub fn track_named_background_task<F>(name: &'static str, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    drop(spawn_named_tracked_task(name, fut));
}

/// Current count of in-flight tasks started via [`track_background_task`].
/// Exposed for tests; `drain()` reads the shared counter directly.
pub fn background_task_count() -> usize {
    background_tasks().load(std::sync::atomic::Ordering::SeqCst)
}

/// Process-wide daemon shutdown signal (ADR-119).
///
/// Cancelled exactly once, when the daemon's unified shutdown future resolves
/// — before `drain()` begins waiting on tracked tasks — so long-running
/// daemon components supervised outside this module observe shutdown through
/// the same path the daemon itself does, rather than inventing their own.
/// Clones share the underlying token; child tokens derived from it are
/// cancelled transitively.
///
/// In non-daemon processes the token simply never fires.
pub fn daemon_shutdown_token() -> tokio_util::sync::CancellationToken {
    static TOKEN: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
        std::sync::OnceLock::new();
    TOKEN
        .get_or_init(tokio_util::sync::CancellationToken::new)
        .clone()
}

// ── active background phase names (ADR-103) ──────────────────────────────────
//
// A lightweight, best-effort process-wide gauge of which named background
// phases (e.g. `ann_warm`) are in flight right now, read by `comm.health`'s
// resource self-report so a caller can see "what is the daemon doing" at a
// glance without correlating timestamps across the event log itself. Counted
// per name rather than boolean, since more than one occurrence of the same
// named phase can legitimately overlap (e.g. two embedding models warming
// concurrently) — the name only drops out of the reported set once every
// concurrent occurrence has ended.
pub(super) static ACTIVE_PHASES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::OnceLock::new();

pub(super) fn active_phases() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>>
{
    ACTIVE_PHASES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// RAII guard for one occurrence of a named background phase. Increments the
/// phase's count on creation (see [`register_active_phase`]); decrements on
/// `Drop`, so the count comes back down whether the guarded work returns
/// normally, panics, or is cancelled — the same rationale as
/// `BackgroundTaskGuard` above.
pub struct PhaseGuard {
    pub(super) name: String,
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let mut map = active_phases()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = map.get_mut(&self.name) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.name);
            }
        }
    }
}

/// Register one occurrence of a named background phase as currently active.
/// Returns a guard: drop it (or let it fall out of scope) when the phase
/// ends. Best-effort process-wide gauge only, read by `comm.health` — never
/// load-bearing for correctness.
pub fn register_active_phase(name: &str) -> PhaseGuard {
    let mut map = active_phases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *map.entry(name.to_string()).or_insert(0) += 1;
    PhaseGuard {
        name: name.to_string(),
    }
}

/// Currently active background-phase names, sorted for deterministic output.
/// Empty when no tracked phase is in flight.
pub fn active_phase_names() -> Vec<String> {
    let map = active_phases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
}

// ── server ────────────────────────────────────────────────────────────────────

/// Build a point-in-time [`MetricsSnapshot`] of this process's server-side
/// gauges. Called only from `handle_conn`'s `metrics_only` arm — a
/// process-global, read-only assembly with no side effects of its own.
/// See `docs/api/daemon.md#build_metrics_snapshot` for where each gauge is sourced
/// from and why.
#[cfg(unix)]
pub(super) fn build_metrics_snapshot<D: DaemonDispatch>(dispatcher: &D) -> MetricsSnapshot {
    let open_tx_count = khive_storage::tx_registry::snapshot().len();
    // ADR-091 Amendment 3: deliberately the process-wide aggregate, not a
    // backend-attributed view — this gauge reports "oldest pinned tx in this
    // process" across every wired backend, not an attribution claim about
    // which database it belongs to. Attribution consumers (the session sweep,
    // the per-backend checkpoint tasks) use the scoped `oldest_for` views in
    // khive-db instead.
    let (oldest_pinned_tx_micros, oldest_pinned_tx_label) =
        match khive_storage::tx_registry::oldest() {
            Some((_id, age, label)) => (Some(age.as_micros() as u64), label),
            None => (None, None),
        };

    let checkpoint_pool = dispatcher.pool_for_checkpoint();
    let mut secondary_index = 0;
    let wal_checkpoint_stores = checkpoint_task_specs(
        checkpoint_pool.clone(),
        dispatcher.secondary_pools_for_checkpoint(),
        None,
        String::new(),
    )
    .into_iter()
    .map(|task| {
        let (store_id, role) = if task.is_main {
            ("main".to_string(), "main".to_string())
        } else {
            let store_id = format!("secondary:{secondary_index}");
            secondary_index += 1;
            (store_id, "secondary".to_string())
        };
        CheckpointStoreMetrics {
            store_id,
            role,
            database: task
                .pool
                .canonical_path()
                .and_then(std::path::Path::file_name)
                .map(|name| name.to_string_lossy().into_owned()),
            timing: khive_db::checkpoint::checkpoint_timing(&task.pool),
        }
    })
    .collect();
    let routine_wal = checkpoint_pool
        .as_deref()
        .and_then(khive_db::checkpoint::routine_wal_observation);
    let writer_stages = checkpoint_pool
        .as_deref()
        .and_then(khive_db::writer_task::last_writer_stage_observation);
    let (write_queue_depth, write_queue_capacity) = checkpoint_pool
        .as_ref()
        .and_then(|pool| pool.writer_task_handle().ok().flatten())
        .map(|handle| (Some(handle.queue_depth()), Some(handle.capacity())))
        .unwrap_or((None, None));

    MetricsSnapshot {
        lifecycle: None,
        wal_pages: routine_wal.as_ref().map(|sample| sample.log_frames),
        wal_log_frames: routine_wal.as_ref().map(|sample| sample.log_frames),
        wal_checkpointed_frames: routine_wal
            .as_ref()
            .map(|sample| sample.checkpointed_frames),
        wal_pending_frames: routine_wal.as_ref().map(|sample| sample.pending_frames),
        wal_physical_bytes: routine_wal
            .as_ref()
            .and_then(|sample| sample.physical_wal_bytes),
        wal_observed_at_unix_ms: routine_wal
            .as_ref()
            .map(|sample| sample.observed_at_unix_ms),
        wal_checkpoint_stores,
        wal_truncate_attempts: khive_db::checkpoint::truncate_attempts(),
        wal_truncate_consecutive_failures: khive_db::checkpoint::truncate_consecutive_failures(),
        wal_checkpoint_skipped_ticks: khive_db::checkpoint::checkpoint_skipped_ticks(),
        wal_checkpoint_consecutive_skips: khive_db::checkpoint::checkpoint_consecutive_skips(),
        wal_checkpoint_last_skip_wal_pages: khive_db::checkpoint::checkpoint_last_skip_wal_pages(),
        oldest_pinned_tx_micros,
        oldest_pinned_tx_label,
        open_tx_count,
        write_queue_depth,
        write_queue_capacity,
        write_last_queue_wait_micros: writer_stages
            .as_ref()
            .map(|sample| sample.queue_wait_micros),
        write_last_transaction_acquire_micros: writer_stages
            .as_ref()
            .map(|sample| sample.transaction_acquire_micros),
        write_last_body_micros: writer_stages.as_ref().map(|sample| sample.body_micros),
        write_last_commit_micros: writer_stages.as_ref().map(|sample| sample.commit_micros),
        write_last_total_micros: writer_stages.as_ref().map(|sample| sample.total_micros),
        write_last_observed_at_unix_ms: writer_stages
            .as_ref()
            .map(|sample| sample.observed_at_unix_ms),
        connections: None,
        recall_ledger: Some(recall_ledger_snapshot()),
    }
}
