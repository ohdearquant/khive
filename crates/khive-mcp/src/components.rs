//! Daemon component registry (ADR-119): host-supervised, daemon-role-only
//! long-running work beside the verb plane.
//!
//! A daemon component is not a verb. It is host-constructed background work
//! (channel ingest loops, drains, maintenance scans) supervised by this
//! registry for cancellation, restart budgets, backoff, health, and bounded
//! shutdown. External components register at link time through `inventory`
//! (ADR-119 Amendment 1), so a distribution binary's components participate
//! without this crate naming any of them. The host additionally contributes
//! dynamic `schedule-tick`, `blob-upload-sweep`, and configured channel outbox
//! registrations when their resolved packs supply writable state; a plain
//! core build still has an empty external inventory.
//!
//! Supervision joins the daemon's existing shutdown path: every supervisor
//! task is registered through `track_background_task`, and cancellation
//! arrives via [`khive_runtime::daemon_shutdown_token`], which the daemon
//! cancels before `drain()` — so each component's bounded shutdown runs
//! inside the drain wait.
//!
//! Blocking work inside a component must use a bounded blocking pool
//! (`spawn_blocking`) or a subprocess boundary; a component future must not
//! occupy an async runtime worker with synchronous work.
//!
//! Production startup runs after the daemon has bound its socket, restricted
//! its permissions, and written its PID file while holding the boot guard.
//! A candidate that fails establishment starts no components. The daemon's
//! teardown guard still cancels components on every exit, including unwinding
//! from the startup callback; normal shutdown joins them inside drain.
//! Components must remain idempotent across ordinary crash/restart delivery.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use crate::server::KhiveMcpServer;

mod backoff;
#[cfg(test)]
use backoff::restart_delay_ms;
use backoff::Backoff;

/// How the host reacts when a component's future resolves with an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartClass {
    /// Never restarted; any failure is terminal.
    Never,
    /// Restarted after a retryable failure, within the registration's budget.
    OnFailure,
}

/// Component-reported failure. The author classifies; the host acts only on
/// the classification.
#[derive(Debug)]
pub enum ComponentError {
    /// Transient: eligible for restart under the budget (backend unavailable,
    /// transient resolution failure).
    Retryable(String),
    /// Cannot change within the process lifetime (contradictory
    /// configuration, schema/contract incompatibility). Terminal immediately,
    /// no restart, budget irrelevant.
    Permanent(String),
}

impl std::fmt::Display for ComponentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComponentError::Retryable(e) => write!(f, "retryable: {e}"),
            ComponentError::Permanent(e) => write!(f, "permanent: {e}"),
        }
    }
}

/// Future returned by a component start function.
pub type ComponentFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ComponentError>> + Send>>;

/// Start function collected at link time. Receives the host context and
/// returns the component's long-running future.
pub type ComponentStart = fn(HostContext) -> ComponentFuture;

/// Link-time registration of one daemon component.
///
/// Submitted via `inventory::submit!` from the crate that owns the component.
/// The host constructs and supervises; registrations are collected only at
/// daemon startup — non-daemon roles never start components.
pub struct DaemonComponentRegistration {
    /// Stable component name: health rows, logs, and the startup roster.
    pub name: &'static str,
    pub restart: RestartClass,
    /// Restart budget for retryable failures over the daemon process
    /// lifetime. Exhaustion is terminal (`Unhealthy`); the host never
    /// hot-loops a failing component.
    pub max_restarts: u32,
    pub backoff_initial_ms: u64,
    pub backoff_max_ms: u64,
    /// Bound on the wait for the component to observe cooperative
    /// cancellation before the host aborts its task.
    pub shutdown_timeout_ms: u64,
    pub start: ComponentStart,
}

inventory::collect!(DaemonComponentRegistration);

type ComponentFactory = Arc<dyn Fn(HostContext) -> ComponentFuture + Send + Sync + 'static>;

