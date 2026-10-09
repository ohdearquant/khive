//! Build the runtime + server from CLI args and serve over the selected transport.
//!
//! This is the bootstrap that the `kkernel mcp` subcommand drives. Logging is
//! initialized by the binary, not here.

#[path = "serve/claimed_backend.rs"]
mod claimed_backend;
#[path = "serve/disk_policy.rs"]
mod disk_policy;
#[cfg(unix)]
#[path = "serve/events_socket_preflight.rs"]
mod events_socket_preflight;
#[path = "serve/gate_boot_disclosure.rs"]
mod gate_boot_disclosure;

#[cfg(feature = "channel-telegram")]
#[path = "serve/telegram.rs"]
mod telegram;
#[cfg(feature = "channel-telegram")]
pub(crate) use telegram::telegram_outbox_loop;
#[cfg(feature = "channel-telegram")]
use telegram::*;

use disk_policy::{disk_guard_numbers, open_backend_with_policies, validate_disk_guard_topology};
#[cfg(test)]
use disk_policy::{open_backend, open_backend_with_wal_ceiling};
#[cfg(unix)]
pub use events_socket_preflight::{
    preflight_events_socket_for_boot, prepare_preflighted_daemon_store_plan,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use khive_runtime::{
    config_from_env, parse_pack_list, runtime_config_from_khive_config, BackendConfig, BackendId,
    BackendKind, BlobHydrator, ConnectionPool, KhiveConfig, KhiveRuntime, OpenedDiagnosticBackend,
    OutboundEmailPolicy, OutputFormat, RuntimeConfig, StorageBackend,
};

use crate::args::{resolve_cli_namespace, Args};
use crate::server::KhiveMcpServer;
use crate::transport::{ServeOptions, TransportRegistry};

/// Output of [`build_registry_for_multi_backend`] — carries the registry and
/// the per-pack runtimes so `kkernel` can build a `BackendRegistry` for the
/// coordinator (ADR-029 Phase 2).
pub struct MultiBackendRegistry {
    /// The assembled [`khive_runtime::VerbRegistry`] ready to be passed to a server.
    pub registry: khive_runtime::VerbRegistry,
    /// Namespace the registry was built for.
    pub default_namespace: String,
    /// Config fingerprint (for daemon matching).
    pub config_id: String,
    /// Pack-name → `Arc<KhiveRuntime>`, one entry per declared pack.
    pub per_pack_runtimes: HashMap<String, Arc<KhiveRuntime>>,
    /// The `main` backend (needed by the coordinator to build the BackendRegistry).
    pub main_backend: Arc<StorageBackend>,
    /// The default runtime this boot built alongside the per-pack runtimes.
    /// A clone (cheap: internal state is `Arc<RwLock<_>>`-shared), kept so
    /// callers — chiefly boot-wiring tests — can assert on its installed
    /// state (e.g. `has_note_write_validator()`) without re-deriving it.
    pub default_runtime: KhiveRuntime,
}

/// Stable machine code for a concrete database override that conflicts with
/// an already-declared multi-backend topology.
pub const DB_OVERRIDE_CONFLICT_CODE: &str = "database_override_conflict";

/// Invocation-level refusal raised before any verb is dispatched when a
/// concrete `--db`/`KHIVE_DB` value would collapse declared backends.
///
/// The `config_source` the envelope reports is the canonicalized selected
/// file path (`diagnostic_config_path`): under symlinks it can diverge from
/// the path the operator typed.
#[derive(Debug)]
pub struct DatabaseOverrideConflict {
    db_override: String,
    backend_count: usize,
    config_source: Option<PathBuf>,
}

impl DatabaseOverrideConflict {
    fn new(
        db_override: &str,
        backend_count: usize,
        config_source: Option<&std::path::Path>,
    ) -> Self {
        Self {
            db_override: db_override.to_owned(),
            backend_count,
            config_source: config_source.map(std::path::Path::to_path_buf),
        }
    }

    /// Stable JSON shape emitted by `kkernel exec` when dispatch never began.
    pub fn envelope(&self) -> serde_json::Value {
        let config_path = self
            .config_source
            .as_deref()
            .map(|path| path.display().to_string());
        serde_json::json!({
            "ok": false,
            "invocation": {
                "started": false,
            },
            "error": {
                "code": DB_OVERRIDE_CONFLICT_CODE,
                "message": self.to_string(),
                "db_override": self.db_override.as_str(),
                "declared_backends": self.backend_count,
                "config_path": config_path,
            },
        })
    }
}

impl std::fmt::Display for DatabaseOverrideConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "--db {:?} (or KHIVE_DB) cannot be combined with [[backends]]: {} \
             backend(s) are already declared in the discovered config, so applying this \
             override here is ambiguous (it could silently collapse distinct declared \
             backends onto a single file). Remedy: edit the backend paths in the \
             discovered config file (searched in order: ./khive.toml, \
             <db-dir>/config.toml, ~/.khive/config.toml), or point at a different config \
             with --config <file> / KHIVE_CONFIG.",
            self.db_override, self.backend_count
        )?;
        if let Some(path) = self.config_source.as_deref() {
            write!(
                formatter,
                " This invocation selected the config file at {}; the backends above \
                 are declared there.",
                path.display()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for DatabaseOverrideConflict {}

/// Return the stable invocation-refusal envelope when `error` carries a
/// [`DatabaseOverrideConflict`] anywhere in its source chain, not only as
/// the top-level error: an intermediate carrier that adds context
/// (`anyhow::Context`) must not silently degrade the documented JSON
/// refusal to a generic error rendering.
pub fn db_override_refusal_envelope(error: &anyhow::Error) -> Option<serde_json::Value> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DatabaseOverrideConflict>())
        .map(DatabaseOverrideConflict::envelope)
}

/// Build a server from `args`, then serve it over `--daemon` or the named transport.
///
/// #667: `build_server` runs migrations and applies pack schema plans (FTS DDL
/// included) while constructing the runtime. Acquiring the boot/recovery lock
/// *before* that call and holding it through daemon bind+pid-write (or
/// dropping it right after construction in non-daemon mode) closes the window
/// where a second concurrently-booting process could run schema DDL against
/// the same database file at the same time — see
/// [`khive_runtime::daemon::run_daemon_with_boot_guard`].
/// Run the server using the supplied transport registry.
///
/// Library hosts enabling file-backed daemon boot on Unix must initialize
/// `khive_db::pool::initialize_claimed_file_observer` before any SQLite I/O,
/// following its unsafe startup contract. The stock `kkernel` binary does so.
pub async fn run(args: Args, registry: &TransportRegistry) -> anyhow::Result<()> {
    #[cfg(unix)]
    if !args.daemon && args.transport.as_deref().unwrap_or("stdio") == "stdio" {
        crate::daemon::capture_bridge_executable();
    }
    if let Some(generation) = args.resumed_generation {
        tracing::warn!(
            generation,
            "bridge self-heal: this process is a resumed generation of an \
             in-place re-exec triggered by bridge self-heal"
        );
    }
    // #667: in daemon mode, failing to acquire the boot guard must abort
    // before runtime construction runs migrations/FTS DDL unguarded. #3069:
    // the resolved store claims below also precede that construction and stay
    // held while the daemon serves, even when a second process uses another
    // HOME and therefore has a different socket/boot lock. Non-daemon callers
    // keep the existing best-effort boot lock only.
    if args.daemon {
        khive_runtime::daemon::mark_warm_index_host();
    }
    let (cli_ns_explicit, cli_ns) =
        resolve_cli_namespace(&args).map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut prepared = prepare_server_boot(&args, cli_ns, cli_ns_explicit, cli_ns_explicit)?;
    #[cfg(unix)]
    let store_plan = if args.daemon {
        Some(prepare_preflighted_daemon_store_plan(
            &mut prepared.config,
            &mut prepared.db_anchor,
            &mut prepared.khive_cfg.backends,
            args.db.as_deref() == Some(":memory:"),
        )?)
    } else {
        None
    };
    #[cfg(unix)]
    let boot_guard = if args.daemon {
        Some(khive_runtime::daemon::acquire_daemon_boot_guard()?)
    } else {
        khive_runtime::daemon::acquire_recovery_lock()
    };
    #[cfg(unix)]
    if args.daemon {
        crate::daemon::refuse_serving_socket_before_store_claim().await?;
    }
    #[cfg(unix)]
    let store_guards = if let Some(plan) = store_plan {
        let mut guards = khive_runtime::daemon::claim_stores(&plan.paths, &plan.read_only_paths)?;
        plan.assert_aliases_unchanged()?;
        khive_runtime::daemon::bind_daemon_store_files(&mut guards, &plan.read_only_paths)?;
        // Refuse a stable retarget before constructing a pool. The pool later
        // opens SQLite by pathname and sets journal_mode inside its constructor.
        khive_runtime::daemon::assert_daemon_store_identities(&guards)?;
        Some(guards)
    } else {
        None
    };
    #[cfg(unix)]
    let daemon_claims = store_guards.as_deref();
    #[cfg(not(unix))]
    let daemon_claims = None;
    let (server, schedule_rt) =
        build_server_from_prepared(&args, prepared, true, daemon_claims).await?;
    #[cfg(unix)]
    if let Some(guards) = store_guards.as_deref() {
        khive_runtime::daemon::assert_daemon_store_identities(guards)?;
    }
    tracing::info!(target: "khive.boot", "{}", resolved_actor_disclosure(server.actor_id()));

    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon_with_options_and_boot_guard_and_start(
            server,
            boot_guard,
            args.daemon_options(),
            |server| start_host_background_tasks(&args, server, schedule_rt),
        )
        .await?;
        return Ok(());
    }
    #[cfg(unix)]
    drop(boot_guard);
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!(
            "--daemon mode requires Unix (macOS/Linux). On Windows, use the stdio transport."
        );
    }

    // ADR-091 Amendment 2 Plank A: every non-daemon process runs the
    // observe-only session sweep (never PASSIVE/TRUNCATE checkpointing —
    // that stays daemon-owned).
    start_host_background_tasks(&args, &server, schedule_rt);
    serve_with_session_sweep(server, &args, registry).await
}

fn daemon_startup_report(
    args: &Args,
    server: &KhiveMcpServer,
    has_schedule: bool,
) -> khive_runtime::daemon::DaemonStartupReport {
    let mut report = khive_runtime::daemon::DaemonStartupReport::default();
    if args.daemon
        && args.daemon_options().lifetime == khive_runtime::daemon::DaemonLifetime::Demand
    {
        #[cfg(feature = "channel-email")]
        report.skipped_components.extend([
            "email_channel_poll".to_owned(),
            "email_channel_outbound".to_owned(),
        ]);
        #[cfg(feature = "channel-telegram")]
        report.skipped_components.extend([
            "telegram_channel_poll".to_owned(),
            "telegram_channel_outbound".to_owned(),
        ]);
        if has_schedule {
            report.skipped_components.push("schedule-tick".to_owned());
        }
        report.idle_ineligible_reasons = crate::components::idle_retirement_obligations(server);
        #[cfg(unix)]
        if !server.default_runtime_is_read_only()
            && server
                .events_split_config()
                .is_some_and(|split| split.socket_path.is_some())
        {
            report
                .idle_ineligible_reasons
                .push("events_child_may_be_exclusively_owned".to_owned());
        }
    }
    report
}

fn start_host_background_tasks(
    args: &Args,
    server: &KhiveMcpServer,
    schedule_rt: Option<KhiveRuntime>,
) -> khive_runtime::daemon::DaemonStartupReport {
    let report = daemon_startup_report(args, server, schedule_rt.is_some());
    if args.daemon
        && args.daemon_options().lifetime == khive_runtime::daemon::DaemonLifetime::Demand
    {
        for component in &report.skipped_components {
            tracing::info!(component, "demand daemon: background component skipped");
        }
        start_daemon_components_if_daemon(args, server, None);
        return report;
    }
    #[cfg(feature = "channel-email")]
    spawn_email_channel_loops_if_daemon(server, args);
    #[cfg(feature = "channel-telegram")]
    spawn_telegram_channel_loops_if_daemon(server, args);
    start_daemon_components_if_daemon(args, server, schedule_rt);
    report
}

#[cfg(all(test, unix))]
#[path = "serve/demand_startup_tests.rs"]
mod demand_startup_tests;

/// Whether this process owns the email channel loops (#602).
///
/// Channel loops (IMAP poll + outbox scan) are a daemon-role responsibility:
/// before this gate, `spawn_email_channel_loops` was called unconditionally
/// from EVERY serve entrypoint, so every stdio `kkernel mcp` client process
/// (one per Claude Code session, agent, etc.) spawned its own independent IMAP
/// poll loop against the same mailbox. Nine concurrent pollers exhausted
/// Exchange Online's per-mailbox connection slots and took inbound email down
/// for ~19h on 2026-07-04. `args.daemon` is the same flag `run`/`serve_server`
/// already use to decide whether to hand off to
/// `khive_runtime::daemon::run_daemon`, so gating on it keeps daemon-role
/// detection in one place shared by both boot paths, matching the
/// `checkpoint_pool_for` pattern (#601/#604).
#[cfg(feature = "channel-email")]
fn is_daemon_role(args: &Args) -> bool {
    args.daemon
}

/// Combine process role with the fixed runtime-mode admission captured when
/// the server was built. A daemon flag alone never authorizes background
/// writes: each loop is admitted only when the runtime serving its verbs is
/// writable in the resolved single- or multi-backend topology.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn channel_loop_plan(server: &KhiveMcpServer, args: &Args) -> crate::server::ChannelLoopAdmission {
    if args.daemon {
        server.channel_loop_admission()
    } else {
        crate::server::ChannelLoopAdmission::default()
    }
}

/// Name the reason inbound polling was refused when the comm runtime is
/// writable but the blob pack's runtime is not. Comm's own read-only refusal is
/// reported by the callers' skip line; this is the blob counterpart.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn log_inbound_refused_for_read_only_blob(
    channel_kind: &str,
    admission: crate::server::ChannelLoopAdmission,
) {
    if admission.inbound_blocked_by_read_only_blob {
        tracing::error!(
            channel = channel_kind,
            "{channel_kind} inbound polling not started: the blob pack runtime is read-only, so \
             quarantined originals cannot be stored; assign the blob pack a writable backend"
        );
    }
}

/// Handle for the ADR-091 Amendment 2 Plank A session sweep task. Dropping
/// the sender alone is NOT a sufficient shutdown contract (minor, ADR-091
/// Amendment 2): the sweep task's own clean-shutdown heartbeat
/// removal runs asynchronously after observing the channel close, and the
/// tokio runtime is not guaranteed to poll it to completion before the
/// process exits. [`Self::shutdown`] holds the `JoinHandle` and awaits it
/// (bounded) so the removal has actually run before `serve`/`run` returns.
struct SessionSweepHandle {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    join: tokio::task::JoinHandle<()>,
}

impl SessionSweepHandle {
    async fn shutdown(self) {
        drop(self.shutdown_tx);
        if tokio::time::timeout(std::time::Duration::from_secs(2), self.join)
            .await
            .is_err()
        {
            tracing::warn!(
                "ADR-091 Amendment 2 Plank A: session sweep task did not exit within 2s of \
                 the shutdown signal; its walpin heartbeat removal may not have completed"
            );
        }
    }
}

/// Spawn the ADR-091 Amendment 2 Plank A observe-only session sweep task,
/// fanned out over every file-backed backend this server carries: `pool` as
/// the main backend, plus one entry per pool in `secondary_pools` (ADR-091
/// Amendment 3). Returns `None` only when the server has no file-backed
/// backend at all (a purely in-memory or registry-only server). Returns a
/// [`SessionSweepHandle`] the caller MUST hold for the session's run scope
/// and shut down explicitly (see [`SessionSweepHandle::shutdown`]) — mirrors
/// `run_checkpoint_task`'s shutdown-channel contract on the daemon side.
///
/// Called from BOTH non-daemon serve entrypoints (`run` and `serve_server`,
/// item: sweep coverage, ADR-091 Amendment 2) — `serve_server` is the
/// ADR-029 multi-backend coordinator boot path, and previously never started
/// this sweep at all, leaving every multi-backend session permanently
/// invisible to cross-process WAL-pin attribution.
///
/// Platform-independent (ADR-091 Amendment 2: "Windows is a
/// supported target"): the tx_registry age check and the walpin sidecar
/// write path (`khive_db::walpin`) both run on every platform now — only
/// sidecar-directory *enumeration* (the daemon's TRUNCATE-time attribution
/// read) is Unix-only, and daemon mode itself already requires Unix. A
/// Windows session still registers its beacon and writes heartbeats, so it
/// classifies as `reporting`/`registered-silent` (not a permanent `unknown`)
/// whenever a Unix daemon does enumerate the shared sidecar directory.
fn spawn_session_walpin_sweep(server: &KhiveMcpServer) -> Option<SessionSweepHandle> {
    let mut backends = Vec::new();
    if let Some(pool) = server.pool() {
        backends.push(khive_db::SweepBackend {
            pool,
            is_main: true,
        });
    }
    for pool in server.secondary_pools() {
        backends.push(khive_db::SweepBackend {
            pool,
            is_main: false,
        });
    }
    if backends.is_empty() {
        return None;
    }
    let config = khive_db::SessionSweepConfig::from_env();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let join = tokio::spawn(khive_db::run_session_sweep_task(
        backends,
        config,
        shutdown_rx,
    ));
    tracing::info!("ADR-091 Amendment 2 Plank A: session WAL-registry sweep started");
    Some(SessionSweepHandle { shutdown_tx, join })
}

/// Serve `server` on the transport resolved from `args` with the ADR-091
/// Amendment 2 Plank A session sweep held for exactly the serve scope.
///
/// Both non-daemon serve entrypoints (`run` and `serve_server`) funnel
/// through here so the sweep lifecycle exists in one place. Every serve-path
/// early return — unknown transport, serve error, clean serve return —
/// happens inside the inner future, upstream of the unconditional shutdown
/// below, so a future early return cannot leak the sweep task or skip its
/// clean-shutdown heartbeat removal. Explicit shutdown (not just a dropped
/// sender) means the task's heartbeat removal has actually completed before
/// this function returns.
async fn serve_with_session_sweep(
    server: KhiveMcpServer,
    args: &Args,
    registry: &TransportRegistry,
) -> anyhow::Result<()> {
    let session_sweep = spawn_session_walpin_sweep(&server);
    serve_holding_sweep(session_sweep, server, args, registry).await
}

/// Inner half of [`serve_with_session_sweep`], split so tests can inject an
/// observable [`SessionSweepHandle`] and prove the shutdown is awaited on
/// every return path — the completion signal fires happens-before this
/// function returns, which the spawn-composed wrapper cannot demonstrate
/// deterministically (a dropped sender also wakes the task, just not before
/// the caller resumes).
async fn serve_holding_sweep(
    session_sweep: Option<SessionSweepHandle>,
    server: KhiveMcpServer,
    args: &Args,
    registry: &TransportRegistry,
) -> anyhow::Result<()> {
    let result = async {
        let transport_name = args.transport.as_deref().unwrap_or("stdio");
        let transport = registry.get(transport_name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown transport {transport_name:?}; registered: {}",
                registry.names().join(", ")
            )
        })?;
        let opts = ServeOptions {
            bind: args.bind.clone(),
        };
        transport.serve(server, &opts).await
    }
    .await;
    if let Some(sweep) = session_sweep {
        sweep.shutdown().await;
    }
    result
}

/// Admit each email channel loop only when this is the daemon process and the
/// runtime that backs that loop's verbs is writable. Shared by both serve
/// entrypoints (`run` and `serve_server`) so neither role nor storage-mode
/// gating can drift between single- and multi-backend boot paths.
///
/// If no daemon is running, mail is simply not polled until one starts — that
/// is the intended behavior, not a silent failure; the log line makes it
/// observable.
#[cfg(feature = "channel-email")]
fn spawn_email_channel_loops_if_daemon(server: &KhiveMcpServer, args: &Args) {
    let admission = channel_loop_plan(server, args);
    if !is_daemon_role(args) {
        tracing::info!("email channel loops: skipped (client role; daemon owns channel loops)");
        return;
    }
    log_inbound_refused_for_read_only_blob("email", admission);
    if !admission.inbound_poll && !admission.outbound_delivery {
        tracing::info!(
            "email channel loops: skipped (assigned comm runtime does not admit writes)"
        );
        return;
    }
    tracing::info!(
        inbound_poll = admission.inbound_poll,
        outbound_delivery = admission.outbound_delivery,
        "email channel loops: applying daemon/runtime admission"
    );
    spawn_email_channel_loops(server, admission);
}

/// Start ADR-119 daemon components in daemon role only. Non-daemon roles
/// must not start components and stay byte-identical in behavior and output
/// — the silent return keeps client runs unchanged. In daemon role the
/// registry itself always logs the enumerated roster (names + count),
/// including an empty one. A resolved `schedule_rt` contributes the dynamic
/// `schedule-tick` component; `None` leaves it out of the roster.
fn start_daemon_components_if_daemon(
    args: &Args,
    server: &KhiveMcpServer,
    schedule_rt: Option<KhiveRuntime>,
) -> usize {
    if !args.daemon {
        return 0;
    }
    // ADR-170: the main daemon supervises the events daemon — spawn it when
    // the events socket is unreachable, respawn if it dies. Both paths come
    // from the server's own resolved events-split config, so the supervised
    // daemon and the forwarding clients cannot anchor at diverging paths;
    // a socket is present exactly when this host upgraded to forwarding
    // (`enable_events_forwarding_for_daemon`), and the `KHIVE_EVENTS_SPLIT=0`
    // kill-switch already produced no split at resolution.
    // A read-only deployment never supervises an events daemon: the
    // supervised process opens the events sidecar writable, which would
    // create and schema-initialize it on this deployment's behalf. The
    // runtime side independently refuses to forward writes when its backend
    // is read-only, so the two guards fail safe together.
    #[cfg(unix)]
    if server.default_runtime_is_read_only() {
        tracing::info!("read-only deployment: events daemon supervision skipped");
    } else if let (Some(split), Some(wal_ceiling), Some((disk_guard, volume_lock_dir))) = (
        server.events_split_config(),
        server.events_wal_ceiling_policy(),
        server.events_disk_policy(),
    ) {
        if let Some(socket) = split.socket_path.clone() {
            khive_runtime::daemon::track_named_background_task(
                "events_daemon_supervision",
                khive_runtime::events_split::supervise_events_daemon_with_policies(
                    split.db_path.clone(),
                    socket,
                    wal_ceiling,
                    disk_guard,
                    volume_lock_dir,
                ),
            );
        }
    }
    crate::components::start_daemon_components_with_schedule(server, schedule_rt)
}

/// Admit the schedule runtime to daemon supervision only when its own assigned
/// backend is writable. This decision is deliberately per runtime: a read-only
/// main backend must not disable a schedule pack routed to a writable secondary,
/// while a writable main must not accidentally start a ticker for a schedule
/// pack routed to a read-only secondary.
fn writable_schedule_runtime(runtime: Option<KhiveRuntime>) -> Option<KhiveRuntime> {
    runtime.filter(|runtime| !runtime.is_read_only())
}

#[cfg(all(test, feature = "channel-email"))]
thread_local! {
    static EMAIL_POLL_TEST_CHANNEL: std::cell::RefCell<Option<std::sync::Arc<dyn khive_channel::Channel>>> =
        const { std::cell::RefCell::new(None) };
    static EMAIL_POLL_TEST_SHUTDOWN: std::cell::RefCell<Option<tokio_util::sync::CancellationToken>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "channel-email")]
fn email_poll_shutdown_token(
    process_shutdown: tokio_util::sync::CancellationToken,
) -> tokio_util::sync::CancellationToken {
    #[cfg(test)]
    let process_shutdown = EMAIL_POLL_TEST_SHUTDOWN
        .with(|token| token.borrow_mut().take())
        .unwrap_or(process_shutdown);
    process_shutdown
}

