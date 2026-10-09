//! Runtime startup, process-role admission and session sweep lifecycle.

use super::*;

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
    pub(super) fn new(
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
        tracing::warn!(target: "khive_mcp::serve",
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

pub(super) fn daemon_startup_report(
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

pub(super) fn start_host_background_tasks(
    args: &Args,
    server: &KhiveMcpServer,
    schedule_rt: Option<KhiveRuntime>,
) -> khive_runtime::daemon::DaemonStartupReport {
    let report = daemon_startup_report(args, server, schedule_rt.is_some());
    if args.daemon
        && args.daemon_options().lifetime == khive_runtime::daemon::DaemonLifetime::Demand
    {
        for component in &report.skipped_components {
            tracing::info!(target: "khive_mcp::serve", component, "demand daemon: background component skipped");
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
pub(super) fn is_daemon_role(args: &Args) -> bool {
    args.daemon
}

/// Combine process role with the fixed runtime-mode admission captured when
/// the server was built. A daemon flag alone never authorizes background
/// writes: each loop is admitted only when the runtime serving its verbs is
/// writable in the resolved single- or multi-backend topology.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) fn channel_loop_plan(
    server: &KhiveMcpServer,
    args: &Args,
) -> crate::server::ChannelLoopAdmission {
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
pub(super) fn log_inbound_refused_for_read_only_blob(
    channel_kind: &str,
    admission: crate::server::ChannelLoopAdmission,
) {
    if admission.inbound_blocked_by_read_only_blob {
        tracing::error!(target: "khive_mcp::serve",
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
pub(super) struct SessionSweepHandle {
    pub(super) shutdown_tx: tokio::sync::watch::Sender<()>,
    pub(super) join: tokio::task::JoinHandle<()>,
}

impl SessionSweepHandle {
    async fn shutdown(self) {
        drop(self.shutdown_tx);
        if tokio::time::timeout(std::time::Duration::from_secs(2), self.join)
            .await
            .is_err()
        {
            tracing::warn!(target: "khive_mcp::serve",
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
    tracing::info!(target: "khive_mcp::serve", "ADR-091 Amendment 2 Plank A: session WAL-registry sweep started");
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
pub(super) async fn serve_with_session_sweep(
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
pub(super) async fn serve_holding_sweep(
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
pub(super) fn spawn_email_channel_loops_if_daemon(server: &KhiveMcpServer, args: &Args) {
    let admission = channel_loop_plan(server, args);
    if !is_daemon_role(args) {
        tracing::info!(target: "khive_mcp::serve", "email channel loops: skipped (client role; daemon owns channel loops)");
        return;
    }
    log_inbound_refused_for_read_only_blob("email", admission);
    if !admission.inbound_poll && !admission.outbound_delivery {
        tracing::info!(target: "khive_mcp::serve",
            "email channel loops: skipped (assigned comm runtime does not admit writes)"
        );
        return;
    }
    tracing::info!(target: "khive_mcp::serve",
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
pub(super) fn start_daemon_components_if_daemon(
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
        tracing::info!(target: "khive_mcp::serve", "read-only deployment: events daemon supervision skipped");
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
pub(super) fn writable_schedule_runtime(runtime: Option<KhiveRuntime>) -> Option<KhiveRuntime> {
    runtime.filter(|runtime| !runtime.is_read_only())
}