#[derive(Clone)]
struct ComponentRegistration {
    name: &'static str,
    restart: RestartClass,
    max_restarts: u32,
    backoff_initial_ms: u64,
    backoff_max_ms: u64,
    shutdown_timeout_ms: u64,
    start: ComponentFactory,
}

impl From<&'static DaemonComponentRegistration> for ComponentRegistration {
    fn from(reg: &'static DaemonComponentRegistration) -> Self {
        let start = reg.start;
        Self {
            name: reg.name,
            restart: reg.restart,
            max_restarts: reg.max_restarts,
            backoff_initial_ms: reg.backoff_initial_ms,
            backoff_max_ms: reg.backoff_max_ms,
            shutdown_timeout_ms: reg.shutdown_timeout_ms,
            start: Arc::new(move |ctx| start(ctx)),
        }
    }
}

const SCHEDULE_COMPONENT_NAME: &str = "schedule-tick";
const SCHEDULE_MAX_RESTARTS: u32 = 5;
const SCHEDULE_BACKOFF_INITIAL_MS: u64 = 1_000;
const SCHEDULE_BACKOFF_MAX_MS: u64 = 60_000;
const SCHEDULE_SHUTDOWN_TIMEOUT_MS: u64 = 5_000;

fn schedule_component_registration(
    runtime: khive_runtime::KhiveRuntime,
    interval: Duration,
) -> ComponentRegistration {
    ComponentRegistration {
        name: SCHEDULE_COMPONENT_NAME,
        restart: RestartClass::OnFailure,
        max_restarts: SCHEDULE_MAX_RESTARTS,
        backoff_initial_ms: SCHEDULE_BACKOFF_INITIAL_MS,
        backoff_max_ms: SCHEDULE_BACKOFF_MAX_MS,
        shutdown_timeout_ms: SCHEDULE_SHUTDOWN_TIMEOUT_MS,
        start: Arc::new(move |ctx| {
            Box::pin(crate::pending_events::schedule_tick_loop(
                runtime.clone(),
                ctx,
                interval,
            ))
        }),
    }
}

const BLOB_UPLOAD_COMPONENT_NAME: &str = "blob-upload-sweep";

/// Service obligations without an idle-retirement completion contract.
/// Unknown linked components stay named and ineligible; a registration is
/// never silently treated as expendable cache maintenance.
pub(crate) fn idle_retirement_obligations(server: &KhiveMcpServer) -> Vec<String> {
    let mut obligations: Vec<String> = inventory::iter::<DaemonComponentRegistration>()
        .map(|registration| format!("unclassified_component:{}", registration.name))
        .collect();
    if server.blob_upload_manager().is_some() {
        obligations.push(format!("service_obligation:{BLOB_UPLOAD_COMPONENT_NAME}"));
    }
    obligations.sort();
    obligations.dedup();
    obligations
}

fn blob_upload_component_registration(
    manager: Arc<khive_pack_blob::uploads::UploadManager>,
) -> ComponentRegistration {
    ComponentRegistration {
        name: BLOB_UPLOAD_COMPONENT_NAME,
        restart: RestartClass::OnFailure,
        max_restarts: 5,
        backoff_initial_ms: 1_000,
        backoff_max_ms: 60_000,
        shutdown_timeout_ms: 5_000,
        start: Arc::new(move |ctx| Box::pin(blob_upload_sweep_loop(manager.clone(), ctx))),
    }
}