#[cfg(feature = "channel-email")]
/// Spawn the email channel polling + outbox loops if the `channel-email`
/// feature is enabled and `KHIVE_EMAIL_*` config resolves. Non-fatal: logs a
/// warning and returns on incomplete config. Only call this with the
/// role-and-runtime admission returned by [`channel_loop_plan`] — use
/// [`spawn_email_channel_loops_if_daemon`], which both serve entrypoints call.
fn spawn_email_channel_loops(
    server: &KhiveMcpServer,
    admission: crate::server::ChannelLoopAdmission,
) {
    use khive_channel::ChannelRegistry;
    use khive_channel_email::EmailChannel;
    use std::sync::Arc;

    match EmailChannel::from_env() {
        Ok(email_ch) => {
            let email_ch = Arc::new(email_ch);
            let mut ch_registry = ChannelRegistry::new();
            let dyn_ch: Arc<dyn khive_channel::Channel> = email_ch.clone();
            #[cfg(test)]
            let dyn_ch = EMAIL_POLL_TEST_CHANNEL
                .with(|channel| channel.borrow_mut().take())
                .unwrap_or(dyn_ch);
            ch_registry.register(dyn_ch);
            let ch_registry = Arc::new(ch_registry);
            let verb_reg = server.verb_registry_clone();
            let runtime = server.channel_outbox_runtime_clone();
            let ingest_ns = ingest_namespace_from_env();
            let default_actor = email_default_inbound_actor_from_env();
            let mailbox = email_ch.mailbox().to_string();

            let ingest_ns_clone = ingest_ns.clone();
            let default_actor_clone = default_actor.clone();
            let verb_reg_poll = verb_reg.clone();
            let ingest_ns_outbox = ingest_ns.clone();
            let mailbox_clone = mailbox.clone();
            let email_ch_clone = Arc::clone(&email_ch);
            let runtime_outbox = runtime.clone();

            let spawned = run_if_authorized(&ingest_ns, &verb_reg, || {
                if admission.inbound_poll {
                    let poll_shutdown =
                        email_poll_shutdown_token(khive_runtime::daemon_shutdown_token());
                    khive_runtime::track_named_background_task("email_channel_poll", async move {
                        if let Err(error) = ensure_channel_quarantine_storage(&verb_reg_poll).await
                        {
                            tracing::error!(
                                error = %error,
                                "email polling disabled: quarantine storage readiness check failed"
                            );
                            return;
                        }
                        channel_poll_loop(
                            ch_registry,
                            verb_reg_poll,
                            ingest_ns_clone,
                            default_actor_clone,
                            poll_shutdown,
                        )
                        .await;
                    });
                    tracing::info!("email channel polling loop started");
                }
                if admission.outbound_delivery {
                    match runtime_outbox {
                        Some(rt) => {
                            crate::components::start_channel_component(
                                "email-outbound",
                                server,
                                move |ctx| {
                                    Box::pin(channel_outbox_loop(
                                        email_ch_clone.clone(),
                                        rt.clone(),
                                        ingest_ns_outbox.clone(),
                                        mailbox_clone.clone(),
                                        ctx,
                                    ))
                                },
                            );
                            tracing::info!("email channel outbox loop started");
                        }
                        None => {
                            tracing::error!(
                                "email outbox loop was NOT started: server has no comm-routed \
                                 runtime handle, which the loop needs to scan, claim, and mark \
                                 outbound notes; outbound mail will not be sent"
                            );
                        }
                    }
                }
            });
            if !spawned {
                tracing::error!(
                    namespace = %ingest_ns,
                    "email channel loops NOT started: ingest namespace authorization failed (fail-closed)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                "channel-email feature is enabled but configuration is incomplete: {e}; \
                 email polling is disabled"
            );
        }
    }
}

/// Resolve the target namespace for ingested channel messages.
///
/// Reads `KHIVE_EMAIL_INGEST_NAMESPACE`; falls back to `"local"` when the
/// variable is unset or blank. Called once at server startup before the poll
/// loop is spawned.
#[cfg(feature = "channel-email")]
fn ingest_namespace_from_env() -> String {
    nonblank_env_or("KHIVE_EMAIL_INGEST_NAMESPACE", "local")
}

/// Resolve the default inbound actor for fresh (uncorrelated) email messages.
#[cfg(feature = "channel-email")]
fn email_default_inbound_actor_from_env() -> String {
    default_inbound_actor_from_env("KHIVE_EMAIL_DEFAULT_ACTOR", "channel:email")
}

/// Resolve the default inbound actor for fresh (uncorrelated) channel messages.
///
/// Reads the supplied environment variable; falls back to the supplied channel
/// actor when it is unset or blank. Both channel defaults are isolated
/// from the anonymous `local` mailbox.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn default_inbound_actor_from_env(actor_variable: &str, fallback: &str) -> String {
    nonblank_env_or(actor_variable, fallback)
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn nonblank_env_or(variable: &str, fallback: &str) -> String {
    std::env::var(variable)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Run `on_authorized` only when the ingest namespace passes the preflight check.
///
/// Returns `true` when the closure was called (preflight passed), `false`
/// otherwise.  Tests can inject a counting closure to verify the loop is not
/// started when preflight fails (ADR-056 §6 fail-closed contract).
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn run_if_authorized(
    ns_str: &str,
    registry: &khive_runtime::VerbRegistry,
    on_authorized: impl FnOnce(),
) -> bool {
    if preflight_ingest_namespace(ns_str, registry) {
        on_authorized();
        true
    } else {
        false
    }
}

/// Validate and authorize the ingest namespace before spawning the poll loop.
///
/// Returns `true` when `ns_str` parses to a valid namespace AND the registry
/// gate permits it.  Returns `false` on any parse failure or authorization
/// denial, after logging the reason.  The caller must not spawn the poll loop
/// when this returns `false` (fail-closed, ADR-056 §6).
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn preflight_ingest_namespace(ns_str: &str, registry: &khive_runtime::VerbRegistry) -> bool {
    match khive_runtime::Namespace::parse(ns_str) {
        Ok(ns) => match registry.authorize_namespace(ns) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    namespace = %ns_str,
                    error = %e,
                    "ingest namespace authorization denied; email polling will not start"
                );
                false
            }
        },
        Err(e) => {
            tracing::error!(
                namespace = %ns_str,
                error = %e,
                "invalid ingest namespace string; email polling will not start"
            );
            false
        }
    }
}

#[cfg(feature = "channel-email")]
const CHANNEL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
const UNKNOWN_INGEST_QUARANTINE_THRESHOLD: u8 = 5;

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelIngestDisposition {
    Hold { attempt: Option<u8> },
    Quarantine,
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn channel_ingest_attempt_key(channel_kind: &str, external_id: Option<&str>) -> Option<String> {
    external_id.map(|external_id| format!("{channel_kind}:{external_id}"))
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn channel_ingest_disposition(
    classification: khive_runtime::ChannelIngestFailureClass,
    channel_kind: &str,
    external_id: Option<&str>,
    unknown_attempts: &mut std::collections::HashMap<String, u8>,
) -> ChannelIngestDisposition {
    use khive_runtime::ChannelIngestFailureClass;

    match classification {
        ChannelIngestFailureClass::Retryable { .. } => {
            ChannelIngestDisposition::Hold { attempt: None }
        }
        ChannelIngestFailureClass::Permanent { .. } => ChannelIngestDisposition::Quarantine,
        ChannelIngestFailureClass::Unknown { .. } => {
            let Some(key) = channel_ingest_attempt_key(channel_kind, external_id) else {
                return ChannelIngestDisposition::Hold { attempt: None };
            };
            let attempt = unknown_attempts.entry(key).or_default();
            *attempt = attempt.saturating_add(1);
            if *attempt >= UNKNOWN_INGEST_QUARANTINE_THRESHOLD {
                ChannelIngestDisposition::Quarantine
            } else {
                ChannelIngestDisposition::Hold {
                    attempt: Some(*attempt),
                }
            }
        }
    }
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn log_channel_quarantine(
    channel_kind: &str,
    classification: khive_runtime::ChannelIngestFailureClass,
    external_id: &str,
) {
    tracing::warn!(
        channel = channel_kind,
        classification = classification.name(),
        reason = classification.reason(),
        external_id,
        "quarantined inbound message after comm.ingest refusal"
    );
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
async fn ensure_channel_quarantine_storage(
    registry: &khive_runtime::VerbRegistry,
) -> Result<(), khive_runtime::RuntimeError> {
    use serde_json::json;

    if !registry.has_verb("blob.put") || !registry.has_verb("blob.stat") {
        return Err(khive_runtime::RuntimeError::Unconfigured(
            "channel quarantine requires the blob pack".to_string(),
        ));
    }

    // `blob.stat` is read-only. Probing a valid, absent digest verifies both
    // verb registration and that the pack's BlobStore is installed without
    // publishing a startup artifact.
    registry
        .dispatch(
            "blob.stat",
            json!({"content_ref": "0000000000000000000000000000000000000000000000000000000000000000"}),
        )
        .await?;
    Ok(())
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[allow(clippy::too_many_arguments)]
async fn quarantine_channel_ingest_failure(
    registry: &khive_runtime::VerbRegistry,
    ingest_namespace: &str,
    channel_kind: &str,
    channel_slug: &str,
    default_inbound_actor: Option<&str>,
    envelope: &khive_channel::ChannelEnvelope,
    classification: khive_runtime::ChannelIngestFailureClass,
    retention_limit: Option<usize>,
) -> Result<(), khive_runtime::RuntimeError> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use serde_json::json;

    let external_id = envelope.external_id.as_deref().ok_or_else(|| {
        khive_runtime::RuntimeError::InvalidInput(
            "cannot quarantine a channel message without external_id".to_string(),
        )
    })?;
    let (replay_bytes, notification_to) = envelope
        .quarantine_replay
        .as_ref()
        .map(|replay| (replay.bytes.as_slice(), replay.notification_to.as_str()))
        .unwrap_or_else(|| (envelope.content.as_bytes(), envelope.to.as_str()));

    // The same retention bound as the poller's own quarantine path: past it
    // the message is still recorded, without its original bytes. An error
    // reading the count holds progress through the caller.
    let content_ref = if quarantine_original_may_be_retained(
        registry,
        ingest_namespace,
        retention_limit,
    )
    .await?
    {
        let put = registry
            .dispatch("blob.put", json!({"bytes": BASE64.encode(replay_bytes)}))
            .await?;
        Some(
            put.get("content_ref")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    khive_runtime::RuntimeError::Internal(
                        "blob.put returned no string content_ref for channel quarantine"
                            .to_string(),
                    )
                })?
                .to_string(),
        )
    } else {
        tracing::warn!(
            channel = channel_kind,
            external_id,
            limit = retention_limit,
            "quarantine retention limit reached; recording the refused message without its original bytes"
        );
        None
    };

    // Quarantine sender prefix invariant: the sender retains the originating
    // channel prefix because prefix-keyed consumers depend on it for alert
    // visibility. Moving `email:quarantine` outside `email:` hides the alert.
    let quarantine_sender = format!("{channel_kind}:quarantine");
    debug_assert!(
        quarantine_sender.starts_with(&format!("{channel_kind}:")),
        "quarantine sender prefix invariant: prefix-keyed consumers require the channel prefix"
    );

    let mut metadata = json!({
        "quarantined": "true",
        "quarantine_classification": classification.name(),
        "quarantine_reason": classification.reason(),
        "quarantine_external_id": external_id,
    });
    let content = match &content_ref {
        Some(content_ref) => {
            metadata["quarantine_content_ref"] = json!(content_ref);
            "Inbound channel message quarantined. Original bytes are temporarily available through the attached content reference."
        }
        None => {
            metadata["quarantine_original_retained"] = json!("false");
            metadata["quarantine_original_not_retained_reason"] = json!("retention-limit");
            "Inbound channel message quarantined. Its original bytes were not stored because the quarantine retention limit was reached."
        }
    };
    let mut params = json!({
        "namespace": ingest_namespace,
        "from": quarantine_sender,
        "to": notification_to,
        "content": content,
        "channel_kind": channel_kind,
        "channel_slug": channel_slug,
        "external_id": external_id,
        "correlation_external_id": envelope.correlation_external_id.clone(),
        "metadata": metadata,
    });
    if let Some(actor) = default_inbound_actor {
        params["default_inbound_actor"] = json!(actor);
    }
    registry.dispatch("comm.ingest", params).await?;
    log_channel_quarantine(channel_kind, classification, external_id);
    Ok(())
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[allow(clippy::too_many_arguments)]
async fn handle_channel_ingest_failure(
    registry: &khive_runtime::VerbRegistry,
    ingest_namespace: &str,
    channel: (&str, &str),
    default_inbound_actor: Option<&str>,
    envelope: &khive_channel::ChannelEnvelope,
    error: &khive_runtime::RuntimeError,
    unknown_attempts: &mut std::collections::HashMap<String, u8>,
    retention_limit: Option<usize>,
) -> bool {
    let (channel_kind, channel_slug) = channel;
    let classification = error.channel_ingest_failure_class();
    match channel_ingest_disposition(
        classification,
        channel_kind,
        envelope.external_id.as_deref(),
        unknown_attempts,
    ) {
        ChannelIngestDisposition::Hold { attempt } => {
            tracing::warn!(
                channel = channel_kind,
                classification = classification.name(),
                reason = classification.reason(),
                external_id = envelope.external_id.as_deref(),
                attempt,
                threshold = UNKNOWN_INGEST_QUARANTINE_THRESHOLD,
                "comm.ingest failed; holding channel progress"
            );
            false
        }
        ChannelIngestDisposition::Quarantine => {
            match quarantine_channel_ingest_failure(
                registry,
                ingest_namespace,
                channel_kind,
                channel_slug,
                default_inbound_actor,
                envelope,
                classification,
                retention_limit,
            )
            .await
            {
                Ok(()) => true,
                Err(quarantine_error) => {
                    tracing::warn!(
                        channel = channel_kind,
                        classification = classification.name(),
                        reason = classification.reason(),
                        external_id = envelope.external_id.as_deref(),
                        error = %quarantine_error,
                        "channel quarantine failed; holding progress for retry"
                    );
                    false
                }
            }
        }
    }
}

/// This maintenance verb is deliberately separate from `comm.heartbeat`:
/// heartbeat rows live in `CHANNEL_HEALTH_NAMESPACE`, while quarantine notes
/// live in the explicitly configured ingest namespace.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
async fn cleanup_expired_channel_quarantine(
    registry: &khive_runtime::VerbRegistry,
    ingest_namespace: &str,
    channel_kind: &str,
    channel_slug: &str,
) -> Result<(), khive_runtime::RuntimeError> {
    use serde_json::json;

    registry
        .dispatch(
            "comm.cleanup_expired_quarantine",
            json!({
                "namespace": ingest_namespace,
                "channel_kind": channel_kind,
                "channel_slug": channel_slug,
            }),
        )
        .await?;
    // Historical quarantines predate channel slugs. Drain one bounded page
    // per poll, with the same hold-on-error behavior as the exact-slug pass.
    registry
        .dispatch(
            "comm.cleanup_expired_quarantine",
            json!({
                "namespace": ingest_namespace,
                "channel_kind": channel_kind,
                "channel_slug": "",
                "mode": "legacy_slugless",
            }),
        )
        .await?;
    Ok(())
}

/// Wait `interval` between channel-loop cycles, unless the caller's shutdown
/// token fires first. Returns `false` when shutdown is observed, which is the
/// caller's signal to leave its loop.
///
/// Poll loops also select their transport read against this token. Store
/// dispatches and progress commits finish their existing sequence instead of
/// being dropped mid-flight. A cancelled read or cycle wait leaves the last
/// committed progress available for the next poll.
///
/// The token is a parameter, never read from
/// `khive_runtime::daemon_shutdown_token()` inside a loop. That singleton is
/// cancelled once per process, and this crate's test binary runs an in-process
/// daemon whose shutdown cancels it for every later test in the same process —
/// so a loop reading it directly stops before its first cycle in any test that
/// happens to run after one of those. Production passes the singleton at the
/// spawn site, which is where the process's daemon role is already known.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
async fn channel_cycle_wait(
    interval: std::time::Duration,
    shutdown: &tokio_util::sync::CancellationToken,
) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => false,
        _ = tokio::time::sleep(interval) => true,
    }
}

/// Whether one more quarantined original may be stored before it is published.
///
/// The bound is the number of live quarantine records in the ingest namespace
/// (`comm.health`'s `quarantined_count`), so it survives restarts and shrinks
/// as expired records are cleaned up. Records stored without an original count
/// too, which keeps the bound conservative. `None` applies no bound.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
async fn quarantine_original_may_be_retained(
    registry: &khive_runtime::VerbRegistry,
    ingest_namespace: &str,
    limit: Option<usize>,
) -> Result<bool, khive_runtime::RuntimeError> {
    let Some(limit) = limit else {
        return Ok(true);
    };
    if limit == 0 {
        return Ok(false);
    }
    let health = registry
        .dispatch(
            "comm.health",
            serde_json::json!({"namespace": ingest_namespace}),
        )
        .await?;
    let live = health
        .get("quarantined_count")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            khive_runtime::RuntimeError::Internal(
                "comm.health returned no numeric quarantined_count".to_string(),
            )
        })?;
    Ok(live < limit as u64)
}

/// Background task that polls all registered channels every 5 seconds and
/// ingests new inbound messages via `comm.ingest`.
///
/// #605: the 5s cadence is the happy-path default only. A connect/auth
/// failure (classified by `khive_channel_email::is_backoff_eligible`) starts
/// a per-channel-kind jittered exponential backoff (`ImapBackoff`,
/// 5s -> 10s -> ... capped at ~10min) instead of retrying flat every 5s; a
/// success resets that channel's backoff to base, and the loop returns to
/// the normal 5s cadence. This is process-side pressure relief on top of the
/// per-credential single-flight guard inside `LiveImap` itself. Eligible
/// failures log via [`log_eligible_poll_failure`]: `warn!` only on an
/// escalation edge, `debug!` while riding the same capped step — never one
/// `warn!` per retry.
///
/// Only compiled when the `channel-email` feature is enabled.
#[cfg(feature = "channel-email")]
async fn channel_poll_loop(
    channels: std::sync::Arc<khive_channel::ChannelRegistry>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
    default_inbound_actor: String,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use base64::Engine as _;
    use chrono::{DateTime, Utc};
    use khive_channel_email::{is_backoff_eligible, ImapBackoff};
    use serde_json::json;
    use std::collections::HashMap;
    // Per-channel bootstrap "since" floor (issue #449). This
    // only feeds the date-based SINCE search used while a channel has no
    // committed UID high-water yet (first-ever poll, or a UIDVALIDITY
    // reset); once a checkpoint has a high-water, polling is UID-ranged and
    // this floor is unused for that channel. Each entry only advances to the
    // poll tick's timestamp once that channel's full cycle -- cursor_get,
    // poll_page, every comm.ingest, and cursor_commit -- succeeds this tick.
    // Advancing it unconditionally (as a shared `last_poll` timestamp used
    // to) would drop the earlier floor on any bootstrap-cycle failure, and
    // if that failure spans a calendar-day boundary the next checkpoint-less
    // poll's SINCE clause would use the newer date, permanently skipping the
    // previous day's uncommitted mail.
    let mut bootstrap_since: HashMap<(String, String), DateTime<Utc>> = HashMap::new();
    // One backoff state per (kind, slug) — i.e. per credential (#606).
    // Keying by kind alone would throttle a
    // second same-kind credential (e.g. a second mailbox) whenever the first
    // one's connection fails, even though the two are independent
    // credentials with independent connectivity.
    let mut backoffs: HashMap<(String, String), ImapBackoff> = HashMap::new();
    // ADR-094: tracks the error class of the most recent unresolved failure
    // per (kind, slug), so `ChannelPollFailed` fires once per failure episode
    // (first failure since success, or a change in error class) rather than
    // once per retry. Cleared on every success.
    let mut last_error_class: HashMap<(String, String), &'static str> = HashMap::new();
    // Unknown ingest failures are bounded per external message ID. Entries
    // remain pinned through quarantine until the page cursor itself commits,
    // so a cursor-commit failure cannot restart the five-attempt wait.
    let mut unknown_ingest_attempts: HashMap<String, u8> = HashMap::new();
    let mut next_interval = CHANNEL_POLL_INTERVAL;
    let event_store = registry.event_store();
    // Captured before the loop's first sleep (issue #449 follow-up).
    // A channel's very first bootstrap floor must reflect
    // when the daemon actually started, not whenever its first tick happens
    // to fire: `tokio::time::sleep` below runs before any polling, so
    // computing `now` after it (as the loop used to) can land on the far
    // side of a calendar-day boundary the daemon started before. Every
    // vacant `bootstrap_since` entry -- on tick 1 or any later tick a
    // channel is first seen on -- uses this single startup timestamp
    // instead of that tick's own `now`.
    let startup_since = Utc::now();

    loop {
        if !channel_cycle_wait(next_interval, &shutdown).await {
            tracing::info!("email channel polling loop: daemon shutdown observed, stopping");
            return;
        }
        next_interval = CHANNEL_POLL_INTERVAL;

        let now = Utc::now();

        for (kind, slug, channel) in channels.iter() {
            let backoff_key = (kind.to_string(), slug.to_string());
            let since = *bootstrap_since
                .entry(backoff_key.clone())
                .or_insert(startup_since);
            // Set once this channel's cycle durably completes (a fresh
            // commit, or nothing new to commit); gates whether `since`
            // advances past this tick's `now` for next time.
            let mut bootstrap_floor_advances = false;

            append_channel_lifecycle_event(
                event_store.as_ref(),
                khive_types::EventKind::ChannelPollStarted,
                khive_storage::ChannelPollStartedPayload {
                    channel_kind: kind.to_string(),
                    channel_slug: slug.to_string(),
                    since_rfc3339: since.to_rfc3339(),
                },
            )
            .await;

            // One bounded expiry page per credential per tick, including
            // empty polls. A failure must hold this cycle before a success
            // heartbeat or cursor advance can be recorded.
            if let Err(error) =
                cleanup_expired_channel_quarantine(&registry, &ingest_namespace, kind, slug).await
            {
                tracing::warn!(channel = kind, slug, error = %error,
                    "quarantine retention cleanup failed; holding channel poll");
                record_channel_heartbeat(
                    &registry,
                    kind,
                    slug,
                    HeartbeatOutcome::Failure {
                        class: "retention",
                        message: error.to_string(),
                    },
                    event_store.as_ref(),
                )
                .await;
                continue;
            }

            // Durable checkpoint path (issue #449): cursor_get -> poll_page ->
            // every comm.ingest -> cursor_commit, committing only when the
            // whole page durably ingested. A cursor_get failure means we
            // cannot trust what progress to poll from, so this channel is
            // skipped for the cycle rather than risk polling from an empty
            // checkpoint and silently discarding durable state.
            let checkpoint = match load_channel_cursor(&registry, kind, slug).await {
                Ok(cp) => cp,
                Err(e) => {
                    tracing::warn!(
                        channel = kind,
                        "comm.cursor_get failed; skipping this channel's poll this cycle: {e}"
                    );
                    continue;
                }
            };

            #[cfg(test)]
            poll_timing_tests::at(poll_timing_tests::Boundary::EmailBeforePoll).await;
            // A transport poll may outlast the daemon drain budget. No
            // cursor or bootstrap floor advances until its page is ingested.
            let polled = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::info!("email channel polling loop: cancelled in-flight poll");
                    return;
                }
                result = channel.poll_page(since, checkpoint.as_ref()) => result,
            };
            match polled {
                Ok(page) => {
                    let prior_attempt =
                        backoffs.get(&backoff_key).map(|b| b.attempt()).unwrap_or(0);
                    if let Some(backoff) = backoffs.get_mut(&backoff_key) {
                        backoff.record_success();
                    }
                    last_error_class.remove(&backoff_key);

                    // Only a recovery from a prior failure/backoff episode is
                    // an interesting lifecycle transition — an unbroken
                    // string of healthy polls never had ChannelPollFailed
                    // fire, so there is nothing to report recovering from.
                    if prior_attempt > 0 {
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelPollSucceeded,
                            khive_storage::ChannelPollSucceededPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                envelope_count: page.envelopes.len(),
                                previous_backoff_attempt: prior_attempt,
                            },
                        )
                        .await;
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelBackoffReset,
                            khive_storage::ChannelBackoffResetPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                previous_backoff_attempt: prior_attempt,
                            },
                        )
                        .await;
                    }

                    record_channel_heartbeat(
                        &registry,
                        kind,
                        slug,
                        HeartbeatOutcome::Success,
                        event_store.as_ref(),
                    )
                    .await;

                    // Every envelope in the page must durably ingest before
                    // the cursor is allowed to advance past it (issue #449):
                    // a partial-page ingest failure must leave
                    // the checkpoint untouched so the next poll re-selects
                    // the whole page -- comm.ingest's `INSERT OR IGNORE`
                    // dedup then skips re-storing the messages that already
                    // succeeded, and only the failed one is retried.
                    let page_attempt_keys: Vec<String> = page
                        .envelopes
                        .iter()
                        .filter_map(|env| {
                            channel_ingest_attempt_key(kind, env.external_id.as_deref())
                        })
                        .collect();
                    let mut page_fully_ingested = true;
                    for env in page.envelopes {
                        let mut metadata = env.metadata.clone();
                        if kind == "email"
                            && metadata.get("quarantined").map(String::as_str) == Some("true")
                        {
                            let Some(replay) = env.quarantine_replay.as_ref() else {
                                tracing::warn!(
                                    channel = kind,
                                    external_id = env.external_id.as_deref(),
                                    "quarantined email has no original-byte replay; holding channel progress"
                                );
                                page_fully_ingested = false;
                                continue;
                            };
                            let retain_original = match quarantine_original_may_be_retained(
                                &registry,
                                &ingest_namespace,
                                channel.quarantine_retention_limit(),
                            )
                            .await
                            {
                                Ok(retain) => retain,
                                Err(error) => {
                                    tracing::warn!(
                                        channel = kind,
                                        external_id = env.external_id.as_deref(),
                                        %error,
                                        "could not read the retained quarantine count; holding channel progress"
                                    );
                                    page_fully_ingested = false;
                                    continue;
                                }
                            };
                            if !retain_original {
                                tracing::warn!(
                                    channel = kind,
                                    external_id = env.external_id.as_deref(),
                                    limit = channel.quarantine_retention_limit(),
                                    "quarantine retention limit reached; recording the message without its original bytes"
                                );
                                metadata.insert(
                                    "quarantine_original_retained".to_string(),
                                    "false".to_string(),
                                );
                                metadata.insert(
                                    "quarantine_original_not_retained_reason".to_string(),
                                    "retention-limit".to_string(),
                                );
                            } else {
                                let put = registry
                                    .dispatch(
                                        "blob.put",
                                        json!({
                                            "bytes": base64::engine::general_purpose::STANDARD
                                                .encode(&replay.bytes)
                                        }),
                                    )
                                    .await;
                                let content_ref = match put {
                                    Ok(result) => {
                                        match result.get("content_ref").and_then(|v| v.as_str()) {
                                            Some(content_ref) => content_ref.to_string(),
                                            None => {
                                                tracing::warn!(
                                            channel = kind,
                                            external_id = env.external_id.as_deref(),
                                            "blob.put returned no quarantine content reference; holding channel progress"
                                        );
                                                page_fully_ingested = false;
                                                continue;
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            channel = kind,
                                            external_id = env.external_id.as_deref(),
                                            %error,
                                            "failed to publish quarantined email original; holding channel progress"
                                        );
                                        page_fully_ingested = false;
                                        continue;
                                    }
                                };
                                metadata.insert("quarantine_content_ref".to_string(), content_ref);
                            }
                        }
                        let params = json!({
                            "namespace": ingest_namespace,
                            "from": env.from.clone(),
                            "to": env.to.clone(),
                            "content": env.content.clone(),
                            "subject": env.subject.clone(),
                            "channel_kind": kind,
                            "channel_slug": slug,
                            "external_id": env.external_id.clone(),
                            "legacy_external_id": env.legacy_external_id.clone(),
                            "sent_at": env.sent_at.as_ref().map(|ts| ts.to_rfc3339()),
                            "correlation_external_id": env.correlation_external_id.clone(),
                            "default_inbound_actor": default_inbound_actor,
                            "wire_message_id": env.wire_message_id.clone(),
                            "wire_references": env.wire_references.clone(),
                            "metadata": metadata,
                        });
                        if let Err(error) = registry.dispatch("comm.ingest", params).await {
                            let handled = handle_channel_ingest_failure(
                                &registry,
                                &ingest_namespace,
                                (kind, slug),
                                Some(&default_inbound_actor),
                                &env,
                                &error,
                                &mut unknown_ingest_attempts,
                                channel.quarantine_retention_limit(),
                            )
                            .await;
                            if !handled {
                                page_fully_ingested = false;
                            }
                        }
                    }

                    if page_fully_ingested {
                        match page.next_checkpoint {
                            Some(next_checkpoint) => {
                                #[cfg(test)]
                                poll_timing_tests::at(
                                    poll_timing_tests::Boundary::EmailBeforeCommit,
                                )
                                .await;
                                match commit_channel_cursor(&registry, kind, slug, &next_checkpoint)
                                    .await
                                {
                                    Ok(()) => bootstrap_floor_advances = true,
                                    Err(e) => {
                                        tracing::warn!(
                                            channel = kind,
                                            "comm.cursor_commit failed; progress not durably \
                                             advanced, next poll will retry: {e}"
                                        );
                                    }
                                }
                            }
                            // Nothing new to commit this tick is not a
                            // failure -- safe to advance the bootstrap floor.
                            None => bootstrap_floor_advances = true,
                        }
                        if bootstrap_floor_advances {
                            for key in page_attempt_keys {
                                unknown_ingest_attempts.remove(&key);
                            }
                        }
                    } else {
                        tracing::warn!(
                            channel = kind,
                            "not committing IMAP cursor: at least one message in this page \
                             failed comm.ingest; the whole page will be retried next poll"
                        );
                    }
                }
                Err(e) => {
                    let class = channel_error_class(&e);
                    record_channel_heartbeat(
                        &registry,
                        kind,
                        slug,
                        HeartbeatOutcome::Failure {
                            class,
                            message: e.to_string(),
                        },
                        event_store.as_ref(),
                    )
                    .await;

                    // First failure since success or since the error class
                    // changed — a run of identical retries at the same class
                    // does not re-fire this event.
                    if last_error_class.get(&backoff_key) != Some(&class) {
                        last_error_class.insert(backoff_key.clone(), class);
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelPollFailed,
                            khive_storage::ChannelPollFailedPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                error_class: class.to_string(),
                                error_message: e.to_string(),
                            },
                        )
                        .await;
                    }

                    if is_backoff_eligible(&e) {
                        let backoff = backoffs.entry(backoff_key).or_default();
                        let tick = backoff.record_failure();
                        log_eligible_poll_failure(kind, &e, &tick);
                        next_interval = next_interval.max(tick.delay);

                        if tick.should_warn {
                            append_channel_lifecycle_event(
                                event_store.as_ref(),
                                khive_types::EventKind::ChannelBackoffArmed,
                                khive_storage::ChannelBackoffArmedPayload {
                                    channel_kind: kind.to_string(),
                                    channel_slug: slug.to_string(),
                                    attempt: tick.attempt,
                                    step_ms: tick.step.as_millis() as u64,
                                    delay_ms: tick.delay.as_millis() as u64,
                                },
                            )
                            .await;
                        }
                    } else {
                        // Non-eligible failures (config/gate errors, never
                        // produced by poll/connect in practice) are not
                        // connectivity pressure, so they keep the pre-#605
                        // warn-every-retry behavior at the normal cadence.
                        tracing::warn!(channel = kind, "channel poll failed: {e}");
                    }
                }
            }

            if bootstrap_floor_advances {
                bootstrap_since.insert((kind.to_string(), slug.to_string()), now);
            }
        }
    }
}

