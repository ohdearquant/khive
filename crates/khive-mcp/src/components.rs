//! Daemon component registry (ADR-119): host-supervised, daemon-role-only
//! long-running work beside the verb plane.
//!
//! A daemon component is not a verb. It is host-constructed background work
//! (channel ingest loops, drains, maintenance scans) supervised by this
//! registry for cancellation, restart budgets, backoff, health, and bounded
//! shutdown. External components register at link time through `inventory`
//! (ADR-119 Amendment 1), so a distribution binary's components participate
//! without this crate naming any of them. The host additionally contributes
//! the dynamic `schedule-tick` registration when its resolved pack set carries
//! a schedule runtime (ADR-119 Amendment 4); a plain core build still has an
//! empty external inventory.
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
//! Startup ordering caveat: components start on the serve path after the
//! boot guard is acquired but before the daemon finishes establishing
//! ownership (socket bind, pid write). A process that fails establishment
//! exits through `ComponentTeardown` — components are cancelled, but may
//! have run briefly first. Side-effecting components (the ingest class)
//! must therefore be idempotent under that window: work emitted by a
//! process that never became the daemon may be performed again by the one
//! that does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use crate::server::KhiveMcpServer;

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

/// Start the linked daemon components plus the host-owned schedule drain when
/// the daemon resolved the `schedule` pack. The schedule component is dynamic
/// because it must capture that exact pack runtime; reconstructing a runtime
/// here can point the drain at a different backend (ADR-106, PR #782).
pub(crate) fn start_daemon_components_with_schedule(
    server: &KhiveMcpServer,
    schedule_runtime: Option<khive_runtime::KhiveRuntime>,
) -> usize {
    let regs = component_registrations(schedule_runtime);
    start_component_registrations(
        regs,
        server,
        khive_runtime::daemon_shutdown_token(),
        component_health().clone(),
    )
}