async fn blob_upload_sweep_loop(
    manager: Arc<khive_pack_blob::uploads::UploadManager>,
    ctx: HostContext,
) -> Result<(), ComponentError> {
    let interval = manager.sweep_interval();
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => return Ok(()),
            _ = ticker.tick() => {}
        }
        match manager.sweep().await {
            Ok(_) => ctx.heartbeat(),
            Err(error) => {
                tracing::warn!(%error, "blob upload sweep failed; retrying at the next tick");
            }
        }
    }
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn channel_component_registration(
    name: &'static str,
    start: ComponentFactory,
) -> ComponentRegistration {
    ComponentRegistration {
        name,
        restart: RestartClass::OnFailure,
        max_restarts: 5,
        backoff_initial_ms: 1_000,
        backoff_max_ms: 60_000,
        shutdown_timeout_ms: 5_000,
        start,
    }
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(crate) fn start_channel_component(
    name: &'static str,
    server: &KhiveMcpServer,
    start: impl Fn(HostContext) -> ComponentFuture + Send + Sync + 'static,
) {
    start_component_registrations(
        vec![channel_component_registration(name, Arc::new(start))],
        server,
        khive_runtime::daemon_shutdown_token(),
        component_health().clone(),
    );
}

/// Supervisor-observed component state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentState {
    Running,
    /// Failed retryably; restart pending or in backoff.
    Degraded,
    /// Terminal clean stop: cooperative cancellation or clean completion.
    Stopped,
    /// Terminal failure: permanent error or exhausted restart budget.
    Unhealthy,
}

#[derive(Clone, Debug)]
pub struct ComponentStatus {
    pub state: ComponentState,
    pub restart_count: u32,
    pub last_error: Option<String>,
    pub last_start: Option<SystemTime>,
    pub last_heartbeat: Option<SystemTime>,
}

impl Default for ComponentStatus {
    fn default() -> Self {
        Self {
            state: ComponentState::Running,
            restart_count: 0,
            last_error: None,
            last_start: None,
            last_heartbeat: None,
        }
    }
}

/// Process-local component status registry. Operator-visible through
/// structured logs; the snapshot is in-process only — ADR-119 adds no generic
/// wire surface for it. ADR-106's narrow schedule-attempt timestamp is tracked
/// separately by `KhiveMcpServer` and is not a serialization of these rows.
#[derive(Clone, Default)]
pub struct HealthReporter {
    inner: Arc<Mutex<HashMap<&'static str, ComponentStatus>>>,
}

impl HealthReporter {
    fn with_entry(&self, name: &'static str, f: impl FnOnce(&mut ComponentStatus)) {
        let mut map = self.inner.lock().expect("component health lock");
        f(map.entry(name).or_default());
    }

    fn record_start(&self, name: &'static str, restart_count: u32) {
        self.with_entry(name, |s| {
            s.state = ComponentState::Running;
            s.restart_count = restart_count;
            s.last_start = Some(SystemTime::now());
        });
    }

    fn record_state(&self, name: &'static str, state: ComponentState, error: Option<String>) {
        self.with_entry(name, |s| {
            s.state = state;
            if error.is_some() {
                s.last_error = error;
            }
        });
    }

    fn heartbeat(&self, name: &'static str) {
        self.with_entry(name, |s| s.last_heartbeat = Some(SystemTime::now()));
    }

    pub fn status(&self, name: &str) -> Option<ComponentStatus> {
        self.inner
            .lock()
            .expect("component health lock")
            .get(name)
            .cloned()
    }

    pub fn snapshot(&self) -> Vec<(&'static str, ComponentStatus)> {
        let map = self.inner.lock().expect("component health lock");
        let mut rows: Vec<_> = map.iter().map(|(k, v)| (*k, v.clone())).collect();
        rows.sort_by_key(|(k, _)| *k);
        rows
    }
}

/// The process-wide reporter used by [`start_daemon_components`].
pub fn component_health() -> &'static HealthReporter {
    static HEALTH: OnceLock<HealthReporter> = OnceLock::new();
    HEALTH.get_or_init(HealthReporter::default)
}