/// Append one ADR-094 channel lifecycle event, namespaced and attributed the
/// same way as `record_channel_heartbeat`'s persisted rows.
///
/// Best-effort: `store == None` is a no-op, and a serialize/append failure is
/// logged and swallowed — no lifecycle-append error may ever interrupt or
/// slow down channel polling.
#[cfg(feature = "channel-email")]
async fn append_channel_lifecycle_event<P: serde::Serialize>(
    store: Option<&std::sync::Arc<dyn khive_storage::EventStore>>,
    kind: khive_types::EventKind,
    payload: P,
) {
    let Some(store) = store else {
        return;
    };
    let payload_value = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                event_kind = %kind.name(),
                "failed to serialize channel lifecycle event payload"
            );
            return;
        }
    };
    let event = khive_storage::Event::new(
        khive_pack_comm::CHANNEL_HEALTH_NAMESPACE,
        "channel.poll_lifecycle",
        kind,
        khive_types::SubstrateKind::Event,
        "daemon:channel_poll_loop",
    )
    .with_payload(payload_value);
    if let Err(err) = store.append_event(event).await {
        tracing::warn!(
            error = %err,
            event_kind = %kind.name(),
            "channel lifecycle event append failed"
        );
    }
}

/// One poll attempt's outcome, as reported to `comm.heartbeat` (#606).
#[cfg(feature = "channel-email")]
enum HeartbeatOutcome {
    Success,
    Failure {
        class: &'static str,
        message: String,
    },
}

/// Map a [`khive_channel::ChannelError`] to the `comm.heartbeat` `error_class`
/// open string enum (#606: `auth | transport | config`
/// in v1, callers must tolerate unknown classes). `Auth`/`Transport` are the
/// connectivity classes `is_backoff_eligible` already distinguishes;
/// `Config`/`UnauthorizedSender`/`InvalidEnvelope` are static/attribution
/// failures, never produced by `poll`/`connect` in practice (see
/// `is_backoff_eligible`'s doc comment), so they all map to `"config"`.
#[cfg(feature = "channel-email")]
fn channel_error_class(err: &khive_channel::ChannelError) -> &'static str {
    match err {
        khive_channel::ChannelError::Auth(_) | khive_channel::ChannelError::RetryableAuth(_) => {
            "auth"
        }
        khive_channel::ChannelError::Transport(_)
        | khive_channel::ChannelError::RateLimited { .. }
        | khive_channel::ChannelError::PermanentTransport(_) => "transport",
        khive_channel::ChannelError::Config(_)
        | khive_channel::ChannelError::UnauthorizedSender(_)
        | khive_channel::ChannelError::InvalidEnvelope(_) => "config",
    }
}

/// Persist one poll attempt's outcome via the `comm.heartbeat` subhandler
/// (#606). Best-effort: a failed write is logged, never interrupts the poll
/// loop. Takes NO `namespace` param — heartbeat rows are always dispatched
/// against `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` regardless of the
/// daemon's configured `KHIVE_EMAIL_INGEST_NAMESPACE` (2026-07-04); an
/// explicitly-scoped `comm.health` read may see a different namespace
/// (khive #877).
#[cfg(feature = "channel-email")]
async fn record_channel_heartbeat(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
    outcome: HeartbeatOutcome,
    event_store: Option<&std::sync::Arc<dyn khive_storage::EventStore>>,
) {
    use serde_json::json;

    let namespace = khive_pack_comm::CHANNEL_HEALTH_NAMESPACE;
    let params = match &outcome {
        HeartbeatOutcome::Success => json!({
            "namespace": namespace,
            "channel_kind": channel_kind,
            "channel_slug": channel_slug,
            "poll_interval_secs": CHANNEL_POLL_INTERVAL.as_secs(),
            "outcome": "success",
        }),
        HeartbeatOutcome::Failure { class, message } => json!({
            "namespace": namespace,
            "channel_kind": channel_kind,
            "channel_slug": channel_slug,
            "poll_interval_secs": CHANNEL_POLL_INTERVAL.as_secs(),
            "outcome": "failure",
            "error_class": class,
            "error_message": message,
        }),
    };
    if let Err(e) = registry.dispatch("comm.heartbeat", params).await {
        tracing::warn!(
            channel = channel_kind,
            "comm.heartbeat failed to persist poll outcome: {e}"
        );
        append_channel_lifecycle_event(
            event_store,
            khive_types::EventKind::ChannelHeartbeatPersistFailed,
            khive_storage::ChannelHeartbeatPersistFailedPayload {
                channel_kind: channel_kind.to_string(),
                channel_slug: channel_slug.to_string(),
                error: e.to_string(),
            },
        )
        .await;
    }
}

/// Load the durable poll checkpoint for `(channel_kind, channel_slug)` via
/// `comm.cursor_get` (issue #449). Returns `Ok(None)` on first-run
/// (`comm.cursor_get` returns JSON `null`). A dispatch failure or a
/// malformed response is returned as `Err` so the caller skips this
/// channel's poll for the cycle rather than risk polling with empty
/// progress and silently discarding durable state.
#[cfg(feature = "channel-email")]
async fn load_channel_cursor(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
) -> Result<Option<khive_channel::StoredChannelCheckpoint>, khive_runtime::RuntimeError> {
    use serde_json::json;

    let value = registry
        .dispatch(
            "comm.cursor_get",
            json!({
                "channel_kind": channel_kind,
                "channel_slug": channel_slug,
            }),
        )
        .await?;
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(|e| {
        khive_runtime::RuntimeError::Internal(format!(
            "comm.cursor_get returned a malformed checkpoint: {e}"
        ))
    })
}

/// Persist the durable poll checkpoint for `(channel_kind, channel_slug)`
/// via `comm.cursor_commit` (issue #449).
///
/// Callers MUST only call this after every envelope in the page has
/// returned `Ok` from `comm.ingest` -- see `channel_poll_loop`. Committing
/// on a partial page would advance the cursor past a message that was never
/// durably ingested, permanently skipping it.
#[cfg(feature = "channel-email")]
async fn commit_channel_cursor(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
    checkpoint: &khive_channel::ChannelCheckpoint,
) -> Result<(), khive_runtime::RuntimeError> {
    use serde_json::json;

    registry
        .dispatch(
            "comm.cursor_commit",
            json!({
                "channel_kind": channel_kind,
                "channel_slug": channel_slug,
                "source": checkpoint.source,
                "generation": checkpoint.generation,
                "high_water": checkpoint.high_water,
            }),
        )
        .await?;
    Ok(())
}

/// Log a backoff-eligible poll failure at the level ADR-091's `crossing_warn`
/// discipline calls for: `warn!` only on an escalation edge
/// (`tick.should_warn`, i.e. the computed step just changed), `debug!` on a
/// repeat at the same step. Regression fix (2026-07-04): the poll loop
/// previously emitted a generic `warn!` on every eligible retry in addition
/// to the escalation-edge warn, so sustained pressure spammed warn-level logs
/// once per retry instead of once per escalation. Extracted to a standalone
/// function so the level decision is unit-testable without driving the full
/// poll loop.
#[cfg(feature = "channel-email")]
fn log_eligible_poll_failure(
    kind: &str,
    err: &khive_channel::ChannelError,
    tick: &khive_channel_email::BackoffTick,
) {
    if tick.should_warn {
        tracing::warn!(
            channel = kind,
            attempt = tick.attempt,
            delay_secs = tick.delay.as_secs_f64(),
            "IMAP poll backoff escalating after connect/auth failure: {err}"
        );
    } else {
        tracing::debug!(
            channel = kind,
            attempt = tick.attempt,
            delay_secs = tick.delay.as_secs_f64(),
            "channel poll failed, holding at current backoff step: {err}"
        );
    }
}

/// True if a note's `delivered_at` property marks it as already delivered.
///
/// Must match the outbox-scan pending predicate
/// (`list_undelivered_outbound_messages`): a present-but-null `delivered_at`
/// is undelivered, not delivered (checking `.is_some()` alone would treat an
/// explicit null — e.g. left by a curation `update` — as delivered and strand
/// the note in the outbox forever), and a terminal `properties.delivery`
/// state (`"delivered"` / `"failed"`, ADR-122 §1) is not pending even when
/// `delivered_at` is absent.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn note_already_delivered(props: &serde_json::Map<String, serde_json::Value>) -> bool {
    let delivered_at_set = props
        .get("delivered_at")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let terminal_delivery = props
        .get("delivery")
        .and_then(|v| v.as_str())
        .is_some_and(|state| state == "delivered" || state == "failed");
    delivered_at_set || terminal_delivery
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
const OUTBOUND_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
const OUTBOUND_RETRY_CEILING: std::time::Duration = std::time::Duration::from_secs(30 * 60);

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
async fn record_outbound_send_failure(
    runtime: &khive_runtime::KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    note_id: uuid::Uuid,
    error: &khive_channel::ChannelError,
) -> khive_runtime::RuntimeResult<khive_storage::note::Note> {
    use khive_channel::DeliveryFailureClass;

    match error.delivery_failure_class() {
        DeliveryFailureClass::Transient => {
            let (base_delay, max_delay) = match error {
                khive_channel::ChannelError::RateLimited { retry_after, .. } => (
                    OUTBOUND_RETRY_BASE.max(*retry_after),
                    OUTBOUND_RETRY_CEILING.max(*retry_after),
                ),
                _ => (OUTBOUND_RETRY_BASE, OUTBOUND_RETRY_CEILING),
            };
            runtime
                .mark_outbound_message_transient_failure(
                    token,
                    note_id,
                    chrono::Utc::now(),
                    error.to_string(),
                    base_delay,
                    max_delay,
                )
                .await
        }
        DeliveryFailureClass::Permanent => {
            runtime
                .mark_outbound_message_failed(
                    token,
                    note_id,
                    chrono::Utc::now().to_rfc3339(),
                    error.to_string(),
                )
                .await
        }
    }
}

#[cfg(feature = "channel-email")]
fn outbound_claim_failure_is_permanent(error: &khive_runtime::RuntimeError) -> bool {
    fn invalid_storage_input(error: &khive_storage::StorageError) -> bool {
        match error {
            khive_storage::StorageError::InvalidInput { .. } => true,
            khive_storage::StorageError::WriterTaskRequestFailed { source, .. } => {
                invalid_storage_input(source)
            }
            _ => false,
        }
    }
    match error {
        khive_runtime::RuntimeError::InvalidInput(_) => true,
        khive_runtime::RuntimeError::Khive(error) => {
            error.kind() == khive_types::ErrorKind::InvalidInput
        }
        khive_runtime::RuntimeError::Storage(error) => invalid_storage_input(error),
        // Pressure, conflicts, and unclassified backend failures can recover;
        // they get the existing bounded backoff, never a per-note terminal mark.
        _ => false,
    }
}

#[cfg(feature = "channel-email")]
async fn record_outbound_claim_failure(
    runtime: &khive_runtime::KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    note_id: uuid::Uuid,
    error: &khive_runtime::RuntimeError,
) -> khive_runtime::RuntimeResult<khive_storage::note::Note> {
    if outbound_claim_failure_is_permanent(error) {
        runtime
            .mark_outbound_message_claim_failed(
                token,
                note_id,
                chrono::Utc::now().to_rfc3339(),
                error.to_string(),
            )
            .await
    } else {
        runtime
            .mark_outbound_message_claim_transient_failure(
                token,
                note_id,
                chrono::Utc::now(),
                error.to_string(),
                OUTBOUND_RETRY_BASE,
                OUTBOUND_RETRY_CEILING,
            )
            .await
    }
}

/// Background task that delivers undelivered outbound email notes every 5 seconds.
///
/// Implements AT-LEAST-ONCE delivery: the `external_id` (= RFC 822 Message-ID) is
/// persisted to the note BEFORE sending. A crash between the SMTP success and the
/// `delivered_at` write causes a duplicate send on restart; the duplicate carries
/// the same Message-ID so receiving MTAs typically collapse it.
///
/// Every storage touch (scan, `external_id` claim, `delivered_at` mark) goes
/// through `runtime`'s non-wire owner-side APIs rather than
/// `registry.dispatch(...)`: the generic wire verbs run on the kg pack's
/// runtime, which under a `[packs.comm]` backend assignment is a different
/// backend than the one holding comm's notes, and `external_id` is
/// additionally one of the owner-established properties the generic `update`
/// verb refuses to patch on a pack-owned note kind.
///
/// Only compiled when the `channel-email` feature is enabled.
#[cfg(feature = "channel-email")]
pub(crate) async fn channel_outbox_loop(
    email_channel: Arc<dyn khive_channel::Channel>,
    runtime: khive_runtime::KhiveRuntime,
    ingest_namespace: String,
    mailbox: String,
    ctx: crate::components::HostContext,
) -> Result<(), crate::components::ComponentError> {
    outbox::require_email_delivery_policy(&runtime)?;
    let historical = match std::env::var(khive_runtime::HISTORICAL_DOMAINS_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(crate::components::ComponentError::Permanent(format!(
                "{} must contain valid Unicode text",
                khive_runtime::HISTORICAL_DOMAINS_ENV
            )));
        }
    };
    let domains =
        khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(&mailbox, &historical)
            .map_err(crate::components::ComponentError::Permanent)?;
    outbox::validate_loop_channel(email_channel.as_ref(), "email")?;
    let slug = email_channel.slug();
    let mut channels = khive_channel::ChannelRegistry::new();
    channels.register(email_channel);
    let namespace = khive_runtime::Namespace::parse(&ingest_namespace)
        .map_err(|error| crate::components::ComponentError::Permanent(error.to_string()))?;
    let mut pause_until = None;
    loop {
        if !channel_cycle_wait(OUTBOUND_RETRY_BASE, ctx.cancellation()).await {
            return Ok(());
        }
        outbox::outbox_once(
            outbox::OutboxChannels::Registered {
                registry: &channels,
                slug: &slug,
            },
            outbox::OutboxPolicy::Email {
                mailbox: &mailbox,
                domains: &domains,
            },
            &runtime,
            &namespace,
            ctx.cancellation(),
            &mut pause_until,
        )
        .await?;
        ctx.heartbeat();
    }
}

/// Execute one email outbox scan. Kept separate from the five-second loop so
/// routing and owner-claim behavior can be verified without sleeping or
/// opening a network transport. Account-wide authentication errors propagate
/// to the supervisor without becoming per-message terminal failures.
#[cfg(all(test, feature = "channel-email"))]
#[allow(clippy::too_many_arguments)]
async fn channel_outbox_once(
    email_channel: &dyn khive_channel::Channel,
    runtime: &khive_runtime::KhiveRuntime,
    namespace: &khive_runtime::Namespace,
    mailbox: &str,
    domain: &str,
    allowlist: &[String],
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<(), crate::components::ComponentError> {
    let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(mailbox, "")
        .map_err(crate::components::ComponentError::Permanent)?;
    debug_assert_eq!(domains.current(), domain);
    let policy = OutboundEmailPolicy::configured(allowlist.to_vec())
        .map_err(crate::components::ComponentError::Permanent)?;
    let runtime = runtime.clone().with_outbound_email_policy(policy);
    let mut pause_until = None;
    outbox::outbox_once(
        outbox::OutboxChannels::Single(email_channel),
        outbox::OutboxPolicy::Email {
            mailbox,
            domains: &domains,
        },
        &runtime,
        namespace,
        cancellation,
        &mut pause_until,
    )
    .await
}

/// Serve a pre-built server (ADR-029 Phase 2 boot path).
///
/// Extracted from `run()` so that `kkernel`'s `Command::Mcp` arm can build a
/// coordinator-equipped server and then call this to drive the
/// daemon/transport dispatch. The `Args` object is still needed for `--daemon`,
/// `--transport`, and `--bind` flags.
///
/// `boot_guard` is the recovery lock the caller acquired *before* building
/// `server` (#667) — building a multi-backend coordinator server also runs
/// migrations and applies pack schema plans, so the same
/// acquire-before-construct/hold-through-bind pattern used in [`run`] applies
/// here. Pass `None` only if the caller could not acquire the lock.
///
/// `schedule_rt` is the caller's resolved `"schedule"`-pack runtime handle
/// (ADR-106) — see `start_daemon_components_if_daemon`. `kkernel`'s
/// coordinator-attached multi-backend boot path resolves this from the same
/// `MultiBackendRegistry.per_pack_runtimes` map it uses to build `server`
/// itself, so the tick drains the identical backend/actor/pack configuration
/// the live server serves.
pub async fn serve_server(
    server: KhiveMcpServer,
    args: &Args,
    registry: &TransportRegistry,
    boot_guard: Option<std::fs::File>,
    schedule_rt: Option<KhiveRuntime>,
) -> anyhow::Result<()> {
    if let Some(generation) = args.resumed_generation {
        tracing::warn!(
            generation,
            "bridge self-heal: this process is a resumed generation of an \
             in-place re-exec triggered by bridge self-heal"
        );
    }
    tracing::info!(target: "khive.boot", "{}", resolved_actor_disclosure(server.actor_id()));
    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon_with_options_and_boot_guard_and_start(
            server,
            boot_guard,
            args.daemon_options(),
            |server| start_host_background_tasks(args, server, schedule_rt),
        )
        .await?;
        return Ok(());
    }
    drop(boot_guard);
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!(
            "--daemon mode requires Unix (macOS/Linux). On Windows, use the stdio transport."
        );
    }

    // ADR-091 Amendment 2 Plank A: every non-daemon process runs the
    // observe-only session sweep — including this ADR-029 multi-backend
    // coordinator boot path (sweep coverage, ADR-091 Amendment 2).
    // Without this spawn, every multi-backend session is permanently
    // invisible to cross-process WAL-pin attribution.
    start_host_background_tasks(args, &server, schedule_rt);
    serve_with_session_sweep(server, args, registry).await
}