fn component_registrations(
    schedule_runtime: Option<khive_runtime::KhiveRuntime>,
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
        khive_runtime::track_background_task(supervise(
            reg,
            server.clone(),
            parent.child_token(),
            health.clone(),
        ));
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

/// Deterministic-enough restart jitter without a rand dependency: up to a
/// quarter of the current backoff, derived from the clock's subsecond nanos.
fn jitter_ms(backoff_ms: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % (backoff_ms / 4 + 1)
}

/// Apply positive jitter without ever exceeding the registration's hard
/// backoff cap. `backoff_max_ms` is an operator-facing maximum delay, not only
/// a cap on the un-jittered base; allowing `base + jitter` past it makes a
/// documented 60-second ceiling reach 75 seconds at steady state.
fn restart_delay_ms(backoff_ms: u64, backoff_max_ms: u64) -> u64 {
    let cap = backoff_max_ms.max(1);
    let base = backoff_ms.min(cap);
    base.saturating_add(jitter_ms(base)).min(cap)
}

async fn supervise(
    reg: ComponentRegistration,
    server: KhiveMcpServer,
    token: CancellationToken,
    health: HealthReporter,
) {
    let mut restarts: u32 = 0;
    let mut backoff_ms = reg.backoff_initial_ms.clamp(1, reg.backoff_max_ms.max(1));
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
        let delay = Duration::from_millis(restart_delay_ms(backoff_ms, reg.backoff_max_ms));
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
        backoff_ms = backoff_ms.saturating_mul(2).min(reg.backoff_max_ms.max(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    fn tmp_db() -> (TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("khive-test.db");
        let path = path.to_str().expect("utf8 path").to_string();
        (dir, path)
    }

    #[cfg(unix)]
    use khive_storage::test_support::freeze_snapshot_sidecars;

    async fn make_server(db_path: &str) -> KhiveMcpServer {
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(db_path)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            actor_id: Some("actor:component-test".to_string()),
            packs: vec!["kg".to_string()],
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime");
        KhiveMcpServer::new(rt).expect("server")
    }

    async fn wait_for_state(
        health: &HealthReporter,
        name: &str,
        state: ComponentState,
    ) -> ComponentStatus {
        for _ in 0..400 {
            if let Some(s) = health.status(name) {
                if s.state == state {
                    return s;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "component {name} never reached {state:?}; last = {:?}",
            health.status(name)
        );
    }

    #[test]
    fn shutdown_wait_is_clamped_strictly_inside_the_drain_window() {
        let drain_ms = 10_000;
        // A request equal to the drain bound is the failure case: the grace
        // wait would consume the whole window with no time left for the
        // abort and terminal state record.
        assert_eq!(
            clamped_shutdown_wait_ms(drain_ms, drain_ms),
            drain_ms - SHUTDOWN_DRAIN_MARGIN_MS
        );
        assert_eq!(
            clamped_shutdown_wait_ms(u64::MAX, drain_ms),
            drain_ms - SHUTDOWN_DRAIN_MARGIN_MS
        );
        // Requests already inside the bound pass through unchanged.
        assert_eq!(clamped_shutdown_wait_ms(100, drain_ms), 100);
        assert_eq!(
            clamped_shutdown_wait_ms(drain_ms - SHUTDOWN_DRAIN_MARGIN_MS, drain_ms),
            drain_ms - SHUTDOWN_DRAIN_MARGIN_MS
        );
        // A drain window smaller than the margin degrades to an immediate
        // abort rather than underflowing.
        assert_eq!(
            clamped_shutdown_wait_ms(100, SHUTDOWN_DRAIN_MARGIN_MS / 2),
            0
        );
    }

    #[test]
    fn restart_delay_including_jitter_never_exceeds_hard_cap() {
        for base in [1, 1_000, 30_000, 59_999, 60_000, u64::MAX] {
            assert!(
                restart_delay_ms(base, SCHEDULE_BACKOFF_MAX_MS) <= SCHEDULE_BACKOFF_MAX_MS,
                "base={base} exceeded the documented 60-second schedule restart cap"
            );
        }
        assert_eq!(
            restart_delay_ms(u64::MAX, u64::MAX),
            u64::MAX,
            "saturating jitter arithmetic must remain overflow-safe"
        );
    }

    #[test]
    fn tmp_db_guard_removes_database_and_all_sidecars() {
        let (dir, db_path) = tmp_db();
        let dir_path = dir.path().to_path_buf();
        for suffix in ["", "-wal", "-shm", ".khive-blob-gc.lock"] {
            std::fs::write(format!("{db_path}{suffix}"), b"fixture").expect("write fixture");
        }

        drop(dir);

        assert!(
            !dir_path.exists(),
            "dropping the directory guard must remove the database and every sibling sidecar"
        );
    }

    #[tokio::test]
    async fn empty_registration_set_is_a_no_op() {
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        let started = start_components(&[], &server, CancellationToken::new(), health.clone());
        assert_eq!(started, 0);
        assert!(health.snapshot().is_empty());
    }

    #[test]
    #[serial_test::serial(config_ledger)]
    fn schedule_roster_contains_exactly_one_dynamic_component_when_resolved() {
        let (_f, db) = tmp_db();
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(db)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime");

        let absent = component_registrations(None)
            .into_iter()
            .filter(|reg| reg.name == SCHEDULE_COMPONENT_NAME)
            .count();
        let present: Vec<_> = component_registrations(Some(rt))
            .into_iter()
            .filter(|reg| reg.name == SCHEDULE_COMPONENT_NAME)
            .collect();

        assert_eq!(absent, 0, "pack-absent roster must omit schedule-tick");
        assert_eq!(
            present.len(),
            1,
            "resolved schedule pack contributes one ticker"
        );
        let reg = &present[0];
        assert_eq!(reg.restart, RestartClass::OnFailure);
        assert_eq!(reg.max_restarts, 5);
        assert_eq!(reg.backoff_initial_ms, 1_000);
        assert_eq!(reg.backoff_max_ms, 60_000);
        assert_eq!(reg.shutdown_timeout_ms, 5_000);
    }

    #[test]
    #[serial_test::serial(config_ledger)]
    fn schedule_roster_omits_read_only_assigned_runtime_without_writer_acquisition() {
        let (_f, db) = tmp_db();
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(db)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        };
        KhiveRuntime::new(cfg.clone()).expect("create and migrate snapshot source");
        #[cfg(unix)]
        freeze_snapshot_sidecars(cfg.db_path.as_ref().expect("db path"));
        let read_only = KhiveRuntime::new_readonly(cfg).expect("open schedule snapshot read-only");
        let before = read_only.backend().pool().writer_acquisition_snapshot();

        let schedule_count = component_registrations(Some(read_only.clone()))
            .into_iter()
            .filter(|reg| reg.name == SCHEDULE_COMPONENT_NAME)
            .count();

        assert_eq!(
            schedule_count, 0,
            "a read-only schedule backend must not launch the writer-dependent ticker"
        );
        assert_eq!(
            read_only.backend().pool().writer_acquisition_snapshot(),
            before,
            "component roster construction must not probe a read-only backend through a writer"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn supervised_schedule_component_heartbeats_and_stops_cooperatively() {
        let (_f, db) = tmp_db();
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime");
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let health = HealthReporter::default();
        let parent = CancellationToken::new();

        let started = start_component_registrations(
            vec![schedule_component_registration(
                rt,
                Duration::from_millis(10),
            )],
            &server,
            parent.clone(),
            health.clone(),
        );
        assert_eq!(started, 1);
        for _ in 0..400 {
            if health
                .status(SCHEDULE_COMPONENT_NAME)
                .is_some_and(|status| status.last_heartbeat.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            health
                .status(SCHEDULE_COMPONENT_NAME)
                .is_some_and(|status| status.last_heartbeat.is_some()),
            "quiet agenda drains must still prove liveness"
        );

        parent.cancel();
        let status =
            wait_for_state(&health, SCHEDULE_COMPONENT_NAME, ComponentState::Stopped).await;
        assert_eq!(status.restart_count, 0);
        assert!(status.last_error.is_none());
        let stopped_heartbeat = status
            .last_heartbeat
            .expect("schedule component heartbeated before shutdown");
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            health
                .status(SCHEDULE_COMPONENT_NAME)
                .and_then(|status| status.last_heartbeat),
            Some(stopped_heartbeat),
            "the inner ticker must be joined before the supervisor reports Stopped; no \
             schedule task may survive component shutdown"
        );
    }

    static DUP_A_RUNS: AtomicU32 = AtomicU32::new(0);
    static DUP_B_RUNS: AtomicU32 = AtomicU32::new(0);
    fn dup_a(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async {
            DUP_A_RUNS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn dup_b(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async {
            DUP_B_RUNS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn duplicate_names_are_flagged_but_both_components_still_start() {
        static REG_A: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-dup",
            restart: RestartClass::Never,
            max_restarts: 0,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: dup_a,
        };
        static REG_B: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-dup",
            restart: RestartClass::Never,
            max_restarts: 0,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: dup_b,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        let started = start_components(
            &[&REG_A, &REG_B],
            &server,
            CancellationToken::new(),
            health.clone(),
        );
        assert_eq!(started, 2);
        // The collision is reported (error log), never resolved by dropping a
        // registration: both components must run.
        wait_for_state(&health, "test-dup", ComponentState::Stopped).await;
        for _ in 0..400 {
            if DUP_A_RUNS.load(Ordering::SeqCst) == 1 && DUP_B_RUNS.load(Ordering::SeqCst) == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "both duplicate-named components should have started; a={} b={}",
            DUP_A_RUNS.load(Ordering::SeqCst),
            DUP_B_RUNS.load(Ordering::SeqCst)
        );
    }

    static CLEAN_RUNS: AtomicU32 = AtomicU32::new(0);
    fn clean_component(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async {
            CLEAN_RUNS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn clean_completion_is_terminal_stopped_without_restart() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-clean",
            restart: RestartClass::OnFailure,
            max_restarts: 5,
            backoff_initial_ms: 1,
            backoff_max_ms: 4,
            shutdown_timeout_ms: 100,
            start: clean_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        start_components(&[&REG], &server, CancellationToken::new(), health.clone());
        let status = wait_for_state(&health, "test-clean", ComponentState::Stopped).await;
        assert_eq!(status.restart_count, 0);
        assert_eq!(CLEAN_RUNS.load(Ordering::SeqCst), 1);
    }

    static RETRY_RUNS: AtomicU32 = AtomicU32::new(0);
    fn retryable_component(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async {
            RETRY_RUNS.fetch_add(1, Ordering::SeqCst);
            Err(ComponentError::Retryable("boom".into()))
        })
    }

    #[tokio::test]
    async fn retryable_failures_consume_budget_then_terminal_unhealthy() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-retry",
            restart: RestartClass::OnFailure,
            max_restarts: 2,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: retryable_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        start_components(&[&REG], &server, CancellationToken::new(), health.clone());
        let status = wait_for_state(&health, "test-retry", ComponentState::Unhealthy).await;
        // initial run + 2 budgeted restarts, then terminal — no hot loop.
        assert_eq!(RETRY_RUNS.load(Ordering::SeqCst), 3);
        assert_eq!(status.restart_count, 2);
        assert_eq!(status.last_error.as_deref(), Some("boom"));
    }

    static PERMANENT_RUNS: AtomicU32 = AtomicU32::new(0);
    fn permanent_component(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async {
            PERMANENT_RUNS.fetch_add(1, Ordering::SeqCst);
            Err(ComponentError::Permanent("bad config".into()))
        })
    }

    #[tokio::test]
    async fn permanent_failure_is_immediately_terminal_despite_budget() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-permanent",
            restart: RestartClass::OnFailure,
            max_restarts: 5,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: permanent_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        start_components(&[&REG], &server, CancellationToken::new(), health.clone());
        let status = wait_for_state(&health, "test-permanent", ComponentState::Unhealthy).await;
        assert_eq!(PERMANENT_RUNS.load(Ordering::SeqCst), 1);
        assert_eq!(status.restart_count, 0);
    }

    fn never_restart_component(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async { Err(ComponentError::Retryable("one shot".into())) })
    }

    #[tokio::test]
    async fn restart_class_never_makes_any_failure_terminal() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-never",
            restart: RestartClass::Never,
            max_restarts: 5,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: never_restart_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        start_components(&[&REG], &server, CancellationToken::new(), health.clone());
        let status = wait_for_state(&health, "test-never", ComponentState::Unhealthy).await;
        assert_eq!(status.restart_count, 0);
    }

    fn cooperative_component(ctx: HostContext) -> ComponentFuture {
        Box::pin(async move {
            loop {
                tokio::select! {
                    _ = ctx.cancellation().cancelled() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_millis(2)) => ctx.heartbeat(),
                }
            }
        })
    }

    #[tokio::test]
    async fn cooperative_cancellation_stops_cleanly_and_heartbeats_recorded() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-coop",
            restart: RestartClass::OnFailure,
            max_restarts: 5,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 500,
            start: cooperative_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        let parent = CancellationToken::new();
        start_components(&[&REG], &server, parent.clone(), health.clone());
        // Let it run a few cycles so a heartbeat lands.
        for _ in 0..200 {
            if health
                .status("test-coop")
                .is_some_and(|s| s.last_heartbeat.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        parent.cancel();
        let status = wait_for_state(&health, "test-coop", ComponentState::Stopped).await;
        assert_eq!(status.restart_count, 0);
        assert!(status.last_heartbeat.is_some());
        assert!(status.last_error.is_none());
    }

    fn hung_component(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async {
            // Ignores cancellation entirely.
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(())
        })
    }

    #[tokio::test]
    async fn hung_component_is_aborted_after_shutdown_timeout() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-hung",
            restart: RestartClass::OnFailure,
            max_restarts: 5,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 30,
            start: hung_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        let parent = CancellationToken::new();
        start_components(&[&REG], &server, parent.clone(), health.clone());
        // Give it a moment to start, then order shutdown.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = std::time::Instant::now();
        parent.cancel();
        // An abort after ignoring cancellation is a real wedge — terminally
        // unhealthy with the abort recorded, never a clean stop.
        let status = wait_for_state(&health, "test-hung", ComponentState::Unhealthy).await;
        assert!(status.last_error.as_deref().unwrap().contains("aborted"));
        // Bounded: well under the hour the component wanted.
        assert!(before.elapsed() < Duration::from_secs(5));
    }

    static OVERFLOW_RUNS: AtomicU32 = AtomicU32::new(0);
    fn overflow_component(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async {
            OVERFLOW_RUNS.fetch_add(1, Ordering::SeqCst);
            Err(ComponentError::Retryable(
                "push the backoff arithmetic".into(),
            ))
        })
    }

    #[tokio::test]
    async fn extreme_backoff_values_never_panic_the_supervisor() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-overflow",
            restart: RestartClass::OnFailure,
            max_restarts: 3,
            backoff_initial_ms: u64::MAX,
            backoff_max_ms: u64::MAX,
            shutdown_timeout_ms: u64::MAX,
            start: overflow_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        let parent = CancellationToken::new();
        start_components(&[&REG], &server, parent.clone(), health.clone());
        // First failure lands Degraded, then the supervisor sits in a
        // (saturating, non-panicking) enormous backoff sleep.
        let status = wait_for_state(&health, "test-overflow", ComponentState::Degraded).await;
        assert_eq!(status.restart_count, 0);
        assert_eq!(OVERFLOW_RUNS.load(Ordering::SeqCst), 1);
        // Cancellation during backoff still stops cleanly — proving the
        // supervisor survived the arithmetic instead of panicking past
        // Degraded.
        parent.cancel();
        wait_for_state(&health, "test-overflow", ComponentState::Stopped).await;
    }

    static PANIC_RUNS: AtomicU32 = AtomicU32::new(0);
    fn panicking_component(_ctx: HostContext) -> ComponentFuture {
        Box::pin(async {
            PANIC_RUNS.fetch_add(1, Ordering::SeqCst);
            panic!("component panic");
        })
    }

    #[tokio::test]
    async fn panic_is_isolated_and_classified_retryable() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-panic",
            restart: RestartClass::OnFailure,
            max_restarts: 1,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: panicking_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        start_components(&[&REG], &server, CancellationToken::new(), health.clone());
        let status = wait_for_state(&health, "test-panic", ComponentState::Unhealthy).await;
        assert_eq!(PANIC_RUNS.load(Ordering::SeqCst), 2);
        assert!(status.last_error.as_deref().unwrap().contains("panic"));
    }

    #[tokio::test]
    async fn synchronous_factory_panic_is_terminal_and_sibling_survives() {
        let healthy_cycles = Arc::new(AtomicU32::new(0));
        let healthy_cycles_for_start = healthy_cycles.clone();
        let healthy = ComponentRegistration {
            name: "test-isolated-healthy",
            restart: RestartClass::Never,
            max_restarts: 0,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: Arc::new(move |ctx: HostContext| -> ComponentFuture {
                let cycles = healthy_cycles_for_start.clone();
                Box::pin(async move {
                    loop {
                        tokio::select! {
                            _ = ctx.cancellation().cancelled() => return Ok(()),
                            _ = tokio::time::sleep(Duration::from_millis(2)) => {
                                cycles.fetch_add(1, Ordering::SeqCst);
                                ctx.heartbeat();
                            }
                        }
                    }
                })
            }),
        };
        let failing = ComponentRegistration {
            name: "test-isolated-failing",
            restart: RestartClass::Never,
            max_restarts: 0,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: Arc::new(|_ctx: HostContext| -> ComponentFuture {
                panic!("synchronous component construction panic");
            }),
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        let parent = CancellationToken::new();
        assert_eq!(
            start_component_registrations(
                vec![failing, healthy],
                &server,
                parent.clone(),
                health.clone(),
            ),
            2
        );

        let failed =
            wait_for_state(&health, "test-isolated-failing", ComponentState::Unhealthy).await;
        assert_eq!(
            failed.restart_count, 0,
            "construction failures are terminal and must not spend restart budget"
        );
        assert!(
            failed
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("construction panicked")),
            "construction panic must be named in terminal health: {failed:?}"
        );
        for _ in 0..400 {
            if healthy_cycles.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            healthy_cycles.load(Ordering::SeqCst) >= 2,
            "one component's construction panic must not cancel or starve an independent sibling"
        );
        assert_eq!(
            health
                .status("test-isolated-healthy")
                .map(|status| status.state),
            Some(ComponentState::Running)
        );

        parent.cancel();
        wait_for_state(&health, "test-isolated-healthy", ComponentState::Stopped).await;
    }

    static DISPATCH_OK: AtomicU32 = AtomicU32::new(0);
    fn dispatching_component(ctx: HostContext) -> ComponentFuture {
        Box::pin(async move {
            // The in-process dispatch path — the surface that skips the
            // wire-only Subhandler visibility gate (daemon-internal
            // capability; the ingest class in distribution builds).
            let params = crate::tools::request::RequestParams {
                ops: "create(kind=\"concept\", name=\"component-dispatch-probe\") \
                      | get(id=$prev.id)"
                    .to_string(),
                ..Default::default()
            };
            let out = ctx
                .server()
                .dispatch_request_local(params)
                .await
                .map_err(|e| ComponentError::Retryable(e.to_string()))?;
            if out.contains("component-dispatch-probe") {
                DISPATCH_OK.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }

    #[tokio::test]
    async fn host_context_dispatch_handle_lands_a_write() {
        static REG: DaemonComponentRegistration = DaemonComponentRegistration {
            name: "test-dispatch",
            restart: RestartClass::Never,
            max_restarts: 0,
            backoff_initial_ms: 1,
            backoff_max_ms: 2,
            shutdown_timeout_ms: 100,
            start: dispatching_component,
        };
        let (_f, db) = tmp_db();
        let server = make_server(&db).await;
        let health = HealthReporter::default();
        start_components(&[&REG], &server, CancellationToken::new(), health.clone());
        let status = wait_for_state(&health, "test-dispatch", ComponentState::Stopped).await;
        assert!(status.last_error.is_none(), "dispatch failed: {status:?}");
        assert_eq!(DISPATCH_OK.load(Ordering::SeqCst), 1);
    }
}