/// Lifecycle and dispatch context handed to a component (ADR-119 Amendment 1).
///
/// Carries the daemon's dispatch handle plus the resolved actor and write
/// namespace, and the component's lifecycle services. It is deliberately not
/// a service locator: components needing more capture it in their own
/// registration crate.
///
/// The dispatch handle is [`KhiveMcpServer`]; its
/// [`dispatch_request_local`](KhiveMcpServer::dispatch_request_local) surface
/// is the in-process path that does not apply the wire-only
/// `Visibility::Subhandler` gate — a component can therefore reach
/// daemon-internal verbs (the ingest class) that the MCP wire surface
/// rejects.
#[derive(Clone)]
pub struct HostContext {
    server: KhiveMcpServer,
    actor: Option<String>,
    namespace: String,
    cancellation: CancellationToken,
    name: &'static str,
    health: HealthReporter,
}

impl HostContext {
    pub(crate) fn new(
        server: KhiveMcpServer,
        cancellation: CancellationToken,
        name: &'static str,
        health: HealthReporter,
    ) -> Self {
        Self {
            actor: server.actor_id().map(str::to_string),
            namespace: server.default_namespace().to_string(),
            server,
            cancellation,
            name,
            health,
        }
    }

    pub fn server(&self) -> &KhiveMcpServer {
        &self.server
    }