/// Build the VerbRegistry and per-pack runtimes for a multi-backend deployment
/// (ADR-028 + ADR-029 Phase 2).
///
/// Returns a [`MultiBackendRegistry`] that `kkernel` uses to both:
/// 1. Construct the `KhiveMcpServer` (via `from_registry_with_meta`), and
/// 2. Build the `BackendRegistry` for the `SubstrateCoordinator`.
///
/// This is an asynchronous host-boot boundary, not a pure registry constructor.
/// Before it returns, every distinct secondary backend has been inventoried and
/// the canonical main backend has completed the application-assisted V21
/// attachment cutover. No runtime or attachment-only GC surface is exposed while
/// that work is pending or incomplete.
///
/// This is a refactor-extraction of the registry-building logic from
/// `build_server_multi_backend`, keeping the existing tests intact.
///
/// `cli_db_override` is the raw, pre-resolution `--db` / `KHIVE_DB` value (issue
/// #553). `[[backends]]` in `khive.toml` otherwise wins unconditionally, so an
/// operator's `--db :memory:` isolation request was silently discarded whenever
/// any backend was declared. `Some(":memory:")` forces every declared backend to
/// in-memory for this invocation (loudly logged). A concrete path matching the
/// declared `main` backend is accepted as a no-op; any other concrete path is
/// rejected rather than silently collapsing distinct declared backends onto one
/// caller-supplied file.
pub async fn build_registry_for_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<MultiBackendRegistry> {
    khive_runtime::assert_db_anchor_consistent(base_config.db_path.as_deref(), cli_db_override)?;
    build_registry_for_multi_backend_inner(base_config, khive_cfg, cli_db_override).await
}

/// Build the coordinated multi-backend registry while checking a canonical
/// database anchor captured earlier in config discovery.
///
/// This has the same secondary-inventory and V21-completion guarantee as
/// [`build_registry_for_multi_backend`].
pub async fn build_registry_for_multi_backend_with_db_anchor(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> anyhow::Result<MultiBackendRegistry> {
    build_registry_for_multi_backend_with_db_anchor_and_max_readers(
        base_config,
        khive_cfg,
        cli_db_override,
        db_anchor,
        None,
    )
    .await
}

/// Build a registry with a reader count fixed before any configured store opens.
/// `None` retains normal sizing; callers selecting a forwarding default should
/// use [`mcp_max_readers`] so explicit counts and direct hosts retain precedence.
pub async fn build_registry_for_multi_backend_with_db_anchor_and_max_readers(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
    max_readers: Option<usize>,
) -> anyhow::Result<MultiBackendRegistry> {
    build_registry_for_multi_backend_with_db_anchor_and_max_readers_and_claims(
        base_config,
        khive_cfg,
        cli_db_override,
        db_anchor,
        max_readers,
        None,
    )
    .await
}

/// Daemon boot variant: verify each claimed pathname immediately after its
/// backend opens, before schema preparation can write to that backend.
/// Unix hosts supplying claims must first initialize
/// `khive_db::pool::initialize_claimed_file_observer` under its unsafe process
/// startup contract, before any SQLite I/O. Claimed construction fails closed
/// when the observer is uninitialized or its VFS/ABI is unsupported.
pub async fn build_registry_for_multi_backend_with_db_anchor_and_max_readers_and_claims(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
    max_readers: Option<usize>,
    daemon_claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<MultiBackendRegistry> {
    // Regression fence: `base_config.db_path` feeds `compute_config_id` below,
    // so it must agree with the canonical anchor for this same `--db` input.
    // This is the shared choke point both multi-backend boot paths funnel
    // through — `build_server_multi_backend` in this file and `kkernel`'s
    // `Command::Mcp` coordinator-attached branch — so the guard lives here
    // once instead of at each caller.
    khive_runtime::assert_captured_db_anchor_consistent(base_config.db_path.as_deref(), db_anchor)?;

    build_registry_for_multi_backend_inner_with_max_readers(
        base_config,
        khive_cfg,
        cli_db_override,
        max_readers,
        daemon_claims,
    )
    .await
}

/// One backend's schema result from [`migrate_configured_storage_topology`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSchemaMigrationStatus {
    /// Configured backend name (`main` for the implicit single backend).
    pub backend: String,
    /// Applied canonical core-schema version after coordination.
    pub applied_version: u32,
    /// Whether this backend advanced only as a prerequisite for the selected
    /// canonical-main target.
    pub prerequisite: bool,
}

#[derive(Debug, Clone)]
struct ConfiguredStorageTargetPlan {
    effective_backends: Vec<BackendConfig>,
    full_topology: bool,
}

fn effective_backend_configs(backends: &[BackendConfig], force_memory: bool) -> Vec<BackendConfig> {
    backends
        .iter()
        .map(|backend| {
            if force_memory {
                BackendConfig {
                    kind: BackendKind::Memory,
                    path: None,
                    wal_ceiling_bytes: None,
                    disk_reserve_bytes: None,
                    disk_guard_deadline_ms: None,
                    ..backend.clone()
                }
            } else {
                backend.clone()
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BackendAliasIdentity {
    /// An existing file can have several distinct canonical hard-link paths.
    #[cfg(any(unix, windows))]
    File(FileIdentity),
    /// A not-yet-created file has only a resolved path to compare.
    Path(PathBuf),
}

fn backend_alias_identity(
    backend_name: &str,
    canonical: &std::path::Path,
) -> anyhow::Result<BackendAliasIdentity> {
    #[cfg(any(unix, windows))]
    {
        match khive_db::file_identity::database_file_identity(canonical) {
            Ok(identity) => Ok(BackendAliasIdentity::File(identity)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(BackendAliasIdentity::Path(canonical.to_path_buf()))
            }
            Err(error) => anyhow::bail!(
                "backend {backend_name}: cannot inspect database identity at {}: {error}",
                canonical.display()
            ),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = backend_name;
        Ok(BackendAliasIdentity::Path(canonical.to_path_buf()))
    }
}

/// Read the identity that SQLite's pool pinned during open. A second pathname
/// stat alone can agree with the pre-open stat after an A→B→A replacement,
/// while SQLite actually holds B.
fn opened_backend_alias_identity(
    backend: &StorageBackend,
    canonical: &std::path::Path,
) -> anyhow::Result<BackendAliasIdentity> {
    #[cfg(any(unix, windows))]
    {
        let identity = backend
            .pool()
            .opened_file_identity_record()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "file-backed backend {} has no opened SQLite file identity",
                    canonical.display()
                )
            })?;
        Ok(BackendAliasIdentity::File(identity))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = backend;
        Ok(BackendAliasIdentity::Path(canonical.to_path_buf()))
    }
}

fn snapshot_matches_opened_backend(
    snapshot: &BackendAliasIdentity,
    opened: &BackendAliasIdentity,
) -> bool {
    #[cfg(any(unix, windows))]
    {
        // An absent first-open path has no file identity to compare yet.
        matches!(snapshot, BackendAliasIdentity::Path(_)) || snapshot == opened
    }
    #[cfg(not(any(unix, windows)))]
    {
        snapshot == opened
    }
}

/// Bind a configured file identity to the database actually opened by SQLite.
/// The opener is injectable only so the path-replacement window can be tested
/// deterministically; production passes `open_backend` unchanged.
fn open_backend_bound_to_alias_identity_with<F>(
    cfg: &BackendConfig,
    max_readers: Option<usize>,
    snapshot: &BackendAliasIdentity,
    canonical: &std::path::Path,
    opener: F,
) -> anyhow::Result<(StorageBackend, BackendAliasIdentity)>
where
    F: FnOnce(&BackendConfig, Option<usize>) -> anyhow::Result<StorageBackend>,
{
    let backend = opener(cfg, max_readers)?;
    let opened = opened_backend_alias_identity(&backend, canonical)?;
    let at_path = backend_alias_identity(&cfg.name, canonical)?;
    let configured_now = canonical_backend_path(cfg)?
        .map(|path| backend_alias_identity(&cfg.name, &path))
        .transpose()?;
    if opened != at_path
        || configured_now.as_ref() != Some(&opened)
        || !snapshot_matches_opened_backend(snapshot, &opened)
    {
        anyhow::bail!(
            "backend {}: database identity changed between topology snapshot and SQLite open \
             at {}: snapshot={snapshot:?}, opened={opened:?}, path_now={at_path:?}, \
             configured_now={configured_now:?}",
            cfg.name,
            canonical.display()
        );
    }
    Ok((backend, opened))
}

fn verify_reused_backend_alias_identity(
    cfg: &BackendConfig,
    snapshot: &BackendAliasIdentity,
    canonical: &std::path::Path,
    existing: &StorageBackend,
) -> anyhow::Result<()> {
    let opened = opened_backend_alias_identity(existing, canonical)?;
    let at_path = backend_alias_identity(&cfg.name, canonical)?;
    let configured_now = canonical_backend_path(cfg)?
        .map(|path| backend_alias_identity(&cfg.name, &path))
        .transpose()?;
    if at_path != opened
        || configured_now.as_ref() != Some(&opened)
        || !snapshot_matches_opened_backend(snapshot, &at_path)
    {
        anyhow::bail!(
            "backend {}: alias identity changed before cached backend reuse at {}: \
             snapshot={snapshot:?}, opened={opened:?}, path_now={at_path:?}, \
             configured_now={configured_now:?}",
            cfg.name,
            canonical.display()
        );
    }
    Ok(())
}

/// Open the entire declared topology against one pre-open snapshot. The
/// injectable opener makes the snapshot→SQLite-open race executable without a
/// scheduler or global hook; production captures both writer policies in
/// `open_backend_with_policies`.
fn open_effective_backends_with<F>(
    config: &RuntimeConfig,
    effective_backends: &[BackendConfig],
    max_readers: Option<usize>,
    mut opener: F,
) -> anyhow::Result<HashMap<String, Arc<StorageBackend>>>
where
    F: FnMut(
        &BackendConfig,
        Option<usize>,
        khive_db::WalCeilingPolicy,
    ) -> anyhow::Result<StorageBackend>,
{
    let mut backends: HashMap<String, Arc<StorageBackend>> = HashMap::new();
    let identities = effective_backends
        .iter()
        .map(|cfg| {
            canonical_backend_path(cfg)?.map_or(Ok(None), |canonical| {
                backend_alias_identity(&cfg.name, &canonical)
                    .map(|identity| Some((identity, canonical)))
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut identity_to_backend: HashMap<BackendAliasIdentity, (Arc<StorageBackend>, String, u64)> =
        HashMap::new();
    for (backend_cfg, identity) in effective_backends.iter().zip(identities) {
        let policy = wal_ceiling_policy_for_backend(config, backend_cfg)?;
        let effective_bytes = policy.effective_bytes(backend_cfg.read_only);
        if let Some((ref key, ref canon)) = identity {
            if let Some((existing, first_name, first_bytes)) = identity_to_backend.get(key) {
                verify_reused_backend_alias_identity(backend_cfg, key, canon, existing)?;
                if existing.is_read_only() != backend_cfg.read_only {
                    anyhow::bail!(
                        "backend {} aliases {} but declares read_only={} while the same \
                         physical database was already opened with read_only={}; every alias \
                         of one database must use the same access mode",
                        backend_cfg.name,
                        canon.display(),
                        backend_cfg.read_only,
                        existing.is_read_only(),
                    );
                }
                if *first_bytes != effective_bytes {
                    return Err(khive_runtime::ConfigError::WalCeilingAliasConflict {
                        first_backend: first_name.clone(),
                        second_backend: backend_cfg.name.clone(),
                        path: canon.clone(),
                        first_bytes: *first_bytes,
                        second_bytes: effective_bytes,
                    }
                    .into());
                }
                if disk_guard_numbers(existing.pool().effective_disk_guard_config())
                    != disk_guard_numbers(
                        backend_cfg.resolve_disk_guard(&config.disk_guard_environment)?,
                    )
                {
                    return Err(khive_runtime::ConfigError::DiskGuardAliasConflict {
                        first_backend: first_name.clone(),
                        second_backend: backend_cfg.name.clone(),
                    }
                    .into());
                }
                backends.insert(backend_cfg.name.clone(), existing.clone());
                continue;
            }
        }
        let (backend, opened_key) = if let Some((ref key, ref canon)) = identity {
            let (backend, opened) = open_backend_bound_to_alias_identity_with(
                backend_cfg,
                max_readers,
                key,
                canon,
                |cfg, max_readers| opener(cfg, max_readers, policy),
            )?;
            (backend, Some(opened))
        } else {
            (opener(backend_cfg, max_readers, policy)?, None)
        };
        let arc = Arc::new(backend);
        if let (Some((snapshot, _)), Some(opened)) = (identity, opened_key) {
            if identity_to_backend.contains_key(&opened) {
                anyhow::bail!(
                    "backend {}: opened database identity was already cached under another \
                     configured path; refusing duplicate pool after topology changed",
                    backend_cfg.name
                );
            }
            let cached = (arc.clone(), backend_cfg.name.clone(), effective_bytes);
            identity_to_backend.insert(opened, cached.clone());
            if matches!(&snapshot, BackendAliasIdentity::Path(_)) {
                identity_to_backend.insert(snapshot, cached);
            }
        }
        backends.insert(backend_cfg.name.clone(), arc);
    }
    Ok(backends)
}

/// Reject conflicting access modes without opening any configured database.
pub fn validate_effective_backend_alias_modes(backends: &[BackendConfig]) -> anyhow::Result<()> {
    let mut physical_sqlite: HashMap<BackendAliasIdentity, (&str, bool)> = HashMap::new();
    for backend in backends {
        let Some(canonical) = canonical_backend_path(backend)? else {
            continue;
        };
        let identity = backend_alias_identity(&backend.name, &canonical)?;

        if let Some((first_name, first_read_only)) = physical_sqlite.get(&identity) {
            if *first_read_only != backend.read_only {
                anyhow::bail!(
                    "backend {} aliases {} (already declared by backend {}) but declares \
                     read_only={} while the same physical database is declared read_only={}; \
                     every alias of one database must use the same access mode",
                    backend.name,
                    canonical.display(),
                    first_name,
                    backend.read_only,
                    first_read_only,
                );
            }
        } else {
            physical_sqlite.insert(identity, (&backend.name, backend.read_only));
        }
    }
    Ok(())
}

fn wal_ceiling_policy_for_backend(
    config: &RuntimeConfig,
    backend: &BackendConfig,
) -> anyhow::Result<khive_db::WalCeilingPolicy> {
    let wal_mode = backend
        .journal_mode
        .as_deref()
        .is_none_or(|mode| mode.eq_ignore_ascii_case("wal"));
    let resolved = khive_runtime::resolve_wal_ceiling(
        backend.wal_ceiling_bytes,
        config.wal_ceiling_env_raw.as_deref(),
        &backend.name,
        backend.kind.clone(),
        wal_mode,
        backend.read_only,
    )?;
    Ok(khive_db::WalCeilingPolicy {
        bytes: resolved.configured_bytes,
        source: resolved.source,
    })
}

/// Validate ceiling policy for every effective backend before daemon forwarding
/// or opening any configured database. Aliases of one SQLite file must agree
/// on the writer policy actually enforced by their shared pool.
pub fn validate_wal_ceiling_topology(
    config: &RuntimeConfig,
    backends: &[BackendConfig],
    force_memory: bool,
) -> anyhow::Result<()> {
    validate_disk_guard_topology(config, backends, force_memory)?;
    let effective = effective_backend_configs(backends, force_memory);
    let mut by_identity: HashMap<BackendAliasIdentity, (&str, u64)> = HashMap::new();
    for backend in &effective {
        let policy = wal_ceiling_policy_for_backend(config, backend)?;
        let effective_bytes = policy.effective_bytes(backend.read_only);
        let Some(path) = canonical_backend_path(backend)? else {
            continue;
        };
        let identity = backend_alias_identity(&backend.name, &path)?;
        if let Some((first_name, first_bytes)) = by_identity.get(&identity) {
            if *first_bytes != effective_bytes {
                return Err(khive_runtime::ConfigError::WalCeilingAliasConflict {
                    first_backend: (*first_name).to_string(),
                    second_backend: backend.name.clone(),
                    path,
                    first_bytes: *first_bytes,
                    second_bytes: effective_bytes,
                }
                .into());
            }
        } else {
            by_identity.insert(identity, (&backend.name, effective_bytes));
        }
    }
    Ok(())
}

fn plan_configured_storage_targets(
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    target_backend: Option<&str>,
) -> anyhow::Result<ConfiguredStorageTargetPlan> {
    reject_conflicting_db_override_with_source(cli_db_override, &khive_cfg.backends, None)?;
    let force_memory = cli_db_override == Some(":memory:");
    let effective_backends = effective_backend_configs(&khive_cfg.backends, force_memory);
    validate_effective_backend_alias_modes(&effective_backends)?;

    let main = effective_backends
        .iter()
        .find(|backend| backend.name == BackendId::MAIN)
        .ok_or_else(|| anyhow::anyhow!("configured topology has no \"main\" backend"))?;
    let full_topology = match target_backend {
        None => true,
        Some(target) => {
            let selected = effective_backends
                .iter()
                .find(|backend| backend.name == target)
                .ok_or_else(|| {
                    let defined = effective_backends
                        .iter()
                        .map(|backend| backend.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    anyhow::anyhow!("unknown backend {target:?}; defined backends: {defined}")
                })?;
            match (
                canonical_backend_path(selected)?,
                canonical_backend_path(main)?,
            ) {
                (Some(selected), Some(main)) => same_database_target(
                    &selected,
                    &main,
                    file_identity(&selected),
                    file_identity(&main),
                ),
                // A force-memory override intentionally creates one distinct
                // ephemeral backend per configured name; only the literal
                // main name is the canonical-main target in that mode.
                (None, None) => selected.name == main.name,
                _ => false,
            }
        }
    };

    Ok(ConfiguredStorageTargetPlan {
        effective_backends,
        full_topology,
    })
}

/// Whether two backends resolve to one database file.
///
/// Equal canonical paths name the same file by construction, so a file
/// identity read that disagrees (the file was replaced between the two reads)
/// never separates them. A matching identity is an additional way to be equal,
/// joining distinct paths such as hard links.
fn same_database_target(
    selected: &std::path::Path,
    main: &std::path::Path,
    selected_identity: Option<FileIdentity>,
    main_identity: Option<FileIdentity>,
) -> bool {
    selected == main
        || matches!(
            (selected_identity, main_identity),
            (Some(selected_id), Some(main_id)) if selected_id == main_id
        )
}

/// Return the configured backend names a read-only schema check must inspect.
///
/// This shares the migration command's physical-alias planning. Selecting
/// `main` (or a SQLite alias of it) therefore includes every secondary
/// prerequisite; selecting an independent secondary inspects only that target.
pub fn configured_storage_check_targets(
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    target_backend: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    if khive_cfg.backends.is_empty() {
        if let Some(target) = target_backend {
            if target != BackendId::MAIN {
                anyhow::bail!(
                    "unknown backend {target:?}; the implicit single-backend topology contains \
                     only \"main\""
                );
            }
        }
        return Ok(vec![BackendId::MAIN.to_string()]);
    }

    let plan = plan_configured_storage_targets(khive_cfg, cli_db_override, target_backend)?;
    if plan.full_topology {
        Ok(plan
            .effective_backends
            .into_iter()
            .map(|backend| backend.name)
            .collect())
    } else {
        Ok(vec![target_backend
            .expect("a partial topology plan always has a selected target")
            .to_string()])
    }
}

#[path = "serve_targeted_migration.rs"]
mod targeted_migration;

/// Migrate storage without constructing pack runtimes or loading embedders.
///
/// When `[[backends]]` is present this executes the same deduplicated,
/// secondary-first inventory and main V21 cutover barrier as normal MCP boot.
/// Independent targets open only that backend after whole-topology preflight.
/// Newly collapsed identities refuse before core-schema migration.
/// With no declared topology it uses the single-backend boot coordinator. No
/// runtime is exposed and no pack DDL is applied.
pub async fn migrate_configured_storage_topology(
    mut base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    target_backend: Option<&str>,
) -> anyhow::Result<Vec<BackendSchemaMigrationStatus>> {
    if khive_cfg.backends.is_empty() {
        if let Some(target) = target_backend {
            if target != BackendId::MAIN {
                anyhow::bail!(
                    "unknown backend {target:?}; the implicit single-backend topology contains \
                     only \"main\""
                );
            }
        }
        let backend = prepare_single_backend_for_schema_admin(&base_config, khive_cfg).await?;
        let applied_version = read_applied_schema_version(backend.sql().as_ref()).await?;
        return Ok(vec![BackendSchemaMigrationStatus {
            backend: BackendId::MAIN.to_string(),
            applied_version,
            prerequisite: false,
        }]);
    }

    let plan = plan_configured_storage_targets(khive_cfg, cli_db_override, target_backend)?;
    if !plan.full_topology {
        let target = target_backend.expect("a partial topology plan always has a target");
        let _force_memory = normalize_redundant_db_override(
            &mut base_config,
            cli_db_override,
            &khive_cfg.backends,
        )?;
        validate_wal_ceiling_topology(&base_config, &plan.effective_backends, false)?;
        let selected = plan
            .effective_backends
            .iter()
            .find(|backend| backend.name == target)
            .expect("the planner validated the selected backend")
            .clone();
        return Ok(vec![
            targeted_migration::migrate_selected_storage_backend_with(
                &base_config,
                &plan.effective_backends,
                &selected,
                |cfg, max_readers, wal| {
                    open_backend_with_policies(
                        cfg,
                        max_readers,
                        wal,
                        cfg.resolve_disk_guard(&base_config.disk_guard_environment)?,
                        base_config.volume_lock_dir.as_deref(),
                    )
                },
            )
            .await?,
        ]);
    }

    let prepared = prepare_configured_storage_topology(
        base_config,
        khive_cfg,
        cli_db_override,
        StorageTopologyPurpose::SchemaAdministration,
        None,
        None,
    )
    .await?;
    let selected_target = target_backend
        .and_then(|target| prepared.backends.get(target))
        .cloned();
    let mut statuses = Vec::with_capacity(khive_cfg.backends.len());
    for configured in &khive_cfg.backends {
        let backend = prepared.backends.get(&configured.name).ok_or_else(|| {
            anyhow::anyhow!(
                "configured backend {:?} disappeared during schema coordination",
                configured.name
            )
        })?;
        statuses.push(BackendSchemaMigrationStatus {
            backend: configured.name.clone(),
            applied_version: read_applied_schema_version(backend.sql().as_ref()).await?,
            prerequisite: selected_target
                .as_ref()
                .is_some_and(|selected| !Arc::ptr_eq(selected, backend)),
        });
    }
    Ok(statuses)
}

async fn read_applied_schema_version(sql: &dyn khive_storage::SqlAccess) -> anyhow::Result<u32> {
    use khive_storage::types::SqlValue;

    let mut reader = sql
        .reader()
        .await
        .map_err(|error| anyhow::anyhow!("open schema-version reader: {error}"))?;
    let value = reader
        .query_scalar(
            khive_db::migrations::schema_version_probe().labelled("read_applied_schema_version"),
        )
        .await
        .map_err(|error| anyhow::anyhow!("read applied schema version: {error}"))?;
    match value {
        Some(SqlValue::Integer(version)) if version >= 0 => Ok(version as u32),
        other => anyhow::bail!("schema-version query returned an invalid value: {other:?}"),
    }
}

/// Validate a `--db`/`KHIVE_DB` override against a non-empty `[[backends]]`
/// declaration WITHOUT opening any backend — the same rule
/// `build_registry_for_multi_backend_inner` enforces, factored out so a
/// caller that hasn't yet decided whether it will construct backends in this
/// process can apply the check up front.
///
/// This closes #1226: `kkernel exec`'s daemon-forward fast path (inline ops,
/// used whenever a warm daemon answers) never called into this guard at all
/// — only the in-process fallback did — so an inline invocation with a
/// conflicting override silently forwarded to the daemon's own already-open
/// backends instead of being rejected, while the same override on
/// `--ops-file` (always in-process by design) correctly bailed. The two call
/// forms disagreed about whether the override was legal because only one of
/// them ever ran this check. Returns `Ok(true)` when the override forces
/// every backend to in-memory (`:memory:`), `Ok(false)` when there is no
/// override to apply or the concrete override already names the declared
/// `main` backend.
pub fn validate_db_override_against_backends(
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
) -> anyhow::Result<bool> {
    validate_db_override_against_backends_with_source(cli_db_override, backends, None)
}

/// Source-preserving form of [`validate_db_override_against_backends`].
/// `config_source` is diagnostic only; it never participates in routing.
pub fn validate_db_override_against_backends_with_source(
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
    config_source: Option<&std::path::Path>,
) -> anyhow::Result<bool> {
    let backend_count = backends.len();
    reject_conflicting_db_override_with_source(cli_db_override, backends, config_source)?;
    match cli_db_override {
        Some(":memory:") => {
            tracing::warn!(
                "--db :memory: (or KHIVE_DB=:memory:) is forcing {backend_count} configured \
                 [[backends]] entries to ephemeral in-memory storage for this invocation; \
                 the backend paths declared in the discovered config (./khive.toml, \
                 <db-dir>/config.toml, or ~/.khive/config.toml) will not be used, and nothing \
                 written this run persists after the process exits"
            );
            Ok(true)
        }
        Some(other) => {
            if backends.is_empty() {
                tracing::info!(
                    "--db {other:?} (or KHIVE_DB) with no declared [[backends]]: the \
                     override names the database directly (ordinary single-backend case)"
                );
            } else {
                tracing::info!(
                    "--db {other:?} (or KHIVE_DB) matches the path declared for the \
                     \"main\" backend in khive.toml; proceeding because the override is a no-op"
                );
            }
            Ok(false)
        }
        None => Ok(false),
    }
}

/// Fail only for a conflicting concrete override, without logging accepted
/// `:memory:` or redundant-main cases. Boot paths use this before their shared
/// builder so a refusal retains its config source without duplicating the
/// builder's acceptance logs.
pub fn reject_conflicting_db_override_with_source(
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
    config_source: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // Self-guard (not a caller contract): with no declared backends there is
    // nothing to collapse, so a concrete override is the ordinary
    // single-backend case and must pass. Without this, the main-backend
    // lookup below finds nothing and EVERY concrete override would be
    // misclassified as ambiguous.
    if backends.is_empty() {
        return Ok(());
    }
    let Some(other) = cli_db_override.filter(|path| *path != ":memory:") else {
        return Ok(());
    };
    if override_matches_declared_main_backend(other, backends)? {
        return Ok(());
    }
    Err(DatabaseOverrideConflict::new(other, backends.len(), config_source).into())
}

/// Filesystem identity of a reindex target, captured so a symlink retargeted
/// or a file replaced in place between validation and open can be told apart
/// from the declared file validation actually checked.
#[cfg(any(unix, windows))]
type FileIdentity = khive_db::file_identity::DatabaseFileIdentity;

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity;

fn file_identity(path: &std::path::Path) -> Option<FileIdentity> {
    #[cfg(any(unix, windows))]
    {
        khive_db::file_identity::database_file_identity(path).ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

/// An identity-bound database target, shared by one-database admin commands.
/// The `kkernel reindex` database target as
/// [`validate_reindex_db_target_with_source`] resolved it: the canonical path
/// reindex must open, plus the filesystem identity observed at validation
/// time (`None` when the file did not exist yet).
///
/// [`reverify_reindex_target_identity`] re-derives this identity immediately
/// before open so a symlink retargeted, or the declared file replaced in
/// place, after validation is refused instead of silently followed.
#[derive(Debug, Clone)]
pub struct ValidatedReindexTarget {
    /// Canonical path reindex must open — never the raw `--db`/`KHIVE_DB`
    /// string, which may still name a symlink.
    pub path: PathBuf,
    identity: Option<FileIdentity>,
}

/// Capture an existing regular database file without opening SQLite or creating paths.
/// Callers must open the returned canonical path and reverify its identity immediately
/// before a writable open, using [`reverify_reindex_target_identity`].
pub fn capture_existing_database_target(
    path: &std::path::Path,
) -> anyhow::Result<ValidatedReindexTarget> {
    let path = canonical_path_no_side_effects(path)?;
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("database {} must already exist", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "database {} is not a regular file",
        path.display()
    );
    let identity = file_identity(&path);
    #[cfg(any(unix, windows))]
    anyhow::ensure!(
        identity.is_some(),
        "cannot capture database identity for {}",
        path.display()
    );
    Ok(ValidatedReindexTarget { path, identity })
}

/// Re-stat a validated reindex target and refuse if its filesystem identity
/// no longer matches what validation observed.
///
/// This is what actually binds the checked identity to the file reindex
/// opens: a symlink retargeted after validation still resolves `target.path`
/// to the same canonical (already symlink-free) path, so pinning the open to
/// `target.path` defeats that redirect by construction; a declared file
/// replaced in place (e.g. another database renamed over it) keeps the same
/// path string but changes its physical file identity, which this call catches.
///
/// This pre-open check alone cannot close a replacement window between its
/// return and the underlying SQLite `open()` syscall.
pub fn reverify_reindex_target_identity(target: &ValidatedReindexTarget) -> anyhow::Result<()> {
    let observed = file_identity(&target.path);
    if observed != target.identity {
        anyhow::bail!(
            "kkernel reindex target {} changed identity between validation and open \
             (validated {:?}, now {:?}); refusing to open a file that may no longer be \
             the declared backend",
            target.path.display(),
            target.identity,
            observed,
        );
    }
    Ok(())
}

/// Validate the single database selected by `kkernel reindex` against a
/// declared multi-backend topology.
///
/// Unlike MCP/exec's override guard, reindex is deliberately a one-database
/// maintenance command and may target any declared SQLite backend, not only
/// `main`. Once `[[backends]]` exists it must name that target explicitly:
/// falling through to the ordinary single-backend default can rebuild an
/// unrelated file while leaving the intended backend's indexes stale.
///
/// Returns `None` when no `[[backends]]` are declared — reindex keeps its
/// ordinary single-backend `--db` behavior and there is no declared identity
/// to bind. Returns `Some` with the canonical path and filesystem identity of
/// the matched backend otherwise; the caller must open exactly that path and
/// call [`reverify_reindex_target_identity`] immediately before doing so.
pub fn validate_reindex_db_target_with_source(
    db_target: Option<&str>,
    backends: &[BackendConfig],
    config_source: Option<&std::path::Path>,
) -> anyhow::Result<Option<ValidatedReindexTarget>> {
    if backends.is_empty() {
        return Ok(None);
    }

    let declared_targets = backends
        .iter()
        .filter_map(|backend| {
            (backend.kind == BackendKind::Sqlite)
                .then_some(backend.path.as_ref())
                .flatten()
                .map(|path| format!("{}={}", backend.name, path.display()))
        })
        .collect::<Vec<_>>();
    let declared_summary = if declared_targets.is_empty() {
        "<none>".to_string()
    } else {
        declared_targets.join(", ")
    };
    let source_suffix = config_source
        .map(|path| format!(" The selected config is {}.", path.display()))
        .unwrap_or_default();

    let Some(db_target) = db_target.filter(|target| *target != ":memory:") else {
        anyhow::bail!(
            "kkernel reindex requires an explicit persistent --db / KHIVE_DB target when \
             [[backends]] is declared; choose one declared SQLite backend path \
             ({declared_summary}).{source_suffix}"
        );
    };

    let target = canonical_path_no_side_effects(&khive_runtime::expand_tilde(
        std::path::Path::new(db_target),
    ))?;
    for backend in backends {
        if backend.kind != BackendKind::Sqlite {
            continue;
        }
        let Some(path) = backend.path.as_ref() else {
            continue;
        };
        if canonical_path_no_side_effects(&khive_runtime::expand_tilde(path))? == target {
            if backend.read_only {
                anyhow::bail!(
                    "kkernel reindex database target {db_target:?} matches declared backend \
                     {name:?}, which is read_only; reindex always writes, so a read-only \
                     backend cannot be reindexed.{source_suffix}",
                    name = backend.name,
                );
            }
            return Ok(Some(ValidatedReindexTarget {
                identity: file_identity(&target),
                path: target,
            }));
        }
    }

    anyhow::bail!(
        "kkernel reindex database target {db_target:?} is not a path declared in \
         [[backends]]; refusing to rebuild an unowned file. Declared SQLite backend \
         paths: {declared_summary}.{source_suffix}"
    )
}

/// Validate a database override and anchor declared topology at its main store.
///
/// Once a concrete path is proven to name the declared `main` backend, the
/// backend topology fully identifies storage and the override has no remaining
/// semantic effect. Both forms must use the declared store rather than a
/// HOME-derived fallback so clients and the daemon compute the same identity.
pub fn normalize_redundant_db_override(
    config: &mut RuntimeConfig,
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
) -> anyhow::Result<bool> {
    normalize_redundant_db_override_with_source(config, cli_db_override, backends, None)
}

/// Source-preserving form of [`normalize_redundant_db_override`].
pub fn normalize_redundant_db_override_with_source(
    config: &mut RuntimeConfig,
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
    config_source: Option<&std::path::Path>,
) -> anyhow::Result<bool> {
    let force_memory = validate_db_override_against_backends_with_source(
        cli_db_override,
        backends,
        config_source,
    )?;
    if force_memory {
        config.db_path = None;
        config.wal_ceiling_bytes = 0;
        config.wal_ceiling_configured_bytes = 0;
        config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
    } else {
        if let Some(main) = backends
            .iter()
            .find(|backend| backend.name == BackendId::MAIN)
        {
            config.db_path = match main.kind {
                BackendKind::Sqlite => main
                    .path
                    .as_ref()
                    .map(|path| khive_runtime::expand_tilde(path)),
                BackendKind::Memory => None,
            };
        }
    }
    Ok(force_memory)
}

fn override_matches_declared_main_backend(
    override_path: &str,
    backends: &[BackendConfig],
) -> anyhow::Result<bool> {
    let Some(main) = backends
        .iter()
        .find(|backend| backend.name == BackendId::MAIN)
    else {
        return Ok(false);
    };
    if main.kind != BackendKind::Sqlite {
        return Ok(false);
    }
    let Some(main_path) = main.path.as_ref() else {
        return Ok(false);
    };

    Ok(canonical_path_no_side_effects(main_path)?
        == canonical_path_no_side_effects(std::path::Path::new(override_path))?)
}

/// Refuse a declared writable SQLite backend whose current filesystem mode is
/// already read-only, without opening SQLite or creating any path.
///
/// The multi-backend boot path performs the same check after open so its
/// captured runtime identity remains authoritative. Pre-open clients must run
/// this narrower probe before daemon forwarding as well: a warm daemon may
/// still hold a write-capable handle acquired before a later chmod, and the
/// declaration-only topology fingerprint would otherwise route the request to
/// that retained writer. A force-memory override supersedes every declared
/// path and must skip this helper entirely at its call site.
pub fn validate_declared_backend_access_modes(backends: &[BackendConfig]) -> anyhow::Result<()> {
    for backend in backends {
        if backend.kind != BackendKind::Sqlite || backend.read_only {
            continue;
        }
        let Some(path) = backend.path.as_ref() else {
            continue;
        };
        let expanded = khive_runtime::expand_tilde(path);
        let metadata = match std::fs::metadata(&expanded) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "backend {}: cannot inspect filesystem access mode for {} before daemon \
                     forwarding: {error}",
                    backend.name,
                    expanded.display(),
                ));
            }
        };
        if metadata.permissions().readonly() {
            anyhow::bail!(
                "backend {}: path {} has no filesystem write bits; declare `read_only = true` \
                 so backend topology and daemon config identity describe the snapshot-inspection \
                 mode explicitly (request was refused before daemon forwarding)",
                backend.name,
                expanded.display(),
            );
        }
    }
    Ok(())
}

struct PreparedStorageTopology {
    base_config: RuntimeConfig,
    backends: HashMap<String, Arc<StorageBackend>>,
    main_backend: Arc<StorageBackend>,
    shared_hydrator: Option<Arc<BlobHydrator>>,
}

fn declared_backend_db_paths(config: &KhiveConfig) -> Arc<[PathBuf]> {
    config
        .backends
        .iter()
        .filter(|backend| backend.kind == BackendKind::Sqlite)
        .filter_map(|backend| backend.path.as_deref().map(khive_runtime::expand_tilde))
        .collect::<Vec<_>>()
        .into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StorageTopologyPurpose {
    Serving,
    SchemaAdministration,
}

async fn schema_admin_requires_blob_hydrator(backend: Arc<StorageBackend>) -> anyhow::Result<bool> {
    let status_backend = Arc::clone(&backend);
    let status = tokio::task::spawn_blocking(move || status_backend.attachment_cutover_status())
        .await
        .context("schema-admin attachment status task panicked")?
        .context("inspect schema-admin attachment status")?;
    if status == khive_db::migrations::AttachmentCutoverStatus::Complete {
        return Ok(false);
    }

    Ok(
        crate::attachment_cutover::legacy_preference_model_count(backend.sql().as_ref())
            .await
            .context("count legacy moodboard models before resolving blob storage")?
            != 0,
    )
}

async fn prepare_configured_storage_topology(
    mut base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    purpose: StorageTopologyPurpose,
    max_readers: Option<usize>,
    daemon_claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<PreparedStorageTopology> {
    let force_memory =
        normalize_redundant_db_override(&mut base_config, cli_db_override, &khive_cfg.backends)?;
    let effective_backends = effective_backend_configs(&khive_cfg.backends, force_memory);
    // Validate every physical alias before opening the first database. A
    // targeted admin command and full server boot must reject the same mixed
    // read-only/read-write alias topology without creating a partial set of
    // database files.
    validate_effective_backend_alias_modes(&effective_backends)?;
    validate_wal_ceiling_topology(&base_config, &effective_backends, false)?;

    // ADR-170: the events split must anchor beside the store that actually
    // holds this deployment's data. The resolver derived it from
    // `base_config.db_path`, which for declared-backend configs is the
    // materialized `$HOME/.khive/khive.db` default rather than the declared
    // main backend — re-anchor beside main's declared file here, preserving
    // the resolver/daemon-host mode decision (direct vs forwarding) by
    // re-deriving the socket beside the moved events db. An in-memory main
    // (including a force-memory override) carries no event-plane split.
    if let Some(split) = base_config.events_split.take() {
        let main_path = effective_backends
            .iter()
            .find(|b| b.name == BackendId::MAIN && b.kind == BackendKind::Sqlite)
            .and_then(|b| b.path.as_ref());
        if let Some(main_path) = main_path {
            let expanded = khive_runtime::expand_tilde(main_path);
            let db_path = khive_runtime::events_split::events_db_path_beside(&expanded);
            let socket_path = split
                .socket_path
                .is_some()
                .then(|| khive_runtime::events_split::events_socket_path_beside(&db_path));
            base_config.events_split = Some(khive_runtime::events_split::EventsSplitConfig {
                db_path,
                socket_path,
            });
        }
    }

    // Open each declared backend, deduplicating SQLite backends by physical
    // file identity (or canonical path before creation; ADR-028 §8). Schema
    // preparation is deferred until after main is identified: every distinct
    // secondary must be inventoried before main can atomically enable
    // attachment-only GC at V21.
    #[cfg(unix)]
    preflight_events_socket_for_boot(&base_config, &effective_backends, force_memory)?;

    let backends = open_effective_backends_with(
        &base_config,
        &effective_backends,
        max_readers,
        |cfg, readers, policy| {
            claimed_backend::open_backend(
                cfg,
                readers,
                policy,
                daemon_claims,
                cfg.resolve_disk_guard(&base_config.disk_guard_environment)?,
                base_config.volume_lock_dir.as_deref(),
            )
        },
    )?;

    if let Some(claims) = daemon_claims {
        khive_runtime::daemon::assert_daemon_store_identities(claims)?;
    }

    let main_backend = backends
        .get(BackendId::MAIN)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[[backends]] is declared but no backend named \"main\" was found; \
             add a [[backends]] entry with name = \"main\""
            )
        })?
        .clone();

    // ADR-160 D4 chooses one liveness authority: attachment-bearing records
    // live only on main. Preflight every distinct secondary before main can
    // expose a durable incomplete marker, then finish any empty interrupted
    // V21 stage there so every runtime starts on one exact schema.
    let mut checked_secondary = HashSet::new();
    let mut unique_secondary = Vec::new();
    for configured in &khive_cfg.backends {
        let backend_name = configured.name.as_str();
        let backend = backends.get(backend_name).ok_or_else(|| {
            anyhow::anyhow!(
                "configured backend {backend_name:?} disappeared during topology preparation"
            )
        })?;
        if Arc::ptr_eq(backend, &main_backend) {
            continue;
        }
        let identity = Arc::as_ptr(backend) as usize;
        if checked_secondary.insert(identity) {
            prepare_core_schema_for_boot(Arc::clone(backend), format!("backend {backend_name}"))
                .await?;
            crate::attachment_cutover::require_secondary_attachment_empty(
                Arc::clone(backend),
                backend_name,
            )
            .await?;
            unique_secondary.push((backend_name.to_string(), Arc::clone(backend)));
        }
    }
    for (backend_name, backend) in unique_secondary {
        crate::attachment_cutover::coordinate_empty_secondary_attachment_cutover(
            backend,
            &backend_name,
        )
        .await?;
    }

    // Only now may main advance. In particular, a zero-ref V20 main must not
    // take the ordinary atomic V21 fast path until every secondary has proved
    // it anchors no process-shared blob.
    prepare_core_schema_for_boot(Arc::clone(&main_backend), "backend main").await?;

    // Serving always resolves the process-wide BlobStore/hydration pair.
    // Core-only schema administration does so only when an unfinished V21
    // cutover has legacy moodboard evidence to authenticate. Exact-current
    // and non-moodboard migrations must remain independent of unused blob
    // configuration (and side-effect free for read-only snapshots).
    let resolve_hydrator = match purpose {
        StorageTopologyPurpose::Serving => true,
        StorageTopologyPurpose::SchemaAdministration => {
            schema_admin_requires_blob_hydrator(Arc::clone(&main_backend)).await?
        }
    };
    let shared_hydrator = if resolve_hydrator {
        // Resolve before staging main: an explicit invalid [storage.blob]
        // selection required for verified migration must fail without leaving
        // a new incomplete marker. The blob pack's backend mode governs
        // read-only wrapping during serving boot.
        let blob_governing_backend = if base_config.packs.iter().any(|pack| pack == "blob") {
            match khive_cfg.packs.get("blob") {
                Some(pack) => backends
                    .get(pack.backend.as_str())
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "[packs.blob].backend = {:?} references an unknown backend",
                            pack.backend
                        )
                    })?,
                None => main_backend.clone(),
            }
        } else {
            main_backend.clone()
        };
        resolve_blob_hydrator_for_boot(
            &base_config,
            khive_cfg,
            main_backend.as_ref(),
            blob_governing_backend.as_ref(),
        )?
    } else {
        None
    };
    crate::attachment_cutover::coordinate_attachment_cutover(
        Arc::clone(&main_backend),
        shared_hydrator.clone(),
        crate::attachment_cutover::legacy_preference_verifier(),
    )
    .await?;

    Ok(PreparedStorageTopology {
        base_config,
        backends,
        main_backend,
        shared_hydrator,
    })
}

/// Resolve one pack's declared backend using the serving route: an explicit
/// `[packs.<name>]` assignment wins, otherwise the pack uses `main`.
/// This inspects configuration only and never opens a backend.
pub fn resolve_pack_backend_config<'a>(
    khive_cfg: &'a KhiveConfig,
    pack_name: &str,
) -> anyhow::Result<(&'a BackendConfig, bool)> {
    let (backend_name, no_embed) = match khive_cfg.packs.get(pack_name) {
        Some(pack) => (pack.backend.as_str(), pack.no_embed),
        None => (BackendId::MAIN, false),
    };
    let mut matching = khive_cfg
        .backends
        .iter()
        .filter(|backend| backend.name == backend_name);
    let backend = matching.next().ok_or_else(|| {
        let defined = khive_cfg.backends.iter().map(|backend| backend.name.as_str()).collect::<Vec<_>>().join(", ");
        anyhow::anyhow!(
            "absent backend route: [packs.{pack_name}].backend = {backend_name:?} references an unknown backend; defined backends: {defined}"
        )
    })?;
    anyhow::ensure!(
        matching.next().is_none(),
        "ambiguous backend route: [packs.{pack_name}].backend = {backend_name:?} has duplicate backend names"
    );
    Ok((backend, no_embed))
}

async fn build_registry_for_multi_backend_inner(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<MultiBackendRegistry> {
    build_registry_for_multi_backend_inner_with_max_readers(
        base_config,
        khive_cfg,
        cli_db_override,
        None,
        None,
    )
    .await
}

async fn build_registry_for_multi_backend_inner_with_max_readers(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    max_readers: Option<usize>,
    daemon_claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<MultiBackendRegistry> {
    let email_policy = OutboundEmailPolicy::from_env().map_err(anyhow::Error::msg)?;
    let PreparedStorageTopology {
        base_config,
        backends,
        main_backend,
        shared_hydrator,
    } = prepare_configured_storage_topology(
        base_config,
        khive_cfg,
        cli_db_override,
        StorageTopologyPurpose::Serving,
        max_readers,
        daemon_claims,
    )
    .await?;

    // Every pack sees the whole serving topology, including stores assigned
    // to other packs, before handlers can accept a code.ingest target.
    let declared_backend_db_paths = declared_backend_db_paths(khive_cfg);
    let diagnostic_backends = opened_diagnostic_backends(&backends);

    // Built before the pack loop: secondary-pack runtimes capture the main
    // runtime's embedder wiring so their `core()`-routed writes embed with
    // main's models even when the pack itself is `no_embed`.
    let default_runtime = KhiveRuntime::from_backend(main_backend.clone(), {
        let mut cfg = base_config.clone();
        cfg.backend_id = BackendId::main();
        cfg
    })
    .with_outbound_email_policy(email_policy)
    .with_declared_backend_db_paths(declared_backend_db_paths.clone())
    .with_diagnostic_backends(diagnostic_backends.clone());

    let pack_names = &base_config.packs;
    let mut per_pack_runtimes_local: HashMap<String, KhiveRuntime> = HashMap::new();
    for pack_name in pack_names {
        let (backend_config, no_embed) = resolve_pack_backend_config(khive_cfg, pack_name)?;
        let backend_name = backend_config.name.as_str();
        let backend = backends.get(backend_name).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "resolved backend {backend_name:?} for pack {pack_name:?} was not prepared"
            )
        })?;
        let mut rt_config = base_config.clone();
        rt_config.backend_id = BackendId::parse(backend_name)?;
        if no_embed {
            // `[packs.<name>] no_embed = true`: this pack's runtime gets zero
            // embedders — pack-owned writes are FTS + metadata only. Clear
            // both model fields together, same contract as
            // `RuntimeConfig::no_embeddings`. `core()`-routed concept writes
            // are NOT affected: `build_pack_runtime` hands every secondary
            // pack the main runtime's embedder wiring for core().
            rt_config.embedding_model = None;
            rt_config.additional_embedding_models = Vec::new();
        }
        per_pack_runtimes_local.insert(
            pack_name.clone(),
            build_pack_runtime(
                backend,
                backend_name,
                rt_config,
                &main_backend,
                &default_runtime,
                declared_backend_db_paths.clone(),
                diagnostic_backends.clone(),
            ),
        );
    }

    // ADR-160 D3: resolve the config-selected `BlobStore` once, pair it with
    // exactly one aggregate hydration budget, and install the same immutable
    // hydrator `Arc` on every runtime handle this boot produces. `core()`
    // clones share each runtime's one-shot slot, so they see this same pair.
    if let Some(hydrator) = shared_hydrator {
        // The shared install: the hydrator's mode was decided above from the
        // blob pack's backend, and the receiving handles legitimately mix
        // modes (a writable blob secondary beside a read-only main is a
        // documented topology), so each handle's own domain-store mode must
        // not gate this install.
        default_runtime.install_shared_blob_hydrator(Arc::clone(&hydrator))?;
        for rt in per_pack_runtimes_local.values() {
            rt.install_shared_blob_hydrator(Arc::clone(&hydrator))?;
        }
    }

    #[cfg(feature = "bench-embedder")]
    {
        for rt in per_pack_runtimes_local.values() {
            for name in rt.registered_embedding_model_names() {
                rt.register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
            }
        }
        for name in default_runtime.registered_embedding_model_names() {
            default_runtime
                .register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
        }
    }

    enforce_strict_actor_mode(
        default_runtime.config().actor_id.as_deref(),
        &default_runtime.config().packs,
    )?;
    if should_warn_unattributed(
        default_runtime.config().actor_id.as_deref(),
        &default_runtime.config().packs,
    ) {
        tracing::warn!(
            "actor identity resolved to \"local\": comm sends will be stamped from \
             \"local\" (unattributed) and comm.inbox will be unscoped (party-line). \
             Set KHIVE_ACTOR or --actor to this lambda's id."
        );
    }

    let gate = default_runtime.config().gate.clone();
    let default_namespace = default_runtime.config().default_namespace.clone();
    let config_id = crate::server::compute_config_id_with_runtime_policies(
        default_runtime.config(),
        Some(khive_cfg),
        default_runtime.ann_fresh_tail_enabled(),
        default_runtime.is_read_only(),
    );
    let visible_namespaces = default_runtime.config().visible_namespaces.clone();

    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_gate(gate);
    builder.with_default_namespace(default_namespace.as_str());
    builder.with_visible_namespaces(visible_namespaces);
    builder.with_actor_id(default_runtime.config().actor_id.clone());

    if default_runtime.is_read_only() {
        builder.with_read_only_audit_store();
    } else {
        // Opening the configured sink is deferred to build and must succeed
        // before serving; an unavailable sink no longer degrades to tracing.
        builder.with_runtime_event_store(&default_runtime)?;
    }

    khive_runtime::PackRegistry::register_packs_with_runtimes(
        pack_names,
        &per_pack_runtimes_local,
        &default_runtime,
        &mut builder,
    )
    .map_err(|e| anyhow::anyhow!("pack registration: {e}"))?;

    khive_mounts::register_mounts(&default_runtime, &mut builder).await?;

    let registry = builder
        .build()
        .map_err(|e| anyhow::anyhow!("registry build: {e}"))?;

    default_runtime.install_edge_rules(registry.all_edge_rules());
    for rt in per_pack_runtimes_local.values() {
        rt.install_edge_rules(registry.all_edge_rules());
    }
    registry.call_register_embedders(&default_runtime);
    registry.call_register_entity_type_validators(&default_runtime);
    // #2943: install entity-kind update hooks (same scope/timing as the
    // entity-type validator above — entities live on the shared/main graph,
    // reached through `core()`, never on a per-pack secondary backend).
    default_runtime.install_entity_kind_hooks(registry.entity_kind_hooks());
    // #750: install pack-owned note-mutation hooks (currently
    // only khive-pack-memory's warm-ANN-cache invalidation) so KG's
    // update/delete verbs notify caching packs even though there is no
    // crate-level dependency between them.
    registry.call_register_note_mutation_hooks(&default_runtime);
    registry.call_register_note_search_ann_providers(&default_runtime);
    for rt in per_pack_runtimes_local.values() {
        registry.call_register_note_search_ann_providers(rt);
    }
    // Note-write identity: install the pack-owned kind set and the pack-owned
    // note-write validator so identity properties are derived at the write and
    // preserved through merge/update on every path, including the ones that
    // reach no pack verb. Each per-pack runtime is constructed independently
    // in the multi-backend boot path (unlike the single-backend
    // `KhiveMcpServer::with_packs` path), so both must be installed on every
    // runtime that could actually serve a generic `create`/`update`/`merge`
    // for a pack-owned kind, not just `default_runtime`.
    let owned_note_kinds: Vec<String> = registry
        .pack_owned_note_kinds()
        .into_iter()
        .map(str::to_string)
        .collect();
    default_runtime.install_pack_owned_note_kinds(owned_note_kinds.clone());
    let note_embedding_policies = registry.all_note_embedding_policies();
    default_runtime.install_note_embedding_policies(&note_embedding_policies);
    for rt in per_pack_runtimes_local.values() {
        rt.install_pack_owned_note_kinds(owned_note_kinds.clone());
        rt.install_note_embedding_policies(&note_embedding_policies);
    }
    // The validator is installed on every runtime the kind list reaches, not
    // just the default: each per-pack runtime is built independently, so none
    // of them shares the default's validator slot, and `core()` clones the
    // secondary's own slots rather than the default's. A runtime that has the
    // kind list but no validator enforces half the rule.
    registry.call_register_note_write_validators(&default_runtime);
    for rt in per_pack_runtimes_local.values() {
        registry.call_register_note_write_validators(rt);
    }

    let backend_for_pack: HashMap<&str, &StorageBackend> = per_pack_runtimes_local
        .iter()
        .map(|(name, rt)| (name.as_str(), rt.backend()))
        .collect();
    let main_ref: &StorageBackend = main_backend.as_ref();
    registry
        .apply_schema_plans_with_map(&backend_for_pack, main_ref)
        .map_err(|e| anyhow::anyhow!("pack schema boot failure: {e}"))?;

    let mut quarantine_sources: Vec<_> = backends
        .iter()
        .map(|(name, backend)| (name.as_str(), backend.as_ref()))
        .collect();
    quarantine_sources.sort_by(|left, right| left.0.cmp(right.0));
    crate::legacy_quarantine::repair_legacy_quarantine(&default_runtime, &quarantine_sources)
        .await?;

    // Wrap runtimes in Arc for the coordinator's BackendRegistry.
    let per_pack_runtimes_arc: HashMap<String, Arc<KhiveRuntime>> = per_pack_runtimes_local
        .into_iter()
        .map(|(k, v)| (k, Arc::new(v)))
        .collect();

    Ok(MultiBackendRegistry {
        registry,
        default_namespace: default_namespace.as_str().to_string(),
        config_id,
        per_pack_runtimes: per_pack_runtimes_arc,
        main_backend,
        default_runtime,
    })
}