    pub fn actor(&self) -> Option<&str> {
        self.actor.as_deref()
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Cancelled when the daemon shuts down (or the supervisor is torn down).
    /// A component's main loop must observe this and return promptly; the
    /// host aborts the task after `shutdown_timeout_ms` otherwise.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Record a liveness heartbeat. A supervised loop should call this once
    /// per successful cycle so the health row distinguishes "alive and
    /// quiet" from "wedged" — a frozen `last_heartbeat` under a `Running`
    /// state is the wedge signal.
    pub fn heartbeat(&self) {
        self.health.heartbeat(self.name);
    }
}

/// Collect link-time registrations and start supervision for each under the
/// daemon's shutdown token. Returns the number of components started.
///
/// Always logs the enumerated roster (names and count) — including the empty
/// one — so zero-components-where-N-expected is one visible line at daemon
/// startup. Call only from a daemon-role process.
pub fn start_daemon_components(server: &KhiveMcpServer) -> usize {
    start_daemon_components_with_schedule(server, None)
}

/// Start linked components and the host-owned schedule and upload loops.
/// Dynamic registrations capture the exact registered pack state; rebuilding
/// a pack here would lose its in-memory state or use another backend.
pub(crate) fn start_daemon_components_with_schedule(
    server: &KhiveMcpServer,
    schedule_runtime: Option<khive_runtime::KhiveRuntime>,
) -> usize {
    let regs = component_registrations(schedule_runtime, server.blob_upload_manager());
    start_component_registrations(
        regs,
        server,
        khive_runtime::daemon_shutdown_token(),
        component_health().clone(),
    )
}

fn component_registrations(
    schedule_runtime: Option<khive_runtime::KhiveRuntime>,
    upload_manager: Option<Arc<khive_pack_blob::uploads::UploadManager>>,
) -> Vec<ComponentRegistration> {
    let linked: Vec<&'static DaemonComponentRegistration> =
        inventory::iter::<DaemonComponentRegistration>().collect();
    let mut regs: Vec<ComponentRegistration> = linked
        .into_iter()
        .map(ComponentRegistration::from)
        .collect();
    // Defense in depth for callers outside the normal serve constructor: the
    // schedule loop begins every tick with stale-claim reclaim DML, so a
    // read-only assigned runtime must never become a supervised component.
    if let Some(runtime) = schedule_runtime.filter(|runtime| !runtime.is_read_only()) {
        regs.push(schedule_component_registration(
            runtime,
            crate::pending_events::tick_interval_from_env(),
        ));
    }
    if let Some(manager) = upload_manager {
        regs.push(blob_upload_component_registration(manager));
    }
    regs
}

#[cfg(test)]
fn start_components(
    regs: &[&'static DaemonComponentRegistration],
    server: &KhiveMcpServer,
    parent: CancellationToken,
    health: HealthReporter,
) -> usize {
    start_component_registrations(
        regs.iter()
            .copied()
            .map(ComponentRegistration::from)
            .collect(),
        server,
        parent,
        health,
    )
}

fn start_component_registrations(
    regs: Vec<ComponentRegistration>,
    server: &KhiveMcpServer,
    parent: CancellationToken,
    health: HealthReporter,
) -> usize {
    let roster: Vec<&'static str> = regs.iter().map(|r| r.name).collect();
    tracing::info!(
        count = regs.len(),
        roster = ?roster,
        "daemon components: roster"
    );
    // Health rows are keyed by name, so two registrations sharing one name
    // write indistinguishable, interleaved state. Both still start — refusing
    // one would silently drop a real component — but the collision is a
    // linked-crate defect and gets an error-level line naming it.
    let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    for name in &roster {
        if !seen.insert(name) {
            tracing::error!(
                component = name,
                "daemon components: duplicate registration name; health rows \
                 for these components will overwrite each other"
            );
        }
    }
    let count = regs.len();
    for reg in regs {
        khive_runtime::track_named_background_task(
            "component_supervision",
            supervise(reg, server.clone(), parent.child_token(), health.clone()),
        );
    }
    count
}

/// Reserved slice of the drain window for post-grace supervisor work: the
/// task abort, the terminal health record, and drain()'s 100ms poll cadence
/// observing the exit. Per-component shutdown waits are clamped to
/// `drain_timeout() - this`, so a supervisor always finishes inside drain.
const SHUTDOWN_DRAIN_MARGIN_MS: u64 = 500;

/// Clamp a component's requested shutdown wait strictly inside the drain
/// window. A wait equal to the drain bound spends the whole window on the
/// grace wait, leaving no time for the abort, the terminal state record,
/// and drain()'s poll to observe the supervisor's exit — drain() would give
/// up with the supervisor still tracked.
fn clamped_shutdown_wait_ms(requested_ms: u64, drain_ms: u64) -> u64 {
    requested_ms.min(drain_ms.saturating_sub(SHUTDOWN_DRAIN_MARGIN_MS))
}

async fn supervise(
    reg: ComponentRegistration,
    server: KhiveMcpServer,
    token: CancellationToken,
    health: HealthReporter,
) {
    let mut restarts: u32 = 0;
    let mut backoff = Backoff::new(reg.backoff_initial_ms, reg.backoff_max_ms);
    let drain_ms = khive_runtime::daemon::drain_timeout().as_millis() as u64;
    let shutdown_wait_ms = clamped_shutdown_wait_ms(reg.shutdown_timeout_ms, drain_ms);
    if shutdown_wait_ms < reg.shutdown_timeout_ms {
        tracing::warn!(
            component = reg.name,
            requested_ms = reg.shutdown_timeout_ms,
            clamped_ms = shutdown_wait_ms,
            drain_bound_ms = drain_ms,
            "daemon component: shutdown timeout exceeds the drain bound; clamped"
        );
    }
    loop {
        health.record_start(reg.name, restarts);
        tracing::info!(
            component = reg.name,
            restart = restarts,
            "daemon component: starting"
        );
        let ctx = HostContext::new(server.clone(), token.clone(), reg.name, health.clone());
        // Invoke the factory behind an unwind boundary before spawning its
        // returned future. Argument evaluation happens before `tokio::spawn`,
        // so putting `(reg.start)(ctx)` directly in that call would let a
        // synchronous construction panic unwind the supervisor itself and
        // strand a false `Running` health row. ADR-119 makes construction
        // failure terminal rather than restartable.
        let component =
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (reg.start)(ctx))) {
                Ok(component) => component,
                Err(payload) => {
                    let detail = payload
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "non-string panic payload".to_string());
                    let error = format!("component construction panicked: {detail}");
                    health.record_state(reg.name, ComponentState::Unhealthy, Some(error.clone()));
                    tracing::error!(
                        component = reg.name,
                        error = %error,
                        "daemon component: construction failed; terminally unhealthy"
                    );
                    return;
                }
            };
        // A separate task per run isolates panics raised while polling the
        // returned component future: they surface as JoinError here instead
        // of unwinding through the supervisor.
        let mut handle = tokio::spawn(component);