/// Return true when the actor identity will produce unattributed comm sends and
/// a party-line inbox.
///
/// Fires when:
/// - `actor_id` is `None` (not configured) or `"local"` (the default fallback), AND
/// - the loaded pack list includes `"comm"`.
///
/// Pure predicate — no I/O, no logging. Callers emit the warning.
///
/// Delegates to the shared actor-identity policy (#567) so this predicate,
/// the gate's actor resolution, and storage-token minting can never disagree
/// about what counts as "unattributed".
pub(crate) fn should_warn_unattributed(actor_id: Option<&str>, loaded_packs: &[String]) -> bool {
    khive_runtime::should_warn_unattributed_actor(actor_id, loaded_packs)
}

/// Return true when strict actor-attribution mode is active.
///
/// Set `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` to opt in. When active, starting the
/// server with the `comm` pack loaded and no actor identity configured is a fatal
/// error instead of a warning. Default is OFF to preserve OSS single-actor
/// behaviour.
///
/// This closes the #199/#200 misconfiguration window for cloud deployments where
/// an operator who misses the startup warning would silently expose a party-line
/// inbox to all tenants.
pub(crate) fn is_strict_actor_mode() -> bool {
    std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Enforce the strict-actor mode contract at server construction time.
///
/// When `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`:
///   - If `actor_id` is `None`/`"local"` AND `"comm"` is in the pack list →
///     return `Err` with a clear message. The server must NOT be constructed.
///
/// When strict mode is OFF (default): return `Ok(())` unconditionally — the
/// caller is still responsible for emitting the non-fatal `should_warn_unattributed`
/// warning.
///
/// # Scope: dispatch paths only
///
/// This function MUST be called from every **SERVING/DISPATCH** construction path —
/// the paths that will actually route verb calls and read or write comm/tenant data:
/// - `build_server` and `build_server_multi_backend` in this file (the `kkernel mcp` paths)
/// - `build_registry_for_multi_backend` in this file (the ADR-029 coordinator path)
/// - `kkernel exec` (`crates/kkernel/src/exec.rs`) — dispatches arbitrary ops
/// - `khive_mcp::pending_events::run_pending_events` — drains and dispatches
///   scheduled events
///
/// **Pure-introspection registry construction is intentionally EXEMPT**
/// (`build_registry` in `crates/kkernel/src/pack_introspect.rs`,
/// `build_taxonomy` in `crates/kkernel/src/kg/validate.rs`) because it never
/// dispatches verbs or reads comm/tenant data — an operator must still be
/// able to introspect a strict-mode deployment.
pub fn enforce_strict_actor_mode(
    actor_id: Option<&str>,
    loaded_packs: &[String],
) -> anyhow::Result<()> {
    if is_strict_actor_mode() && should_warn_unattributed(actor_id, loaded_packs) {
        anyhow::bail!(
            "KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1 is set but no actor identity is \
             configured. Set KHIVE_ACTOR or --actor to this lambda's id before \
             starting in strict mode (comm pack requires an attributed actor to \
             prevent party-line inbox exposure)."
        );
    }
    Ok(())
}

/// Select the local pool size before an MCP host opens any store.
///
/// A forwarding client retains these pools for boot work, `save_to`, and local
/// fallback. Counts are fixed for the process lifetime; daemon and direct hosts
/// retain the ordinary default. An explicit reader count always wins.
pub fn mcp_max_readers(
    args: &Args,
    config: &RuntimeConfig,
    backends: &[BackendConfig],
    explicit: Option<usize>,
) -> Option<usize> {
    let file_backed = if args.db.as_deref() == Some(":memory:") {
        false
    } else if backends.is_empty() {
        config.db_path.is_some()
    } else {
        backends
            .iter()
            .any(|backend| backend.name == BackendId::MAIN && backend.kind == BackendKind::Sqlite)
    };
    #[cfg(unix)]
    let forwarding = !args.daemon && !khive_runtime::daemon::env_truthy("KHIVE_NO_DAEMON");
    #[cfg(not(unix))]
    let forwarding = false;
    explicit.or((forwarding && file_backed).then_some(1))
}

/// Build a fully-configured server from parsed args (without serving).
///
/// This is the supported production boot path for a single-backend server. The
/// returned future does not complete until bounded blob hydration is installed
/// and any resumable V21 attachment cutover has reached `Complete`; callers must
/// not replace it with direct `KhiveRuntime`/`KhiveMcpServer` construction for a
/// legacy database.
///
/// Returns, alongside the server, the resolved writable [`KhiveRuntime`]
/// handle the `"schedule"` pack is bound to. It is `None` when the resolved
/// pack set omits `"schedule"` or that pack's own assigned backend is
/// read-only, because every ticker pass begins with reclaim DML. This is the
/// SAME runtime the server itself dispatches through, never an independently
/// re-resolved one (PR #782 — see
/// `crates/khive-mcp/docs/api/pending-events.md`).
///
/// Derives identity explicitness from a real CLI parse and selects the MCP
/// pool policy before constructing stores. Native one-shot callers use
/// [`build_server_with_explicit_namespace`] and retain default pool sizing.
pub async fn build_server(args: &Args) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
    let (cli_namespace_explicit, cli_namespace) =
        resolve_cli_namespace(args).map_err(|e| anyhow::anyhow!("{e}"))?;
    build_server_inner(
        args,
        cli_namespace,
        cli_namespace_explicit,
        cli_namespace_explicit,
        true,
    )
    .await
}

/// Build a fully-configured server from parsed args plus an independently
/// resolved `(namespace, namespace_explicit, actor_explicit)` triple.
///
/// Like [`build_server`], this is an asynchronous host-boot boundary and returns
/// only after the attachment cutover is complete. This native one-shot builder
/// retains default pool sizing rather than selecting an MCP forwarding policy.
///
/// Extracted from [`build_server`] (PR #782) so non-interactive-CLI callers
/// (e.g. the `--pending-events` one-shot drain wrapper) can supply a
/// namespace default without it being misread as a genuine `--actor`
/// override. `build_server` derives `namespace_explicit` from a real CLI
/// parse, where "a namespace value is present" and "the operator explicitly
/// overrode the actor identity" are the same fact by construction. A caller
/// that synthesizes an `Args` value programmatically does not get to make
/// that inference — pass `actor_explicit: false` while `namespace_explicit`
/// is still `true` (the `kkernel exec` / `kkernel reindex` shape; see
/// `RuntimeConfigInputs::actor_explicit`'s field doc).
pub async fn build_server_with_explicit_namespace(
    args: &Args,
    namespace: khive_runtime::Namespace,
    namespace_explicit: bool,
    actor_explicit: bool,
) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
    build_server_inner(args, namespace, namespace_explicit, actor_explicit, false).await
}

async fn build_server_inner(
    args: &Args,
    namespace: khive_runtime::Namespace,
    namespace_explicit: bool,
    actor_explicit: bool,
    mcp_host: bool,
) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
    let prepared = prepare_server_boot(args, namespace, namespace_explicit, actor_explicit)?;
    build_server_from_prepared(args, prepared, mcp_host, None).await
}

struct PreparedServerBoot {
    config: RuntimeConfig,
    db_anchor: Option<PathBuf>,
    khive_cfg: KhiveConfig,
}

/// Resolve storage topology without opening a database. The daemon takes its
/// store locks from this exact snapshot before `build_server_from_prepared`
/// starts migrations; it must not rediscover HOME/config between the two.
fn prepare_server_boot(
    args: &Args,
    namespace: khive_runtime::Namespace,
    namespace_explicit: bool,
    actor_explicit: bool,
) -> anyhow::Result<PreparedServerBoot> {
    let (config, db_anchor) = resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace,
        namespace_explicit,
        actor_explicit,
        no_embed: args.no_embed,
        packs: if args.pack.is_empty() {
            None
        } else {
            Some(args.pack.clone())
        },
        brain_profile: args.brain_profile.clone(),
    })?;
    let config = {
        let mut config = config;
        if args.daemon {
            enable_events_forwarding_for_daemon(&mut config);
        }
        config
    };

    // Regression fence: `config.db_path` must agree with what the canonical
    // resolver derives from this same `--db` input, or `config_id` (computed
    // from `config.db_path` below) would silently desynchronize this process
    // from any daemon/peer anchored on the same database.
    khive_runtime::assert_captured_db_anchor_consistent(
        config.db_path.as_deref(),
        db_anchor.as_deref(),
    )?;

    // Load the KhiveConfig to check for multi-backend declarations (ADR-028).
    // When no [[backends]] are declared, fall through to the existing single-backend path
    // to preserve byte-for-byte backward compatibility.
    //
    // Deliberately `config_discovery_db_anchor(args.db.as_deref())`, NOT
    // `config.db_path` — `config.db_path` (already resolved above) materializes
    // the `$HOME/.khive/khive.db` default when `--db` is unset (#689), which
    // would re-anchor this reload's tier-3 project-local config discovery to
    // the home directory instead of the process cwd. This keeps the reload in
    // agreement with the discovery anchor `resolve_runtime_config` already used
    // to produce `config` above.
    let db_path_for_config = config_discovery_db_anchor(args.db.as_deref());
    let loaded_config = KhiveConfig::load_with_home_fallback_and_source(
        args.config.as_deref(),
        db_path_for_config.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("config error: {e}"))?;
    let config_source = loaded_config.as_ref().map(|(_, source)| source.as_path());
    let khive_cfg = loaded_config
        .as_ref()
        .map(|(config, _)| config.clone())
        .unwrap_or_default();

    if !khive_cfg.backends.is_empty() {
        reject_conflicting_db_override_with_source(
            args.db.as_deref(),
            &khive_cfg.backends,
            config_source,
        )?;
    }

    Ok(PreparedServerBoot {
        config,
        db_anchor,
        khive_cfg,
    })
}

async fn build_server_from_prepared(
    args: &Args,
    prepared: PreparedServerBoot,
    mcp_host: bool,
    daemon_claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
    let PreparedServerBoot {
        config,
        db_anchor,
        khive_cfg,
    } = prepared;

    let max_readers = if mcp_host {
        mcp_max_readers(args, &config, &khive_cfg.backends, None)
    } else {
        None
    };

    // Issue #1586: disclose the resolved database target once at startup so a
    // no-override invocation's silent default (`$HOME/.khive/khive.db`) is
    // visible in the operator's log alongside the other startup facts. The
    // backends slice keeps the line truthful in multi-backend mode, where the
    // config-declared backend paths — not `config.db_path` — receive writes.
    tracing::info!(target: "khive.boot", "{}", resolved_database_disclosure(config.db_path.as_deref(), &khive_cfg.backends));
    tracing::info!(
        target: "khive.boot",
        "{}",
        resolved_volume_lock_disclosure(config.volume_lock_dir.as_deref())
    );
    tracing::info!(target: "khive.boot", "{}", resolved_wal_ceiling_disclosure(&config, &khive_cfg.backends, args.db.as_deref() == Some(":memory:")));

    if khive_cfg.backends.is_empty() {
        let runtime = build_single_backend_runtime_with_max_readers(
            config,
            &khive_cfg,
            max_readers,
            daemon_claims,
        )
        .await?;
        #[cfg(feature = "bench-embedder")]
        {
            for name in runtime.registered_embedding_model_names() {
                runtime.register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
            }
        }
        enforce_strict_actor_mode(
            runtime.config().actor_id.as_deref(),
            &runtime.config().packs,
        )?;
        if should_warn_unattributed(
            runtime.config().actor_id.as_deref(),
            &runtime.config().packs,
        ) {
            tracing::warn!(
                "actor identity resolved to \"local\": comm sends will be stamped from \
                 \"local\" (unattributed) and comm.inbox will be unscoped (party-line). \
                 Set KHIVE_ACTOR or --actor to this lambda's id."
            );
        }
        let schedule_rt = writable_schedule_runtime(
            runtime
                .config()
                .packs
                .iter()
                .any(|p| p == "schedule")
                .then(|| runtime.clone()),
        );
        let fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
        let server = KhiveMcpServer::new_with_mounts(runtime)
            .await
            .map(|s| s.with_default_output_format(fmt))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok((server, schedule_rt));
    }

    // Multi-backend path (ADR-028).
    let multi = build_registry_for_multi_backend_with_db_anchor_and_max_readers_and_claims(
        config,
        &khive_cfg,
        args.db.as_deref(),
        db_anchor.as_deref(),
        max_readers,
        daemon_claims,
    )
    .await?;
    let schedule_rt = writable_schedule_runtime(
        multi
            .per_pack_runtimes
            .get("schedule")
            .map(|rt| (**rt).clone()),
    );
    let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);
    Ok((server, schedule_rt))
}

/// Canonicalize a SQLite backend path for deduplication (ADR-028 §8).
///
/// The database file may not exist yet at boot time, so this uses the shared
/// no-side-effect path resolver rather than creating the parent as part of
/// identity resolution. That distinction is mandatory for a missing
/// read-only snapshot: boot must fail without materializing either the parent
/// or database. `None` is returned for in-memory backends, which are never
/// deduplicated.
fn canonical_backend_path(cfg: &BackendConfig) -> anyhow::Result<Option<PathBuf>> {
    if cfg.kind == BackendKind::Memory {
        return Ok(None);
    }
    let path = match cfg.path.as_ref() {
        Some(p) => khive_runtime::expand_tilde(p),
        None => return Ok(None),
    };
    canonical_path_no_side_effects(&path)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("backend {}: cannot resolve path: {e}", cfg.name))
}

/// Resolve the daemon's physical SQLite targets before runtime construction.
/// The declared topology, when present, owns the actual files; the runtime's
/// default `db_path` is then only a config-discovery anchor. A `:memory:`
/// override makes every declared backend ephemeral.
#[cfg(unix)]
pub fn daemon_store_paths(
    resolved_db_path: Option<&std::path::Path>,
    backends: &[BackendConfig],
    force_memory: bool,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut resolved_db_path = resolved_db_path.map(std::path::Path::to_path_buf);
    let mut db_anchor = resolved_db_path.clone();
    let mut backends = backends.to_vec();
    Ok(prepare_daemon_store_plan(
        &mut resolved_db_path,
        &mut db_anchor,
        &mut backends,
        force_memory,
    )?
    .paths)
}

/// A daemon-only snapshot binding configured spellings to the canonical paths
/// it will actually open. Alias validation may re-resolve a spelling, but the
/// open target stays the captured canonical path.
#[cfg(unix)]
pub struct DaemonStorePlan {
    pub paths: Vec<PathBuf>,
    pub read_only_paths: Vec<PathBuf>,
    aliases: Vec<(PathBuf, PathBuf)>,
}

#[cfg(unix)]
impl DaemonStorePlan {
    pub fn assert_aliases_unchanged(&self) -> anyhow::Result<()> {
        for (configured, claimed) in &self.aliases {
            let current = canonical_path_no_side_effects(configured)?;
            anyhow::ensure!(
                current == *claimed,
                "configured database path {} changed target after claim (claimed {}, now {}); refusing daemon boot",
                configured.display(),
                claimed.display(),
                current.display()
            );
        }
        Ok(())
    }
}

/// Freeze every daemon SQLite open path before taking its sidecar claims.
/// This mutates only the daemon's private boot snapshot; clients and non-daemon
/// writers retain their original config paths.
#[cfg(unix)]
pub fn prepare_daemon_store_plan(
    resolved_db_path: &mut Option<PathBuf>,
    db_anchor: &mut Option<PathBuf>,
    backends: &mut [BackendConfig],
    force_memory: bool,
) -> anyhow::Result<DaemonStorePlan> {
    if force_memory {
        return Ok(DaemonStorePlan {
            paths: Vec::new(),
            read_only_paths: Vec::new(),
            aliases: Vec::new(),
        });
    }
    let mut paths = Vec::new();
    let mut read_only_paths = Vec::new();
    let mut aliases = Vec::new();
    if let Some(original) = resolved_db_path.clone() {
        let canonical = canonical_path_no_side_effects(&original)?;
        *resolved_db_path = Some(canonical.clone());
        if db_anchor.is_some() {
            *db_anchor = Some(canonical.clone());
        }
        if backends.is_empty() {
            // The legacy single-backend constructor infers read-only mode
            // from an existing file's permissions. Bind its daemon claim in
            // the same mode so a chmod-frozen snapshot still boots.
            if std::fs::metadata(&canonical).is_ok_and(|metadata| metadata.permissions().readonly())
            {
                read_only_paths.push(canonical.clone());
            }
            aliases.push((original, canonical.clone()));
            paths.push(canonical);
        }
    }
    for backend in backends.iter_mut() {
        if let Some(canonical) = canonical_backend_path(backend)? {
            let configured = backend.path.clone().expect("SQLite path was resolved");
            backend.path = Some(canonical.clone());
            aliases.push((configured, canonical.clone()));
            if backend.read_only {
                read_only_paths.push(canonical.clone());
            }
            paths.push(canonical);
        }
    }
    validate_effective_backend_alias_modes(backends)?;
    paths.sort();
    paths.dedup();
    read_only_paths.sort();
    read_only_paths.dedup();
    Ok(DaemonStorePlan {
        paths,
        read_only_paths,
        aliases,
    })
}

/// Bound on final-component symlink hops [`canonical_path_no_side_effects`]
/// will follow before giving up, mirroring the kernel's `ELOOP` limit — high
/// enough for any real alias chain, low enough to fail fast on a cycle.
const MAX_SYMLINK_HOPS: u32 = 40;

pub(crate) fn canonical_path_no_side_effects(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let expanded = khive_runtime::expand_tilde(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("cannot resolve current directory: {e}"))?
            .join(expanded)
    };

    // A dangling final-component symlink (the alias exists, its target does
    // not yet) makes `Path::exists()` below report `false` — it follows
    // symlinks, so a missing target reads as "nothing here". Read the link
    // manually first and resolve through it, so a declared alias like
    // `link.db -> target.db` compares equal to `target.db` even before either
    // file has been created. Iterate rather than recurse, and cap the hop
    // count, so a symlink cycle (a self-link or a mutual a<->b pair) fails
    // loud instead of recursing until the stack overflows.
    let mut current = absolute;
    for _ in 0..MAX_SYMLINK_HOPS {
        let Ok(link_target) = std::fs::read_link(&current) else {
            break;
        };
        current = if link_target.is_absolute() {
            link_target
        } else {
            match current.parent() {
                Some(parent) => parent.join(&link_target),
                None => link_target,
            }
        };
    }
    if std::fs::read_link(&current).is_ok() {
        anyhow::bail!(
            "too many levels of symbolic links resolving {}",
            path.display()
        );
    }
    let absolute = current;

    if absolute.exists() {
        return absolute
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("cannot canonicalize {}: {e}", absolute.display()));
    }

    // Neither `absolute` nor its immediate parent may exist yet (a fresh
    // install with several undeclared directory levels). Walk up to the
    // deepest ancestor that does exist, canonicalize only that (resolving any
    // symlinked directory component along the way), then rejoin the missing
    // tail lexically — never creating anything on disk. `Path::file_name`
    // returns `None` for a `..` component, so the walk records the real last
    // component instead; `.`/`..` in the missing tail are collapsed lexically
    // below, which is sound only for components with no filesystem identity
    // at all. `Path::exists` follows symlinks, so a DANGLING ancestor symlink
    // also reads as nonexistent there — but `missing/..` through a symlink
    // resolves relative to the link's target, not its location, so lexical
    // collapse would be wrong. Probe `symlink_metadata` (which succeeds on a
    // dangling link) and fail loud instead: such a path cannot be opened or
    // created through until the link target exists.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = absolute.as_path();
    let existing_ancestor = loop {
        let Some(parent) = probe.parent() else {
            break None;
        };
        tail.push(
            probe
                .components()
                .next_back()
                .map(|c| c.as_os_str().to_os_string())
                .unwrap_or_default(),
        );
        if parent.exists() {
            break Some(parent.to_path_buf());
        }
        if std::fs::symlink_metadata(parent).is_ok() {
            anyhow::bail!(
                "dangling symbolic link {} while resolving {}",
                parent.display(),
                path.display()
            );
        }
        probe = parent;
    };

    let Some(existing_ancestor) = existing_ancestor else {
        return Ok(absolute);
    };

    let canonical_ancestor = existing_ancestor
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot canonicalize {}: {e}", existing_ancestor.display()))?;

    Ok(tail
        .into_iter()
        .rev()
        .fold(canonical_ancestor, |mut acc, component| {
            if component == *".." {
                acc.pop();
                acc
            } else if component.is_empty() || component == *"." {
                acc
            } else {
                acc.join(component)
            }
        }))
}