        let joined = tokio::select! {
            r = &mut handle => Some(r),
            _ = token.cancelled() => {
                match tokio::time::timeout(
                    Duration::from_millis(shutdown_wait_ms),
                    &mut handle,
                )
                .await
                {
                    Ok(r) => Some(r),
                    Err(_) => {
                        handle.abort();
                        let _ = (&mut handle).await;
                        None
                    }
                }
            }
        };

        if token.is_cancelled() {
            match &joined {
                // Ignored cancellation until the host aborted it: the wedge
                // was real, not cooperative — terminally unhealthy so a
                // frozen loop is visible post-mortem, never a clean stop.
                None => {
                    let msg = format!("aborted: ignored cancellation for {shutdown_wait_ms}ms");
                    tracing::error!(
                        component = reg.name,
                        timeout_ms = shutdown_wait_ms,
                        "daemon component: ignored cancellation past its shutdown \
                         timeout; aborted (terminally unhealthy)"
                    );
                    health.record_state(reg.name, ComponentState::Unhealthy, Some(msg));
                }
                // Cooperative stop: whatever the component returned while
                // stopping, this is a shutdown, not a failure — no budget
                // consumed.
                Some(joined) => {
                    if let Ok(Err(e)) = joined {
                        tracing::info!(component = reg.name, error = %e, "daemon component: error during shutdown (ignored)");
                    }
                    health.record_state(reg.name, ComponentState::Stopped, None);
                    tracing::info!(component = reg.name, "daemon component: stopped (shutdown)");
                }
            }
            return;
        }

        let error = match joined.expect("abort only happens on the cancelled path") {
            Ok(Ok(())) => {
                // Long-running components are not expected to finish.
                health.record_state(reg.name, ComponentState::Stopped, None);
                tracing::warn!(
                    component = reg.name,
                    "daemon component: completed cleanly outside shutdown"
                );
                return;
            }
            Ok(Err(ComponentError::Permanent(e))) => {
                health.record_state(reg.name, ComponentState::Unhealthy, Some(e.clone()));
                tracing::error!(
                    component = reg.name,
                    error = %e,
                    "daemon component: permanent failure; terminally unhealthy"
                );
                return;
            }
            Ok(Err(ComponentError::Retryable(e))) => e,
            Err(join_err) => format!("component task failed: {join_err}"),
        };

        health.record_state(reg.name, ComponentState::Degraded, Some(error.clone()));
        let out_of_budget = reg.restart != RestartClass::OnFailure || restarts >= reg.max_restarts;
        if out_of_budget {
            health.record_state(reg.name, ComponentState::Unhealthy, Some(error.clone()));
            tracing::error!(
                component = reg.name,
                error = %error,
                restarts,
                "daemon component: failure with no restart remaining; terminally unhealthy"
            );
            return;
        }

        restarts += 1;
        let delay = Duration::from_millis(backoff.delay_ms());
        tracing::warn!(
            component = reg.name,
            error = %error,
            restart = restarts,
            backoff_ms = delay.as_millis() as u64,
            "daemon component: retryable failure; restarting after backoff"
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = token.cancelled() => {
                health.record_state(reg.name, ComponentState::Stopped, None);
                tracing::info!(component = reg.name, "daemon component: stopped during backoff (shutdown)");
                return;
            }
        }
        backoff.advance();
    }
}

#[cfg(all(test, any(feature = "channel-email", feature = "channel-telegram")))]
#[path = "components_outbox_tests.rs"]
mod outbox_tests;

#[cfg(test)]
#[path = "components_tests.rs"]
mod tests;