/// Build a fully-wired multi-backend `KhiveMcpServer` (ADR-028).
///
/// Before this future resolves, all distinct secondary databases have passed the
/// no-attachment-authority inventory and the canonical main database has reached
/// V21 `Complete`. This ordering prevents a main-database GC sweep from becoming
/// eligible while a secondary still carries legacy blob liveness.
///
/// Called only when `[[backends]]` is non-empty in `khive.toml`. Delegates
/// registry assembly to [`build_registry_for_multi_backend`] and finishing
/// (pool + output format) to [`build_server_from_multi_backend_registry`] —
/// this function's entire body used to duplicate both (#603); it is now a
/// thin pass-through so a future wiring addition lands in exactly one place.
///
/// `pub` so `kkernel`'s coordinator-attached boot path can be compared
/// against it directly in the #603 parity regression test — both call sites
/// must produce servers with an identical wiring surface for the same config.
pub async fn build_server_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<KhiveMcpServer> {
    khive_runtime::assert_db_anchor_consistent(base_config.db_path.as_deref(), cli_db_override)?;
    let multi =
        build_registry_for_multi_backend_inner(base_config, khive_cfg, cli_db_override).await?;
    Ok(build_server_from_multi_backend_registry(
        multi, khive_cfg, None,
    ))
}

/// Build a coordinated multi-backend server while checking a canonical
/// database anchor captured earlier in config discovery.
///
/// This has the same secondary-inventory and V21-completion guarantee as
/// [`build_server_multi_backend`].
pub async fn build_server_multi_backend_with_db_anchor(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> anyhow::Result<KhiveMcpServer> {
    // The db-anchor consistency guard runs inside `build_registry_for_multi_backend`
    // (the shared choke point every multi-backend boot path funnels through),
    // so it is not duplicated here.
    let multi = build_registry_for_multi_backend_with_db_anchor(
        base_config,
        khive_cfg,
        cli_db_override,
        db_anchor,
    )
    .await?;
    Ok(build_server_from_multi_backend_registry(
        multi, khive_cfg, None,
    ))
}

/// Finish constructing a `KhiveMcpServer` from an already-built
/// [`MultiBackendRegistry`] (#603).
///
/// This is the ONE place that applies every wiring step a multi-backend boot
/// needs on top of the registry: the ADR-078 output-format default, the
/// ADR-091 Planks 0+2 checkpoint pool, and — only for callers that pass one —
/// the cross-backend coordinator (ADR-029 Phase 2). [`build_server_multi_backend`]
/// (this file, `coordinator: None`) and `kkernel`'s `Command::Mcp` multi-backend
/// branch (`crates/kkernel/src/main.rs`, `coordinator: Some(..)`) both call this
/// instead of hand-assembling the server, so a future wiring addition (the
/// fourth `pool`-style patch) is a change to this one function, not to two
/// call sites — #503, ADR-078's inline output-format patch, and #601 each
/// missed wiring by landing only in the hand-copied kkernel branch.
pub fn build_server_from_multi_backend_registry(
    multi: MultiBackendRegistry,
    khive_cfg: &KhiveConfig,
    coordinator: Option<Arc<dyn crate::coordinator::CoordinatorService>>,
) -> KhiveMcpServer {
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    let channel_loop_admission = crate::server::ChannelLoopAdmission::for_pack_runtimes(
        multi.per_pack_runtimes.get("comm").map(Arc::as_ref),
        multi.per_pack_runtimes.get("blob").map(Arc::as_ref),
    );
    // The delivery loops scan, claim, and mark outbound `message` notes, so
    // they must hold the runtime that owns comm's rows — under a
    // `[packs.comm]` backend assignment that is the comm pack's runtime, not
    // the kg/main one (which would list an empty outbox forever).
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    let channel_outbox_runtime = multi
        .per_pack_runtimes
        .get("comm")
        .map(|runtime| runtime.as_ref().clone());
    // Wire the main backend's pool for background WAL checkpointing. The pool is
    // only present for file-backed databases; in-memory backends return None here
    // so that checkpoint_once never runs on a non-WAL connection.
    let pool = checkpoint_pool_for(multi.main_backend.as_ref());
    // ADR-091 Amendment 3: every OTHER file-backed backend this registry
    // wired, so the session sweep (`spawn_session_walpin_sweep`) and the
    // daemon's checkpoint task can attribute and checkpoint them instead of
    // leaving them permanently invisible to cross-process WAL-pin attribution.
    let secondary_pools = secondary_file_backed_pools(&multi);
    let fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
    let default_runtime = multi.default_runtime.clone();

    let server = KhiveMcpServer::from_registry_with_meta(
        multi.registry,
        &multi.default_namespace,
        &multi.config_id,
    )
    .with_default_output_format(fmt)
    .with_secondary_pools(secondary_pools)
    .with_runtime(default_runtime);

    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    let server = server
        .with_channel_outbox_runtime(channel_outbox_runtime)
        .with_channel_loop_admission(channel_loop_admission);

    let server = match coordinator {
        Some(c) => server.with_coordinator(c),
        None => server,
    };

    match pool {
        Some(p) => server.with_pool(p),
        None => server,
    }
}

/// Distinct file-backed backend pools among `multi`'s per-pack runtimes,
/// excluding the main backend's own pool (wired separately via
/// [`checkpoint_pool_for`]) — ADR-091 Amendment 3 fan-out needs exactly one
/// entry per additional file-backed backend the registry wired, not one per
/// pack, since several packs can share a backend.
///
/// Dedup is by canonical database identity (each pool's [`TxOrigin::Database`]
/// origin, minted from its canonical path — see `ConnectionPool::origin`),
/// never by pool pointer: two backends configured with alias spellings of
/// the SAME file (a direct path and a symlinked path, say) mint two distinct
/// `Arc<ConnectionPool>` but converge on one canonical sidecar, and admitting
/// both would race two `SweepBackend`s on the same heartbeat file.
fn secondary_file_backed_pools(multi: &MultiBackendRegistry) -> Vec<Arc<ConnectionPool>> {
    use khive_storage::tx_registry::TxOrigin;

    let mut seen: HashSet<khive_storage::tx_registry::DbIdentity> = HashSet::new();
    if let TxOrigin::Database(id) = multi.main_backend.pool_arc().origin() {
        seen.insert(id);
    }
    let mut pools = Vec::new();
    for rt in multi.per_pack_runtimes.values() {
        let backend = rt.backend();
        if !backend.is_file_backed() || backend.is_read_only() {
            continue;
        }
        let pool = backend.pool_arc();
        let TxOrigin::Database(id) = pool.origin() else {
            // A file-backed backend always mints a `Database` origin
            // (`ConnectionPool::origin`'s own contract); anything else here
            // has no canonical identity to dedup on, so it cannot be
            // admitted as a secondary fan-out target.
            continue;
        };
        if seen.insert(id) {
            pools.push(pool);
        }
    }
    pools
}

/// Construction-time facts that every multi-backend boot path must agree on
/// for identical input config (#603) — the parity contract the shared
/// [`build_server_from_multi_backend_registry`] constructor exists to
/// guarantee. Extend this struct (not the call sites) when a future wiring
/// addition needs its own parity coverage.
#[derive(Debug, PartialEq, Eq)]
pub struct WiringSurface {
    /// Whether a checkpoint pool was wired (#601/#604 — ADR-091 Planks 0+2).
    pub has_checkpoint_pool: bool,
    /// The resolved ADR-078 default output format.
    pub output_format: OutputFormat,
    /// Whether the default ingest namespace passes the email loop's gate
    /// preflight (#503/#602). This captures only the existing public
    /// authorization surface; runtime-mode admission remains private server
    /// wiring and independently prevents read-only-backed tasks from starting.
    /// Only meaningful when the `channel-email` feature is compiled in.
    #[cfg(feature = "channel-email")]
    pub channel_loop_eligible: bool,
}

impl WiringSurface {
    /// Capture the wiring surface of an already-built server.
    pub fn capture(server: &KhiveMcpServer) -> Self {
        Self {
            has_checkpoint_pool: server.pool().is_some(),
            output_format: server.default_output_format(),
            #[cfg(feature = "channel-email")]
            channel_loop_eligible: preflight_ingest_namespace(
                &ingest_namespace_from_env(),
                &server.verb_registry_clone(),
            ),
        }
    }
}

/// Derive the checkpoint pool for a multi-backend boot's `main` backend
/// (ADR-091 Planks 0+2). The pool is only present for file-backed databases;
/// in-memory backends must never drive `checkpoint_once` on a non-WAL
/// connection.
///
/// Called from exactly one place now: [`build_server_from_multi_backend_registry`]
/// (#603) — both multi-backend boot paths (`build_server_multi_backend` in this
/// file and `kkernel`'s `Command::Mcp` coordinator branch) go through that shared
/// constructor, so this derivation is no longer hand-copied at each call site
/// (#601, #604).
pub fn checkpoint_pool_for(main_backend: &StorageBackend) -> Option<Arc<ConnectionPool>> {
    if main_backend.is_file_backed() && !main_backend.is_read_only() {
        Some(main_backend.pool_arc())
    } else {
        None
    }
}

/// Resolve `khive.toml`'s `[storage.blob]` selection against `backend`
/// (ADR-111 Amendment 2's boot-wiring requirement).
///
/// Returns the resolved store/hydration pair on success so multi-backend
/// callers can install the exact same aggregate budget on every runtime
/// without re-resolving or reconstructing it.
///
/// An **explicit** `[storage.blob]` section that fails to resolve (an `s3`
/// backend with no AWS credentials in the environment, an invalid prefix,
/// etc.) aborts boot: silently falling back to `FsBlobStore` would defeat
/// the point of declaring `backend = "s3"`. When `[storage.blob]` is
/// **absent**, a resolution failure (e.g. an in-memory backend with no root
/// to default beside — every `--db :memory:` invocation and most unit
/// tests) is non-fatal and leaves `KhiveRuntime::blob_store()` unset:
/// nothing yet consumes it, and forcing a filesystem root onto every
/// in-memory boot would be a behavior change nobody asked for.
///
fn resolve_blob_hydrator_for_boot(
    config: &RuntimeConfig,
    khive_cfg: &KhiveConfig,
    backend: &StorageBackend,
    governing_backend: &StorageBackend,
) -> anyhow::Result<Option<Arc<BlobHydrator>>> {
    // Resolution and construction are fused inside the runtime so the mode
    // is DERIVED from the governing backend's own access mode (the backend
    // the blob pack maps to, ADR-160 D3) rather than declared here, and the
    // hydrator comes back stamped as governed — the only kind the shared
    // install seam accepts. Read-only derivation opens an existing root and
    // never creates one; the extra wrap over an already-wrapped store is
    // documented harmless (reads delegate, mutators refuse).
    match BlobHydrator::resolve_for_governing_backend(
        khive_cfg,
        backend,
        governing_backend,
        config.blob_hydration_bytes,
    ) {
        Ok(hydrator) => Ok(Some(Arc::new(hydrator))),
        Err(khive_runtime::GovernedBlobError::Resolve(error))
            if khive_cfg.storage.blob.is_none() =>
        {
            tracing::debug!(
                error = %error,
                "no usable BlobStore for this backend and no [storage.blob] configured; \
                 leaving KhiveRuntime::blob_store() unset"
            );
            Ok(None)
        }
        Err(error) => Err(anyhow::anyhow!(
            "[storage.blob] configuration error: {error}"
        )),
    }
}

/// Open, migrate, coordinate V21, and construct one single-backend runtime.
///
/// Both `serve` and `kkernel exec` use this real-host async choke point. The
/// runtime is not exposed until the attachment/GC cutover is complete, and no
/// temporary Tokio runtime or writer-task lifecycle domain is introduced.
pub async fn build_single_backend_runtime(
    config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> anyhow::Result<KhiveRuntime> {
    build_single_backend_runtime_with_max_readers(config, khive_cfg, None, None).await
}

async fn build_single_backend_runtime_with_max_readers(
    mut config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    max_readers: Option<usize>,
    daemon_claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<KhiveRuntime> {
    #[cfg(unix)]
    preflight_events_socket_for_boot(&config, &[], false)?;
    let email_policy = OutboundEmailPolicy::from_env().map_err(anyhow::Error::msg)?;
    let backend = Arc::new(claimed_backend::open_single_backend(
        &mut config,
        max_readers,
        daemon_claims,
    )?);
    if let Some(claims) = daemon_claims {
        khive_runtime::daemon::assert_daemon_store_identities(claims)?;
    }
    prepare_core_schema_for_boot(Arc::clone(&backend), "single backend").await?;

    let hydrator =
        resolve_blob_hydrator_for_boot(&config, khive_cfg, backend.as_ref(), backend.as_ref())?;
    crate::attachment_cutover::coordinate_attachment_cutover(
        Arc::clone(&backend),
        hydrator.clone(),
        crate::attachment_cutover::legacy_preference_verifier(),
    )
    .await?;

    let runtime = KhiveRuntime::from_prepared_backend(backend, config)?
        .with_outbound_email_policy(email_policy)
        .with_declared_backend_db_paths(declared_backend_db_paths(khive_cfg));
    if let Some(hydrator) = hydrator {
        runtime.install_blob_hydrator(hydrator)?;
    }
    crate::legacy_quarantine::repair_legacy_quarantine(&runtime, &[("main", runtime.backend())])
        .await?;
    Ok(runtime)
}

fn open_single_backend(
    config: &mut RuntimeConfig,
    max_readers: Option<usize>,
) -> anyhow::Result<StorageBackend> {
    claimed_backend::open_single_backend(config, max_readers, None)
}

async fn prepare_single_backend_for_schema_admin(
    config: &RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> anyhow::Result<Arc<StorageBackend>> {
    let mut config = config.clone();
    let backend = Arc::new(open_single_backend(&mut config, None)?);
    prepare_core_schema_for_boot(Arc::clone(&backend), "single backend").await?;

    let hydrator = if schema_admin_requires_blob_hydrator(Arc::clone(&backend)).await? {
        resolve_blob_hydrator_for_boot(&config, khive_cfg, backend.as_ref(), backend.as_ref())?
    } else {
        None
    };
    crate::attachment_cutover::coordinate_attachment_cutover(
        Arc::clone(&backend),
        hydrator,
        crate::attachment_cutover::legacy_preference_verifier(),
    )
    .await?;
    Ok(backend)
}

async fn prepare_core_schema_for_boot(
    backend: Arc<StorageBackend>,
    label: impl Into<String>,
) -> anyhow::Result<u32> {
    let label = label.into();
    tokio::task::spawn_blocking(move || backend.prepare_core_schema())
        .await
        .map_err(|error| anyhow::anyhow!("{label}: schema preparation task panicked: {error}"))?
        .map_err(|error| anyhow::anyhow!("{label}: schema preparation: {error}"))
}

/// Resolve and install blob hydration on an already prepared runtime.
///
/// This low-level compatibility helper does **not** run or validate the
/// application-assisted V21 attachment cutover. Production host boot should use
/// [`build_single_backend_runtime`], [`build_server`], or the async multi-backend
/// builders instead. Calling this helper is sound only when `backend` is the
/// runtime's backend and its attachment-cutover status is already `Complete`;
/// the same-backend precondition is now enforced by identity rather than
/// documented, since the resolved mode derives from `backend` and a mismatch
/// would silently resolve the wrong store mode for the receiving runtime.
pub fn install_resolved_blob_store(
    rt: &KhiveRuntime,
    khive_cfg: &KhiveConfig,
    backend: &StorageBackend,
) -> anyhow::Result<Option<Arc<BlobHydrator>>> {
    if !std::ptr::eq(rt.backend(), backend) {
        anyhow::bail!(
            "install_resolved_blob_store requires the runtime's own backend: the supplied \
             backend reference is not the runtime's"
        );
    }
    let hydrator = resolve_blob_hydrator_for_boot(rt.config(), khive_cfg, backend, backend)?;
    if let Some(hydrator) = hydrator.as_ref() {
        rt.install_blob_hydrator(Arc::clone(hydrator))?;
    }
    Ok(hydrator)
}

/// Group the pools opened during boot by canonical file. Every configured name
/// survives aliasing, including names not assigned to a loaded pack. In-memory
/// pools have no file identity and are grouped only when they share one pool.
fn opened_diagnostic_backends(
    backends: &HashMap<String, Arc<StorageBackend>>,
) -> Arc<[OpenedDiagnosticBackend]> {
    let mut names: Vec<_> = backends.keys().cloned().collect();
    names.sort();
    names.sort_by_key(|name| name != BackendId::MAIN);
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    let mut by_memory_pool: HashMap<usize, usize> = HashMap::new();
    let mut opened: Vec<OpenedDiagnosticBackend> = Vec::new();
    for name in names {
        let pool = backends[&name].pool_arc();
        let path = pool.canonical_path().map(PathBuf::from);
        let existing = match &path {
            Some(path) => by_path.get(path).copied(),
            None => by_memory_pool.get(&(Arc::as_ptr(&pool) as usize)).copied(),
        };
        if let Some(index) = existing {
            opened[index].backend_names.push(name);
            continue;
        }
        let index = opened.len();
        match &path {
            Some(path) => {
                by_path.insert(path.clone(), index);
            }
            None => {
                by_memory_pool.insert(Arc::as_ptr(&pool) as usize, index);
            }
        }
        opened.push(OpenedDiagnosticBackend {
            backend_names: vec![name],
            canonical_path: path,
            pool,
        });
    }
    opened.into()
}

/// Construct one per-pack runtime, wiring `core_backend` for secondary-backend packs.
///
/// Centralizing this in one helper ensures that both `build_registry_for_multi_backend`
/// and `build_server_multi_backend` apply the same ADR-073 wiring. Without it, a
/// secondary pack served via `build_server_multi_backend` would receive
/// `core_backend = None`, causing `core()` to fall back to `self.clone()` and write
/// linkable records to the secondary backend instead of main.
fn build_pack_runtime(
    backend: Arc<StorageBackend>,
    backend_name: &str,
    rt_config: RuntimeConfig,
    main_backend: &Arc<StorageBackend>,
    main_runtime: &KhiveRuntime,
    declared_backend_db_paths: Arc<[PathBuf]>,
    diagnostic_backends: Arc<[OpenedDiagnosticBackend]>,
) -> KhiveRuntime {
    // Every pack runtime carries main's embedder wiring for core(): a
    // main-assigned pack has no core pointer, but with `no_embed` its own
    // registry is empty and core-routed concept writes must still embed.
    let rt = KhiveRuntime::from_backend(backend, rt_config)
        .with_outbound_email_policy(main_runtime.outbound_email_policy().clone())
        .with_declared_backend_db_paths(declared_backend_db_paths)
        .with_diagnostic_backends(diagnostic_backends)
        .with_diagnostic_observer_from(main_runtime)
        .with_core_embedders_from(main_runtime);
    if backend_name != BackendId::MAIN {
        rt.with_core_backend(main_backend.clone())
    } else {
        rt
    }
}

/// Resolve the `--db`/`KHIVE_DB` value into the anchor used for tier-3
/// project-local `.khive/config.toml` DISCOVERY — as distinct from
/// [`khive_runtime::resolve_db_anchor`], which always materializes a concrete
/// anchor (defaulting to `$HOME/.khive/khive.db`) for the database that is
/// actually about to be opened.
///
/// An explicit `--db`/`KHIVE_DB` still anchors discovery to that path, for the
/// same config_id-coherence reason `resolve_db_anchor` documents. But when no
/// db was supplied, this returns `None` instead of the materialized home
/// default (#689): passing the home-default path into
/// `KhiveConfig::load_with_home_fallback`'s `db_path` collapses tier 3 onto
/// `$HOME/.khive/config.toml`, silently skipping the project-local
/// `<cwd>/.khive/config.toml` that `project_config_anchor_dir` documents as
/// the `db_path == None` fallback.
pub fn config_discovery_db_anchor(db: Option<&str>) -> Option<std::path::PathBuf> {
    db.and_then(|d| khive_runtime::resolve_db_anchor(Some(d)))
}

/// One-line disclosure of the database target(s) this process will write to,
/// for CLI entry points that resolve a database path. Returns a sentence
/// naming the resolved target: the concrete file path, the ephemeral
/// in-memory marker when the resolved path is `:memory:`, or — when the
/// loaded config declares `[[backends]]` — the config-declared backend
/// targets, which are what actually receive writes in multi-backend mode
/// (the single resolved anchor path is a discovery/fingerprint input there,
/// not a write target).
///
/// The `:memory:` arm (resolved path `None`) wins even when backends are
/// declared: a `:memory:` override forces every declared backend ephemeral
/// (`force_memory` in [`validate_db_override_against_backends`]'s caller), so
/// the ephemeral line is the truthful one.
///
/// Issue #1586: with no `--db`/`KHIVE_DB` override the resolver silently
/// targets the default `$HOME/.khive/khive.db` — the production database for
/// most installs. Naming the resolved target once at startup makes that
/// implicit choice visible without adding a prompt or refusal. Callers decide
/// the channel: `kkernel mcp` logs it at INFO with its other startup facts;
/// `kkernel exec` prints it to stderr (its default log level is `warn`, so an
/// INFO record would never surface there).
pub fn resolved_database_disclosure(
    resolved_db_path: Option<&std::path::Path>,
    backends: &[BackendConfig],
) -> String {
    match resolved_db_path {
        None => "database: :memory: (ephemeral in-memory; nothing persists)".to_string(),
        Some(_) if !backends.is_empty() => {
            let targets: Vec<String> = backends
                .iter()
                .map(|backend| match (&backend.kind, backend.path.as_deref()) {
                    // Kind decides first: a `memory` backend's `path` is
                    // ignored by construction (see `BackendConfig::path`), so
                    // a stray configured path must not be presented as a
                    // write target.
                    (BackendKind::Memory, _) => format!("{}=:memory:", backend.name),
                    (BackendKind::Sqlite, Some(path)) => {
                        format!("{}={}", backend.name, path.display())
                    }
                    (BackendKind::Sqlite, None) => format!("{}=<unresolved>", backend.name),
                })
                .collect();
            format!(
                "database: config-declared backends govern storage targets: {}",
                targets.join(", ")
            )
        }
        Some(path) => format!("database: {} (resolved)", path.display()),
    }
}

/// Name the volume-lock directory this process resolved, beside its storage
/// targets. A process carrying the workspace test marker resolves a temporary
/// namespace instead of the per-user one and scopes its in-process leases by
/// lock directory, and the marker is only an environment variable, so the
/// startup line is where that shows.
pub fn resolved_volume_lock_disclosure(lock_dir: Option<&std::path::Path>) -> String {
    volume_lock_disclosure(lock_dir, khive_db::harness_scoped_process_leases())
}

fn volume_lock_disclosure(lock_dir: Option<&std::path::Path>, harness_scoped: bool) -> String {
    match lock_dir {
        Some(dir) if harness_scoped => format!(
            "volume locks: {} (test marker: in-process leases scoped by lock directory)",
            dir.display()
        ),
        Some(dir) => format!("volume locks: {}", dir.display()),
        None => "volume locks: unresolved; writable SQLite opens are refused".to_string(),
    }
}

/// Report the resolved WAL policy for every storage target at startup. This
/// uses the same configuration snapshot as daemon identity and backend open.
pub fn resolved_wal_ceiling_disclosure(
    config: &RuntimeConfig,
    backends: &[BackendConfig],
    force_memory: bool,
) -> String {
    fn source_name(source: khive_runtime::WalCeilingSource) -> &'static str {
        match source {
            khive_runtime::WalCeilingSource::BackendField => "backend_field",
            khive_runtime::WalCeilingSource::Environment => "environment",
            khive_runtime::WalCeilingSource::Default => "default",
        }
    }

    fn row(name: &str, configured: u64, effective: u64, source: &str, read_only: bool) -> String {
        let status = if read_only && configured > 0 {
            "read_only_not_enforced"
        } else if effective > 0 {
            "enforced"
        } else {
            "disabled"
        };
        // A backend name is operator-supplied text, so it takes the runtime's
        // log-text path: credential shapes are masked, and control, format and
        // line/paragraph separator characters (newline, escape sequences, bidi
        // overrides) are escaped so a name cannot start a forged log line or
        // drive the terminal.
        let name = khive_runtime::secret_gate::bounded_masked_log_text(name);
        format!(
            "{name}: configured_bytes={configured} effective_bytes={effective} source={source} enabled={} status={status}",
            effective > 0
        )
    }

    if backends.is_empty() {
        let read_only = config.db_path.as_deref().is_some_and(|path| {
            std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().readonly())
        });
        let effective = if config.db_path.is_none() || read_only {
            0
        } else {
            config.wal_ceiling_bytes
        };
        return format!(
            "wal_ceiling: {}",
            row(
                BackendId::MAIN,
                config.wal_ceiling_configured_bytes,
                effective,
                source_name(config.wal_ceiling_source),
                read_only,
            )
        );
    }

    let mut rows = Vec::with_capacity(backends.len());
    for backend in backends {
        let (configured, source) = if force_memory {
            (0, khive_runtime::WalCeilingSource::Default)
        } else {
            match backend.wal_ceiling_bytes {
                Some(bytes) => (bytes, khive_runtime::WalCeilingSource::BackendField),
                None if backend.kind == BackendKind::Memory => {
                    (0, khive_runtime::WalCeilingSource::Default)
                }
                None => (
                    config.wal_ceiling_configured_bytes,
                    config.wal_ceiling_source,
                ),
            }
        };
        let memory = force_memory || backend.kind == BackendKind::Memory;
        let read_only = backend.read_only;
        let effective = if memory || read_only { 0 } else { configured };
        rows.push(row(
            &backend.name,
            configured,
            effective,
            source_name(source),
            read_only,
        ));
    }
    rows.sort();
    format!("wal_ceiling: {}", rows.join("; "))
}

/// One-line, stdout-safe disclosure of the actor selected by the resolved
/// CLI/project/environment precedence chain.
pub fn resolved_actor_disclosure(actor_id: Option<&str>) -> String {
    let actor = khive_runtime::resolve_actor(actor_id);
    if khive_runtime::actor_is_unattributed(&actor) {
        format!(
            "actor: {:?} (resolved; unattributed local fallback)",
            actor.id
        )
    } else {
        format!("actor: {:?} (resolved; attributed)", actor.id)
    }
}

/// Inputs for [`resolve_runtime_config`] — the subset of serve-time arguments
/// that determine the resolved [`RuntimeConfig`]. Callers other than
/// `kkernel mcp` (e.g. `kkernel reindex`) supply these directly so they resolve
/// the SAME engines, db path, and actor namespace the MCP server would.
pub struct RuntimeConfigInputs<'a> {
    /// Raw `--db` / `KHIVE_DB` value (`:memory:` sentinel honored).
    pub db: Option<&'a str>,
    /// Explicit `--config` / `KHIVE_CONFIG` path (else home-fallback search).
    pub config: Option<&'a std::path::Path>,
    /// Pre-resolved default namespace.
    pub namespace: khive_runtime::Namespace,
    /// Whether the namespace came from an explicit CLI flag (skips config tier).
    pub namespace_explicit: bool,
    /// Whether the caller holds a GENUINE explicit actor/identity override —
    /// i.e. an operator actually typed `--actor` / `--namespace` (ADR-057).
    ///
    /// Distinct from `namespace_explicit`: `kkernel exec` and `kkernel reindex`
    /// set `namespace_explicit: true` unconditionally (their `--namespace` arg
    /// has no `Option` to distinguish "typed" from "default"), but they have no
    /// `--actor` flag and must NOT suppress the project/db actor-id tiers when
    /// their namespace happens to resolve to `"local"`. Only `kkernel mcp`
    /// (`build_server`, via `resolve_cli_namespace`) sets this to a value that
    /// can suppress those tiers — everyone else passes `false`.
    pub actor_explicit: bool,
    /// Disable embedding entirely (still resolves actor namespace from config).
    pub no_embed: bool,
    /// Explicit CLI packs to register. `None` falls through to `KHIVE_PACKS`,
    /// `[runtime].packs`, then the built-in production set.
    pub packs: Option<Vec<String>>,
    /// Explicit brain profile ID (highest-priority tier).
    ///
    /// `None` lets lower tiers (env var, config file, runtime fallback) handle
    /// resolution. Pass `Some(id)` only when the caller holds an explicit CLI value.
    pub brain_profile: Option<String>,
}

/// Resolve a [`RuntimeConfig`] from serve-time inputs, applying the SAME
/// config-file / env / actor-namespace precedence as `kkernel mcp`.
///
/// Extracted from `build_server` so `kkernel reindex` reuses the exact engine
/// and db resolution — otherwise an admin reindex writes vectors for the
/// default/env model set while the MCP server serves recall from the
/// config-file `[[engines]]` set.
pub fn resolve_runtime_config(inputs: RuntimeConfigInputs<'_>) -> anyhow::Result<RuntimeConfig> {
    let (config, _) = resolve_runtime_config_with_db_anchor(inputs)?;
    Ok(config)
}

/// Resolve a [`RuntimeConfig`] and return the database anchor captured at the
/// same construction boundary. Server boot paths thread this value through
/// consistency validation and registry construction without re-reading HOME.
pub fn resolve_runtime_config_with_db_anchor(
    inputs: RuntimeConfigInputs<'_>,
) -> anyhow::Result<(RuntimeConfig, Option<PathBuf>)> {
    let db_anchor = khive_runtime::resolve_db_anchor(inputs.db);
    let db_path = db_anchor.clone();

    let cli_packs = inputs.packs.filter(|packs| !packs.is_empty());
    let env_packs = std::env::var("KHIVE_PACKS")
        .ok()
        .map(|value| parse_pack_list(&value))
        .filter(|packs| !packs.is_empty());
    let packs_overridden = cli_packs.is_some() || env_packs.is_some();
    let packs = cli_packs
        .or(env_packs)
        .unwrap_or_else(RuntimeConfig::built_in_packs);

    // Tier-1: explicit CLI --brain-profile only (not env — env is tier-3, after TOML).
    // We must NOT read KHIVE_BRAIN_PROFILE here; RuntimeConfig::default() reads it, so
    // we exclude brain_profile from the default spread and set it to None (CLI-only).
    let cli_brain_profile = inputs.brain_profile.filter(|s| !s.trim().is_empty());

    // Threaded into the config-file resolvers so tier-3 project-local config
    // discovery anchors to the resolved database's directory rather than the
    // process cwd when an explicit `--db`/`KHIVE_DB` is given (kills config_id
    // drift between a client and the daemon serving the same database at a
    // different working directory). Deliberately NOT the base config's own
    // `db_path` (which materializes the `$HOME/.khive/khive.db` default when
    // unset, #689) — an unset db must fall through to cwd-anchored discovery
    // instead of silently searching the home directory.
    let db_path_for_config = config_discovery_db_anchor(inputs.db);

    let resolved = if inputs.no_embed {
        // `RuntimeConfig::no_embeddings()` is the canonical "zero embedders"
        // constructor (issue #396) — it clears `embedding_model` and
        // `additional_embedding_models` together, unlike a manual two-field
        // override which can leave `additional_embedding_models` populated
        // from `KHIVE_ADDITIONAL_EMBEDDING_MODELS`.
        let no_embed_base = RuntimeConfig {
            db_path,
            default_namespace: inputs.namespace,
            packs,
            // Explicit CLI flag only at this tier — env and config-file tiers are applied
            // below in resolve_actor_from_config and apply_env_brain_profile.
            brain_profile: cli_brain_profile,
            ..RuntimeConfig::no_embeddings()
        };
        resolve_actor_from_config(
            inputs.config,
            no_embed_base,
            db_path_for_config.as_deref(),
            packs_overridden,
            inputs.db == Some(":memory:"),
        )?
    } else {
        let base_config = RuntimeConfig {
            db_path,
            default_namespace: inputs.namespace,
            packs,
            // Explicit CLI flag only at this tier — env and config-file tiers are applied
            // below in resolve_config and apply_env_brain_profile.
            brain_profile: cli_brain_profile,
            ..RuntimeConfig::default()
        };
        resolve_config(
            inputs.config,
            base_config,
            db_path_for_config.as_deref(),
            packs_overridden,
            inputs.db == Some(":memory:"),
        )?
    };

    // ADR-096 Fork 2 — per-connection `actor_id` precedence chain (highest to
    // lowest), ratified 2026-07-05:
    //
    //   1. Explicit CLI `--actor` / `--namespace` flag (ADR-057), threaded via
    //      `inputs.namespace` / `inputs.actor_explicit` (`resolve_cli_namespace`,
    //      only `build_server` sets `actor_explicit` from a real CLI parse —
    //      see the field doc on `RuntimeConfigInputs::actor_explicit`).
    //      `args.actor` no longer carries a `KHIVE_ACTOR` env-arg alias (the
    //      clap `env` binding was removed from the tier-1 field — see
    //      `args.rs`), so this tier is CLI-flag-only; a bare shell-level
    //      `KHIVE_ACTOR` can no longer masquerade as an explicit flag. When
    //      genuinely explicit, tiers 2-3 below are NOT consulted at all — an
    //      explicit `--actor local` must resolve to anonymous (`None`), not
    //      fall through to a project/db/env actor (the gap this block also
    //      closes). `kkernel exec`/`reindex` force
    //      `namespace_explicit: true` for unrelated reasons (no `Option` on
    //      their `--namespace` arg) but always pass `actor_explicit: false`,
    //      so they keep falling through to tiers 2-3 exactly as before.
    //   2. Project/cwd-anchored config `[actor].id`, resolved INDEPENDENTLY of
    //      the database-anchored config load above (`resolve_project_actor_id`).
    //      Commit 10d9c92c (#651) anchored tier-3 `.khive/config.toml` discovery
    //      to the resolved database's own directory — correct for `config_id`
    //      coherence between a client and a daemon sharing one database, but it
    //      also relocated `[actor]` discovery away from the connecting process's
    //      own project. This tier restores it as a SEPARATE lookup.
    //   3. Whatever `resolved.actor_id` already carries from the
    //      database-anchored config load / `KHIVE_ACTOR` env direct-read
    //      (`resolve_config` / `resolve_actor_from_config` / `RuntimeConfig::
    //      default()` above) — the pre-#651-drift fallback tier. This is the
    //      ONLY place `KHIVE_ACTOR` env feeds `actor_id`; it never touches
    //      `default_namespace`.
    //   4. Anonymous (`None`).
    //
    // Attribution-only: none of these tiers may feed `config_id` (`actor_id` is
    // not read by `compute_config_id`) or `default_namespace` (tier 1 already
    // sets `default_namespace` via `inputs.namespace` — unchanged pre-existing
    // behavior; tiers 2-4 never touch it, per ADR-007 Rev 4 Rule 0).
    let resolved = {
        let mut resolved = resolved;
        let ns = resolved.default_namespace.as_str().to_string();
        if inputs.namespace_explicit && ns != "local" {
            // An explicit non-"local" namespace (CLI `--actor`/`--namespace`,
            // or `kkernel exec`/`reindex`'s forced-explicit `--namespace`)
            // fills `actor_id` directly from the namespace — unchanged
            // pre-existing ADR-057 fill behavior, kept keyed on
            // `namespace_explicit` (not `actor_explicit`) so exec/reindex
            // keep resolving a non-local `--namespace` to that actor.
            resolved.actor_id = Some(ns);
        } else if inputs.actor_explicit {
            // Genuinely explicit CLI actor tier requesting anonymous
            // (`--actor local` / `--namespace local`) is authoritative: do
            // not fall through to project/db/env actor tiers just because
            // "local" also looks like "unset". Gated on `actor_explicit`
            // (not the broader `namespace_explicit`) so `kkernel exec`/
            // `reindex` — which force `namespace_explicit: true` for
            // unrelated reasons and have no `--actor` flag — keep falling
            // through exactly as before.
            resolved.actor_id = None;
        } else {
            let project_actor = khive_runtime::resolve_project_actor_id(inputs.config)
                .map_err(|e| anyhow::anyhow!("config error: {e}"))?;
            resolved.actor_id = project_actor.or(resolved.actor_id);
        }
        resolved
    };

    // ADR-170: events-daemon split. Every file-backed resolution routes event
    // persistence to the events database beside the main store, in DIRECT
    // mode: this resolver serves one-shot hosts (`kkernel exec`, `reindex`,
    // ingest) and tests, which have no events daemon to talk to. Resident
    // daemon hosts upgrade the resolved config to socket forwarding
    // themselves — see [`enable_events_forwarding_for_daemon`] — because
    // only they supervise an events daemon at the derived socket.
    // In-memory resolutions (tests) keep the legacy main-store event plane.
    // `KHIVE_EVENTS_SPLIT=0` is the deployment kill-switch back to legacy.
    let resolved = {
        let mut resolved = resolved;
        let kill_switch = std::env::var("KHIVE_EVENTS_SPLIT").is_ok_and(|v| v.trim() == "0");
        if !kill_switch {
            if let Some(main_db) = resolved.db_path.as_deref() {
                resolved.events_split = Some(khive_runtime::events_split::EventsSplitConfig {
                    db_path: khive_runtime::events_split::events_db_path_beside(main_db),
                    socket_path: None,
                });
            }
        }
        resolved
    };

    // Tier-3 env fallback: KHIVE_BRAIN_PROFILE is applied AFTER CLI (tier-1) and
    // config-file (tier-2) so that a project or global TOML always wins over the env var.
    Ok((apply_env_brain_profile(resolved), db_anchor))
}

/// Upgrade a resolved config's event plane from direct (embedded) mode to
/// socket forwarding — the resident-daemon half of the ADR-170 split.
///
/// `resolve_runtime_config_with_db_anchor` always resolves the event plane in
/// direct mode because most of its callers (one-shot CLI, tests) have no
/// events daemon. The two daemon hosts (`build_server_with_explicit_namespace`
/// under `--daemon`, and `kkernel mcp`'s multi-backend arm) call this after
/// resolution; they are exactly the processes that also supervise an events
/// daemon at the derived socket (`start_daemon_components_if_daemon`), so the
/// socket this routes to is the one that same host keeps alive.
pub fn enable_events_forwarding_for_daemon(config: &mut RuntimeConfig) {
    if let Some(split) = config.events_split.as_mut() {
        split.socket_path = Some(khive_runtime::events_split::events_socket_path_beside(
            &split.db_path,
        ));
    }
}

/// Apply `KHIVE_BRAIN_PROFILE` env var as the tier-3 fallback for `brain_profile`.
///
/// Called after CLI (tier-1) and config-file (tier-2) have already been applied.
/// Only sets `brain_profile` when neither previous tier produced a value.
fn apply_env_brain_profile(mut cfg: RuntimeConfig) -> RuntimeConfig {
    if cfg.brain_profile.is_none() {
        cfg.brain_profile = std::env::var("KHIVE_BRAIN_PROFILE")
            .ok()
            .filter(|s| !s.trim().is_empty());
    }
    cfg
}

/// Resolve the server-level default output format (ADR-078 §2 precedence tier 2-3).
///
/// Precedence (highest to lowest — called AFTER CLI tier is handled at request time):
/// 1. `KHIVE_OUTPUT_FORMAT` env var (tier 2)
/// 2. `khive_cfg.runtime.default_output_format` from TOML (tier 3)
/// 3. Builtin `OutputFormat::Json` (tier 4)
///
/// Returns the resolved [`OutputFormat`] to wire into the server via
/// `with_default_output_format`.
pub fn apply_env_output_format(toml_default: Option<OutputFormat>) -> OutputFormat {
    // Env var (tier 2) overrides TOML (tier 3).
    if let Ok(val) = std::env::var("KHIVE_OUTPUT_FORMAT") {
        match val.trim() {
            "json" => return OutputFormat::Json,
            "auto" => return OutputFormat::Auto,
            "table" => return OutputFormat::Table,
            _ => {
                tracing::warn!(
                    value = %val,
                    "KHIVE_OUTPUT_FORMAT has unknown value; falling back to TOML / builtin default"
                );
            }
        }
    }
    // TOML default (tier 3) or builtin (tier 4).
    toml_default.unwrap_or(OutputFormat::Json)
}

/// Resolve the full config (embedding engines + namespace) from file or env.
///
/// Precedence for the storage namespace (highest to lowest):
/// 1. CLI `--actor` / `--namespace` (carried in `base.default_namespace`)
/// 2. Default "local" from RuntimeConfig
///
/// Config file `[actor] id` does NOT set `default_namespace` — writes stay
/// pinned to `local` (ADR-007 Rev 4 Rule 0). A non-`'local'` `actor.id` IS
/// folded into the default READ visible-set (Rule 3b), but `runtime_config_from_khive_config`
/// preserves `base.default_namespace` regardless of the configured actor.
///
/// Precedence for embedding engines:
/// 1. Config file `[[engines]]`
/// 2. Env vars `KHIVE_EMBEDDING_MODEL` + `KHIVE_ADDITIONAL_EMBEDDING_MODELS`
///
/// `db_path` is the already-resolved database path (or `None` for an in-memory
/// database); it anchors tier-3 project-local config discovery to the
/// database's own directory instead of the process cwd.
fn resolve_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    db_path: Option<&std::path::Path>,
    packs_overridden: bool,
    force_memory: bool,
) -> anyhow::Result<RuntimeConfig> {
    match gate_boot_disclosure::load(config_path, db_path)? {
        Some(khive_cfg) => {
            let base = apply_config_pack_selection(&khive_cfg, base, packs_overridden);
            let env_primary = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
            let env_additional = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS").ok();
            if !khive_cfg.engines.is_empty() && (env_primary.is_some() || env_additional.is_some())
            {
                tracing::warn!(
                    "khive config [[engines]] present; KHIVE_EMBEDDING_MODEL / \
                     KHIVE_ADDITIONAL_EMBEDDING_MODELS env vars are overridden"
                );
            }

            let mut resolved = runtime_config_from_khive_config(&khive_cfg, base);
            resolve_runtime_wal_ceiling(&mut resolved, &khive_cfg.backends, force_memory)?;
            Ok(resolved)
        }
        None => {
            let env_cfg = config_from_env();
            let mut resolved = if env_cfg.engines.is_empty() {
                base
            } else {
                runtime_config_from_khive_config(&env_cfg, base)
            };
            resolve_runtime_wal_ceiling(&mut resolved, &[], force_memory)?;
            Ok(resolved)
        }
    }
}

/// Validate the captured policy snapshot before a forwarding client computes its daemon
/// identity. Backend field overrides are resolved from this same snapshot at
/// the named-backend opener, so a later environment change cannot split the
/// client fingerprint from the opened pool.
fn resolve_runtime_wal_ceiling(
    config: &mut RuntimeConfig,
    backends: &[BackendConfig],
    force_memory: bool,
) -> anyhow::Result<()> {
    if backends.is_empty() && !force_memory {
        config.resolve_disk_guard_policy(false)?;
    }
    if force_memory {
        config.disk_guard_config = None;
        config.wal_ceiling_bytes = 0;
        config.wal_ceiling_configured_bytes = 0;
        config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
        return validate_wal_ceiling_topology(config, backends, true);
    }
    let env_raw = config.wal_ceiling_env_raw.clone();
    let needs_fallback = backends.is_empty()
        || backends.iter().any(|backend| {
            backend.kind == BackendKind::Sqlite && backend.wal_ceiling_bytes.is_none()
        });
    let fallback = if needs_fallback {
        khive_runtime::resolve_wal_ceiling(
            None,
            env_raw.as_deref(),
            BackendId::MAIN,
            if backends.is_empty() && config.db_path.is_none() {
                BackendKind::Memory
            } else {
                BackendKind::Sqlite
            },
            true,
            false,
        )?
    } else {
        khive_runtime::ResolvedWalCeiling {
            configured_bytes: 0,
            effective_bytes: 0,
            source: khive_runtime::WalCeilingSource::Default,
        }
    };
    config.wal_ceiling_env_raw = env_raw;
    config.wal_ceiling_configured_bytes = fallback.configured_bytes;
    config.wal_ceiling_bytes = fallback.effective_bytes;
    config.wal_ceiling_source = fallback.source;

    if backends.is_empty() {
        let kind = if config.db_path.is_some() {
            BackendKind::Sqlite
        } else {
            BackendKind::Memory
        };
        let main = khive_runtime::resolve_wal_ceiling(
            None,
            config.wal_ceiling_env_raw.as_deref(),
            BackendId::MAIN,
            kind,
            true,
            false,
        )?;
        config.wal_ceiling_bytes = main.effective_bytes;
    } else {
        validate_wal_ceiling_topology(config, backends, false)?;
    }
    Ok(())
}

/// Resolve configuration without enabling embedding engines (no-embed path).
///
/// `db_path` anchors tier-3 project-local config discovery to the database's
/// own directory instead of the process cwd (see [`resolve_config`]). The
/// caller-owned namespace remains in `base`, while non-actor sections such as
/// `[git_write]` are still loaded and validated.
fn resolve_actor_from_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    db_path: Option<&std::path::Path>,
    packs_overridden: bool,
    force_memory: bool,
) -> anyhow::Result<RuntimeConfig> {
    match gate_boot_disclosure::load(config_path, db_path)? {
        Some(khive_cfg) => {
            let base = apply_config_pack_selection(&khive_cfg, base, packs_overridden);
            let mut resolved = runtime_config_from_khive_config(&khive_cfg, base);
            resolve_runtime_wal_ceiling(&mut resolved, &khive_cfg.backends, force_memory)?;
            Ok(RuntimeConfig {
                embedding_model: None,
                additional_embedding_models: vec![],
                ..resolved
            })
        }
        None => {
            let mut resolved = base;
            resolve_runtime_wal_ceiling(&mut resolved, &[], force_memory)?;
            Ok(resolved)
        }
    }
}

fn apply_config_pack_selection(
    khive_cfg: &KhiveConfig,
    mut base: RuntimeConfig,
    packs_overridden: bool,
) -> RuntimeConfig {
    if !packs_overridden {
        if let Some(packs) = khive_cfg
            .runtime
            .packs
            .as_ref()
            .filter(|packs| !packs.is_empty())
        {
            base.packs.clone_from(packs);
        }
    }
    base
}

#[cfg(all(test, unix))]
#[path = "serve_targeted_alias_tests.rs"]
mod targeted_alias_tests;

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;

#[cfg(all(test, any(feature = "channel-email", feature = "channel-telegram")))]
#[path = "serve_poll_cancel_tests.rs"]
mod poll_cancellation_tests;

#[cfg(all(test, any(feature = "channel-email", feature = "test-channel-timing")))]
#[path = "serve_poll_timing_tests.rs"]
mod poll_timing_tests;

#[cfg(all(test, feature = "channel-email"))]
#[path = "serve_outbox_claim_tests.rs"]
mod outbox_claim_tests;

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[path = "serve_outbox.rs"]
mod outbox;

include!("serve_outbox_parity_tests.rs");

#[cfg(all(test, feature = "channel-email", feature = "channel-telegram"))]
#[path = "serve_outbox_slug_tests.rs"]
mod outbox_slug_tests;

#[cfg(all(test, unix))]
#[path = "serve_reader_pool_tests.rs"]
mod reader_pool_tests;

#[cfg(test)]
#[path = "serve_email_policy_tests.rs"]
mod email_policy_tests;

#[cfg(test)]
#[path = "serve_disk_guard_tests.rs"]
mod disk_guard_tests;
