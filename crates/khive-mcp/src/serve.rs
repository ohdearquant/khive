//! Build the runtime + server from CLI args and serve over the selected transport.
//!
//! This is the bootstrap that the `kkernel mcp` subcommand drives. Logging is
//! initialized by the binary, not here.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use khive_runtime::{
    config_from_env, run_migrations, runtime_config_from_khive_config, BackendConfig, BackendId,
    BackendKind, ConnectionPool, KhiveConfig, KhiveRuntime, OutputFormat, RuntimeConfig,
    StorageBackend,
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
pub async fn run(args: Args, registry: &TransportRegistry) -> anyhow::Result<()> {
    if let Some(generation) = args.resumed_generation {
        tracing::warn!(
            generation,
            "bridge self-heal: this process is a resumed generation of an \
             in-place re-exec triggered by a stale daemon-protocol mismatch (#714)"
        );
    }
    // #667: in daemon mode, failing to acquire the boot guard must abort
    // before `build_server` runs migrations/FTS DDL unguarded — see
    // `acquire_daemon_boot_guard`. Non-daemon callers keep the best-effort
    // lock (dropped right after construction below).
    #[cfg(unix)]
    let boot_guard = if args.daemon {
        Some(khive_runtime::daemon::acquire_daemon_boot_guard()?)
    } else {
        khive_runtime::daemon::acquire_recovery_lock()
    };
    let (server, schedule_rt) = build_server(&args)?;

    spawn_schedule_tick_loop_if_daemon(&args, &server, schedule_rt);
    start_daemon_components_if_daemon(&args, &server);

    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon_with_boot_guard(server, boot_guard).await?;
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
    serve_with_session_sweep(server, &args, registry).await
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

/// Start ADR-119 daemon components in daemon role only. Non-daemon roles
/// must not start components and stay byte-identical in behavior and output
/// — the silent return keeps client runs unchanged. In daemon role the
/// registry itself always logs the enumerated roster (names + count),
/// including an empty one.
fn start_daemon_components_if_daemon(args: &Args, server: &KhiveMcpServer) {
    if !args.daemon {
        return;
    }
    crate::components::start_daemon_components(server);
}

/// Spawn the daemon-resident schedule-event tick loop (ADR-106) iff `args`
/// indicates this process is the daemon (mirrors the daemon-role gate
/// pattern used by the (now-extracted) channel loops, #602). `schedule_rt`
/// MUST be the daemon's own already-resolved `"schedule"`-pack runtime
/// (never a fresh `RuntimeConfig`, PR #782); `None` means either this isn't
/// the daemon role or the pack set has no `"schedule"`. `server` MUST be the
/// daemon's own live `KhiveMcpServer`, cloned for action-dispatch only — a
/// throwaway server built from `schedule_rt` alone would misroute replayed
/// actions in a multi-backend deployment. See
/// `crates/khive-mcp/docs/api/pending-events.md`.
fn spawn_schedule_tick_loop_if_daemon(
    args: &Args,
    server: &KhiveMcpServer,
    schedule_rt: Option<KhiveRuntime>,
) {
    if !args.daemon {
        tracing::info!("schedule tick loop: skipped (client role; daemon owns the tick)");
        return;
    }
    let Some(rt) = schedule_rt else {
        tracing::info!(
            "schedule tick loop: skipped (\"schedule\" pack is not in this daemon's \
             resolved pack set)"
        );
        return;
    };
    let interval = crate::pending_events::tick_interval_from_env();
    tracing::info!(
        interval_secs = interval.as_secs(),
        "schedule tick loop: spawning (daemon role)"
    );
    tokio::spawn(crate::pending_events::schedule_tick_loop(
        rt,
        server.clone(),
        interval,
    ));
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
/// (ADR-106) — see `spawn_schedule_tick_loop_if_daemon`. `kkernel`'s
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
             in-place re-exec triggered by a stale daemon-protocol mismatch (#714)"
        );
    }
    spawn_schedule_tick_loop_if_daemon(args, &server, schedule_rt);
    start_daemon_components_if_daemon(args, &server);

    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon_with_boot_guard(server, boot_guard).await?;
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
    serve_with_session_sweep(server, args, registry).await
}

/// Build the VerbRegistry and per-pack runtimes for a multi-backend deployment
/// (ADR-028 + ADR-029 Phase 2).
///
/// Returns a [`MultiBackendRegistry`] that `kkernel` uses to both:
/// 1. Construct the `KhiveMcpServer` (via `from_registry_with_meta`), and
/// 2. Build the `BackendRegistry` for the `SubstrateCoordinator`.
///
/// This is a refactor-extraction of the registry-building logic from
/// `build_server_multi_backend`, keeping the existing tests intact.
///
/// `cli_db_override` is the raw, pre-resolution `--db` / `KHIVE_DB` value (issue
/// #553). `[[backends]]` in `khive.toml` otherwise wins unconditionally, so an
/// operator's `--db :memory:` isolation request was silently discarded whenever
/// any backend was declared. `Some(":memory:")` forces every declared backend to
/// in-memory for this invocation (loudly logged); any other concrete path is
/// rejected rather than silently collapsing distinct declared backends onto one
/// caller-supplied file.
pub fn build_registry_for_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<MultiBackendRegistry> {
    khive_runtime::assert_db_anchor_consistent(base_config.db_path.as_deref(), cli_db_override)?;
    build_registry_for_multi_backend_inner(base_config, khive_cfg, cli_db_override)
}

pub fn build_registry_for_multi_backend_with_db_anchor(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> anyhow::Result<MultiBackendRegistry> {
    // Regression fence: `base_config.db_path` feeds `compute_config_id` below,
    // so it must agree with the canonical anchor for this same `--db` input.
    // This is the shared choke point both multi-backend boot paths funnel
    // through — `build_server_multi_backend` in this file and `kkernel`'s
    // `Command::Mcp` coordinator-attached branch — so the guard lives here
    // once instead of at each caller.
    khive_runtime::assert_captured_db_anchor_consistent(base_config.db_path.as_deref(), db_anchor)?;

    build_registry_for_multi_backend_inner(base_config, khive_cfg, cli_db_override)
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
/// override to apply.
pub fn validate_db_override_against_backends(
    cli_db_override: Option<&str>,
    backend_count: usize,
) -> anyhow::Result<bool> {
    match cli_db_override {
        Some(":memory:") => {
            tracing::warn!(
                "--db :memory: (or KHIVE_DB=:memory:) is overriding {backend_count} \
                 configured [[backends]] entries to in-memory storage for this invocation; \
                 khive.toml's declared backend paths will not be used this run"
            );
            Ok(true)
        }
        Some(other) => {
            anyhow::bail!(
                "--db {other:?} (or KHIVE_DB) cannot be combined with [[backends]]: \
                 {backend_count} backend(s) are already declared in khive.toml, so applying \
                 this override here is ambiguous (it could silently collapse distinct \
                 declared backends onto a single file). Edit khive.toml directly to change \
                 backend paths, or pass --db :memory: to force all backends in-memory for \
                 this invocation."
            );
        }
        None => Ok(false),
    }
}

fn build_registry_for_multi_backend_inner(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<MultiBackendRegistry> {
    let backend_count = khive_cfg.backends.len();
    let force_memory = validate_db_override_against_backends(cli_db_override, backend_count)?;

    // Open and migrate each declared backend, deduplicating SQLite backends by
    // canonical path (ADR-028 §8).
    let mut backends: HashMap<String, Arc<StorageBackend>> = HashMap::new();
    let mut path_to_backend: HashMap<std::path::PathBuf, Arc<StorageBackend>> = HashMap::new();
    for backend_cfg in &khive_cfg.backends {
        let owned_cfg = if force_memory {
            BackendConfig {
                kind: BackendKind::Memory,
                path: None,
                ..backend_cfg.clone()
            }
        } else {
            backend_cfg.clone()
        };
        let backend_cfg = &owned_cfg;
        let canonical = canonical_backend_path(backend_cfg)?;
        if let Some(ref canon) = canonical {
            if let Some(existing) = path_to_backend.get(canon) {
                backends.insert(backend_cfg.name.clone(), existing.clone());
                continue;
            }
        }
        let backend = open_backend(backend_cfg)?;
        {
            let mut writer = backend.pool().try_writer().map_err(|e| {
                anyhow::anyhow!("backend {}: migration writer: {e}", backend_cfg.name)
            })?;
            run_migrations(writer.conn_mut())
                .map_err(|e| anyhow::anyhow!("backend {}: migration: {e}", backend_cfg.name))?;
        }
        let arc = Arc::new(backend);
        if let Some(canon) = canonical {
            path_to_backend.insert(canon, arc.clone());
        }
        backends.insert(backend_cfg.name.clone(), arc);
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

    let pack_names = &base_config.packs;
    let mut per_pack_runtimes_local: HashMap<String, KhiveRuntime> = HashMap::new();
    for pack_name in pack_names {
        let (backend_name, backend) = match khive_cfg.packs.get(pack_name.as_str()) {
            None => (BackendId::MAIN, main_backend.clone()),
            Some(pack_cfg) => {
                let backend_name = pack_cfg.backend.as_str();
                let backend = backends.get(backend_name).cloned().ok_or_else(|| {
                    let defined = backends.keys().cloned().collect::<Vec<_>>().join(", ");
                    anyhow::anyhow!(
                        "[packs.{pack_name}].backend = {backend_name:?} references an unknown backend; defined backends: {defined}"
                    )
                })?;
                (backend_name, backend)
            }
        };
        let mut rt_config = base_config.clone();
        rt_config.backend_id = BackendId::new(backend_name);
        per_pack_runtimes_local.insert(
            pack_name.clone(),
            build_pack_runtime(backend, backend_name, rt_config, &main_backend),
        );
    }

    let default_runtime = KhiveRuntime::from_backend(main_backend.clone(), {
        let mut cfg = base_config.clone();
        cfg.backend_id = BackendId::main();
        cfg
    });

    // ADR-111 Amendment 2: resolve the config-selected `BlobStore` once
    // against the main backend and install it on every runtime handle this
    // boot produces (`default_runtime` plus each per-pack runtime), so a
    // pack that later reads `KhiveRuntime::blob_store()` sees the same
    // selection regardless of which backend its own KG data lives on.
    if let Some(store) =
        install_resolved_blob_store(&default_runtime, khive_cfg, main_backend.as_ref())?
    {
        for rt in per_pack_runtimes_local.values() {
            rt.install_blob_store(store.clone());
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
    let config_id = crate::server::compute_config_id(default_runtime.config(), Some(khive_cfg));
    let visible_namespaces = default_runtime.config().visible_namespaces.clone();

    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_gate(gate);
    builder.with_default_namespace(default_namespace.as_str());
    builder.with_visible_namespaces(visible_namespaces);
    builder.with_actor_id(default_runtime.config().actor_id.clone());

    if let Ok(tok) = default_runtime.authorize(khive_runtime::Namespace::local()) {
        if let Ok(event_store) = default_runtime.events(&tok) {
            builder.with_event_store(event_store);
        }
    }

    khive_runtime::PackRegistry::register_packs_with_runtimes(
        pack_names,
        &per_pack_runtimes_local,
        &default_runtime,
        &mut builder,
    )
    .map_err(|e| anyhow::anyhow!("pack registration: {e}"))?;

    let registry = builder
        .build()
        .map_err(|e| anyhow::anyhow!("registry build: {e}"))?;

    default_runtime.install_edge_rules(registry.all_edge_rules());
    for rt in per_pack_runtimes_local.values() {
        rt.install_edge_rules(registry.all_edge_rules());
    }
    registry.call_register_embedders(&default_runtime);
    registry.call_register_entity_type_validators(&default_runtime);
    // #750: install pack-owned note-mutation hooks (currently
    // only khive-pack-memory's warm-ANN-cache invalidation) so KG's
    // update/delete verbs notify caching packs even though there is no
    // crate-level dependency between them.
    registry.call_register_note_mutation_hooks(&default_runtime);

    let backend_for_pack: HashMap<&str, &StorageBackend> = per_pack_runtimes_local
        .iter()
        .map(|(name, rt)| (name.as_str(), rt.backend()))
        .collect();
    let main_ref: &StorageBackend = main_backend.as_ref();
    registry
        .apply_schema_plans_with_map(&backend_for_pack, main_ref)
        .map_err(|e| anyhow::anyhow!("pack schema boot failure: {e}"))?;

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

/// Build a fully-configured server from parsed args (without serving).
///
/// Returns, alongside the server, the resolved [`KhiveRuntime`] handle the
/// `"schedule"` pack is bound to — `None` when the resolved pack set does
/// not include `"schedule"` — for `spawn_schedule_tick_loop_if_daemon` to
/// drain against (ADR-106). This is the SAME runtime the server itself
/// dispatches through, never an independently re-resolved one (PR #782 —
/// see `crates/khive-mcp/docs/api/pending-events.md`).
///
/// Thin wrapper over [`build_server_with_explicit_namespace`]: derives the
/// `(namespace, namespace_explicit)` pair from a real CLI parse and, because
/// this is the genuine `--actor`/`--namespace` CLI flag path, also treats
/// that explicitness as a real actor override.
pub fn build_server(args: &Args) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
    let (cli_namespace_explicit, cli_namespace) =
        resolve_cli_namespace(args).map_err(|e| anyhow::anyhow!("{e}"))?;
    build_server_with_explicit_namespace(
        args,
        cli_namespace,
        cli_namespace_explicit,
        cli_namespace_explicit,
    )
}

/// Build a fully-configured server from parsed args plus an independently
/// resolved `(namespace, namespace_explicit, actor_explicit)` triple.
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
pub fn build_server_with_explicit_namespace(
    args: &Args,
    namespace: khive_runtime::Namespace,
    namespace_explicit: bool,
    actor_explicit: bool,
) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
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
    let khive_cfg =
        KhiveConfig::load_with_home_fallback(args.config.as_deref(), db_path_for_config.as_deref())
            .map_err(|e| anyhow::anyhow!("config error: {e}"))?
            .unwrap_or_default();

    if khive_cfg.backends.is_empty() {
        // Single-backend path — identical to pre-ADR-028 behavior.
        let runtime = KhiveRuntime::new(config)?;
        install_resolved_blob_store(&runtime, &khive_cfg, runtime.backend())?;
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
        let schedule_rt = runtime
            .config()
            .packs
            .iter()
            .any(|p| p == "schedule")
            .then(|| runtime.clone());
        let fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
        let server = KhiveMcpServer::new(runtime)
            .map(|s| s.with_default_output_format(fmt))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok((server, schedule_rt));
    }

    // Multi-backend path (ADR-028).
    let multi = build_registry_for_multi_backend_with_db_anchor(
        config,
        &khive_cfg,
        args.db.as_deref(),
        db_anchor.as_deref(),
    )?;
    let schedule_rt = multi
        .per_pack_runtimes
        .get("schedule")
        .map(|rt| (**rt).clone());
    let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);
    Ok((server, schedule_rt))
}

/// Canonicalize a SQLite backend path for deduplication (ADR-028 §8).
///
/// The database file may not exist yet at boot time, so we cannot call
/// `std::fs::canonicalize` on the file itself. Instead we canonicalize the
/// parent directory (which must exist after `open_backend` creates it) and
/// rejoin the file name. `None` is returned for in-memory backends, which
/// are never deduplicated.
fn canonical_backend_path(cfg: &BackendConfig) -> anyhow::Result<Option<PathBuf>> {
    if cfg.kind == BackendKind::Memory {
        return Ok(None);
    }
    let path = match cfg.path.as_ref() {
        Some(p) => expand_tilde(p),
        None => return Ok(None),
    };
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("backend {}: path has no parent directory", cfg.name))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("backend {}: path has no file name", cfg.name))?;
    // Create the parent so canonicalize succeeds even before the DB file is written.
    std::fs::create_dir_all(parent).map_err(|e| {
        anyhow::anyhow!(
            "backend {}: cannot create parent dir {}: {e}",
            cfg.name,
            parent.display()
        )
    })?;
    let canon_parent = parent.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "backend {}: cannot canonicalize parent dir {}: {e}",
            cfg.name,
            parent.display()
        )
    })?;
    Ok(Some(canon_parent.join(file_name)))
}

/// Build a fully-wired multi-backend `KhiveMcpServer` (ADR-028).
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
pub fn build_server_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<KhiveMcpServer> {
    khive_runtime::assert_db_anchor_consistent(base_config.db_path.as_deref(), cli_db_override)?;
    let multi = build_registry_for_multi_backend_inner(base_config, khive_cfg, cli_db_override)?;
    Ok(build_server_from_multi_backend_registry(
        multi, khive_cfg, None,
    ))
}

pub fn build_server_multi_backend_with_db_anchor(
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
    )?;
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

    let server = KhiveMcpServer::from_registry_with_meta(
        multi.registry,
        &multi.default_namespace,
        &multi.config_id,
    )
    .with_default_output_format(fmt)
    .with_secondary_pools(secondary_pools);

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
        if !backend.is_file_backed() {
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
}

impl WiringSurface {
    /// Capture the wiring surface of an already-built server.
    pub fn capture(server: &KhiveMcpServer) -> Self {
        Self {
            has_checkpoint_pool: server.pool().is_some(),
            output_format: server.default_output_format(),
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
    if main_backend.is_file_backed() {
        Some(main_backend.pool_arc())
    } else {
        None
    }
}

/// Resolve `khive.toml`'s `[storage.blob]` selection against `backend` and
/// install it on `rt` (ADR-111 Amendment 2's boot-wiring requirement).
///
/// Returns the resolved store on success so multi-backend callers can also
/// install it on every per-pack runtime without re-resolving it.
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
/// `pub` so `kkernel`'s `exec` local-dispatch fallback server (the
/// single-backend branch of `build_local_fallback_server`) can install a
/// `BlobStore` the same way the `serve` boot path does, instead of leaving
/// `exec`'s in-process runtime without one (khive#1209).
pub fn install_resolved_blob_store(
    rt: &KhiveRuntime,
    khive_cfg: &KhiveConfig,
    backend: &StorageBackend,
) -> anyhow::Result<Option<Arc<dyn khive_storage::BlobStore>>> {
    match khive_runtime::resolve_blob_store(khive_cfg, backend) {
        Ok(store) => {
            rt.install_blob_store(store.clone());
            Ok(Some(store))
        }
        Err(e) if khive_cfg.storage.blob.is_none() => {
            tracing::debug!(
                error = %e,
                "no usable BlobStore for this backend and no [storage.blob] configured; \
                 leaving KhiveRuntime::blob_store() unset"
            );
            Ok(None)
        }
        Err(e) => Err(anyhow::anyhow!("[storage.blob] configuration error: {e}")),
    }
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
) -> KhiveRuntime {
    let rt = KhiveRuntime::from_backend(backend, rt_config);
    if backend_name != BackendId::MAIN {
        rt.with_core_backend(main_backend.clone())
    } else {
        rt
    }
}

/// Open a `StorageBackend` from a `BackendConfig`.
fn open_backend(cfg: &BackendConfig) -> anyhow::Result<StorageBackend> {
    match cfg.kind {
        BackendKind::Memory => StorageBackend::memory()
            .map_err(|e| anyhow::anyhow!("backend {}: memory open: {e}", cfg.name)),
        BackendKind::Sqlite => {
            let path = cfg.path.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "backend {}: sqlite backend requires a `path` field",
                    cfg.name
                )
            })?;
            let expanded = expand_tilde(path);
            if let Some(parent) = expanded.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!(
                        "backend {}: cannot create parent dir {}: {e}",
                        cfg.name,
                        parent.display()
                    )
                })?;
            }
            if cfg.read_only {
                StorageBackend::sqlite_read_only(&expanded).map_err(|e| {
                    anyhow::anyhow!("backend {}: sqlite read-only open: {e}", cfg.name)
                })
            } else {
                StorageBackend::sqlite(&expanded)
                    .map_err(|e| anyhow::anyhow!("backend {}: sqlite open: {e}", cfg.name))
            }
        }
    }
}

/// Expand a leading `~` to `$HOME` in a path.
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(format!("{home}/{rest}"))
    } else if s == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
    } else {
        path.to_path_buf()
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
    /// Packs to register. `None` falls back to `RuntimeConfig::default().packs`.
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

    let packs = inputs
        .packs
        .unwrap_or_else(|| RuntimeConfig::default().packs);

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
        resolve_actor_from_config(inputs.config, no_embed_base, db_path_for_config.as_deref())?
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
        resolve_config(inputs.config, base_config, db_path_for_config.as_deref())?
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

    // Tier-3 env fallback: KHIVE_BRAIN_PROFILE is applied AFTER CLI (tier-1) and
    // config-file (tier-2) so that a project or global TOML always wins over the env var.
    Ok((apply_env_brain_profile(resolved), db_anchor))
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
) -> anyhow::Result<RuntimeConfig> {
    match KhiveConfig::load_with_home_fallback(config_path, db_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            let env_primary = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
            let env_additional = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS").ok();
            if !khive_cfg.engines.is_empty() && (env_primary.is_some() || env_additional.is_some())
            {
                tracing::warn!(
                    "khive config [[engines]] present; KHIVE_EMBEDDING_MODEL / \
                     KHIVE_ADDITIONAL_EMBEDDING_MODELS env vars are overridden"
                );
            }

            Ok(runtime_config_from_khive_config(&khive_cfg, base))
        }
        None => {
            let env_cfg = config_from_env();
            if env_cfg.engines.is_empty() {
                Ok(base)
            } else {
                Ok(runtime_config_from_khive_config(&env_cfg, base))
            }
        }
    }
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
) -> anyhow::Result<RuntimeConfig> {
    match KhiveConfig::load_with_home_fallback(config_path, db_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            let resolved = runtime_config_from_khive_config(&khive_cfg, base);
            Ok(RuntimeConfig {
                embedding_model: None,
                additional_embedding_models: vec![],
                ..resolved
            })
        }
        None => Ok(base),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{BlobConfig, Namespace, StorageSectionConfig};
    use serial_test::serial;
    use std::io::Write;

    // Force-link khive-pack-template (a dev-dependency only) so its
    // `inventory::submit!` registration is visible to this test binary's
    // `PackRegistry` — mirrors the same force-link in tests/integration.rs.
    #[allow(unused_imports)]
    use khive_pack_template::TemplatePack as _TemplatePack;

    // #689: `config_discovery_db_anchor` is a pure function (no env/cwd
    // dependency), so its explicit-vs-unset contract is covered here without
    // the env-mutation isolation the cwd/HOME-dependent tests below require.
    #[test]
    fn config_discovery_db_anchor_unset_is_none() {
        assert_eq!(
            config_discovery_db_anchor(None),
            None,
            "unset --db must not anchor discovery on the materialized home default"
        );
    }

    #[test]
    fn config_discovery_db_anchor_explicit_matches_resolve_db_anchor() {
        assert_eq!(
            config_discovery_db_anchor(Some("/tmp/explicit.db")),
            khive_runtime::resolve_db_anchor(Some("/tmp/explicit.db")),
            "an explicit --db must anchor discovery identically to resolve_db_anchor"
        );
    }

    #[test]
    fn config_discovery_db_anchor_memory_sentinel_is_none() {
        assert_eq!(config_discovery_db_anchor(Some(":memory:")), None);
    }

    fn write_config(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("khive.toml");
        let mut f = std::fs::File::create(&path).expect("create config file");
        f.write_all(body.as_bytes()).expect("write config");
        path
    }

    fn kg_test_packs() -> Vec<String> {
        vec!["kg".to_string()]
    }

    // The resolver MUST honor config-file `[[engines]]` over RuntimeConfig
    // defaults — otherwise `kkernel reindex` embeds for the wrong model set
    // versus what `kkernel mcp` serves recall from. Regression for PR #8
    // blocker.
    #[test]
    #[serial]
    fn resolver_uses_config_file_engines_over_defaults() {
        // Ensure a stale ambient value cannot leak into either branch.
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        // The shipped default is single-engine, so leaving the additional list
        // unset would make "the config file overrode the default" and "there
        // was nothing to override" produce the same empty result, and the
        // final assertion below would stop discriminating. Declare one
        // deliberately so the override remains observable.
        std::env::set_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS", "paraphrase");

        let default_cfg = RuntimeConfig::default();
        let default_primary = format!("{:?}", default_cfg.embedding_model);
        assert!(
            !default_cfg.additional_embedding_models.is_empty(),
            "precondition: default config must carry an additional engine for this test to discriminate"
        );

        let dir = tempfile::tempdir().expect("temp dir");
        // A single non-default engine that differs from the default primary.
        let path = write_config(
            dir.path(),
            r#"
[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        let resolved_primary = format!("{:?}", resolved.embedding_model);
        assert_ne!(
            resolved_primary, default_primary,
            "resolved primary engine must come from the config file, not the default"
        );
        assert!(
            resolved.embedding_model.is_some(),
            "config-file engine must resolve to a primary embedding model"
        );
        assert!(
            resolved.additional_embedding_models.is_empty(),
            "config file declares one engine; additional list must be empty (not the default's)"
        );
        assert_eq!(resolved.db_path, None, ":memory: must map to in-memory db");

        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    }

    /// Regression for #379: when the loaded config file has NO `[[engines]]`
    /// block, `KHIVE_EMBEDDING_MODEL` is genuinely used as the fallback — it
    /// must resolve into `RuntimeConfig::embedding_model`, not be discarded.
    /// The startup warning must not fire in this case either (the env pair is
    /// applied, not overridden) — see the `resolve_config` fix.
    #[test]
    #[serial]
    fn resolver_falls_back_to_env_when_config_has_no_engines() {
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::set_var("KHIVE_EMBEDDING_MODEL", "bge-small-en-v1.5");

        let dir = tempfile::tempdir().expect("temp dir");
        // Config file present, but with no [[engines]] block at all.
        let path = write_config(
            dir.path(),
            r#"
[runtime]
brain_profile = "unrelated"
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        std::env::remove_var("KHIVE_EMBEDDING_MODEL");

        assert_eq!(
            format!("{:?}", resolved.embedding_model),
            "Some(BgeSmallEnV15)",
            "KHIVE_EMBEDDING_MODEL must be applied as the fallback when the \
             config file has no [[engines]] block, not treated as ignored"
        );
    }

    /// Regression for PR #52: project-toml brain_profile
    /// MUST win over KHIVE_BRAIN_PROFILE env var.
    ///
    /// Merged config precedence: CLI > project toml > global toml > env > default.
    /// Before the fix, the env var was bound into the clap `brain_profile` arg and
    /// placed at tier-1 via RuntimeConfig::default() in the base_config spread,
    /// causing env to override TOML.
    #[test]
    #[serial]
    fn brain_profile_config_beats_env() {
        std::env::set_var("KHIVE_BRAIN_PROFILE", "env-profile");

        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(
            dir.path(),
            r#"
[runtime]
brain_profile = "project-profile"
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
            brain_profile: None, // no explicit CLI flag
        })
        .expect("resolve config");

        std::env::remove_var("KHIVE_BRAIN_PROFILE");

        assert_eq!(
            resolved.brain_profile.as_deref(),
            Some("project-profile"),
            "project TOML brain_profile must win over KHIVE_BRAIN_PROFILE env var"
        );
    }

    /// Env var is used when no CLI flag and no TOML value are present.
    #[test]
    #[serial]
    fn brain_profile_env_fallback_when_no_toml() {
        std::env::set_var("KHIVE_BRAIN_PROFILE", "env-profile");

        let dir = tempfile::tempdir().expect("temp dir");
        // Config file without [runtime] brain_profile.
        let path = write_config(
            dir.path(),
            r#"
[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        std::env::remove_var("KHIVE_BRAIN_PROFILE");

        assert_eq!(
            resolved.brain_profile.as_deref(),
            Some("env-profile"),
            "env var must be used when no CLI flag and no TOML brain_profile is set"
        );
    }

    /// CLI flag wins over both TOML and env var.
    #[test]
    #[serial]
    fn brain_profile_cli_wins_over_all() {
        std::env::set_var("KHIVE_BRAIN_PROFILE", "env-profile");

        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(
            dir.path(),
            r#"
[runtime]
brain_profile = "project-profile"
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
            brain_profile: Some("cli-profile".to_string()), // explicit CLI
        })
        .expect("resolve config");

        std::env::remove_var("KHIVE_BRAIN_PROFILE");

        assert_eq!(
            resolved.brain_profile.as_deref(),
            Some("cli-profile"),
            "CLI --brain-profile must win over both TOML and KHIVE_BRAIN_PROFILE env var"
        );
    }

    /// Regression for #203: the `--actor` / `--namespace`
    /// CLI flag must set `actor_id`, not just `default_namespace`. Before the fix,
    /// `--actor lambda:x` with no `KHIVE_ACTOR` env and no config-file `[actor] id`
    /// left actor_id None → anonymous token → degraded ADR-057 comm + false warning.
    #[test]
    #[serial]
    fn cli_actor_flag_populates_actor_id() {
        std::env::remove_var("KHIVE_ACTOR");

        // ADR-096 Fork 2: an explicit nonexistent config path (rather than `None`)
        // keeps this test hermetic against whatever the real `$HOME/.khive/config.toml`
        // on the machine running the suite happens to contain — the project-actor
        // tier (`resolve_project_actor_id`) now runs unconditionally and would
        // otherwise pick up a real machine's global `[actor]`, if one is set.
        let missing_config =
            std::path::PathBuf::from("/nonexistent/khive-cli-actor-test/config.toml");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: Namespace::parse("lambda:agent-x").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:agent-x"),
            "--actor flag must populate actor_id (flag==env parity), not just default_namespace"
        );
        assert_eq!(
            resolved.default_namespace.as_str(),
            "lambda:agent-x",
            "the flag still sets the write namespace"
        );
    }

    #[test]
    #[serial]
    fn no_embed_explicit_actor_preserves_git_write_config() {
        std::env::remove_var("KHIVE_ACTOR");
        let repo = tempfile::tempdir().expect("repo tempdir");
        std::fs::create_dir(repo.path().join(".git")).expect("create .git");
        let dir = tempfile::tempdir().expect("config tempdir");
        let path = write_config(
            dir.path(),
            &format!(
                "[[git_write.allowed]]\nrepo = {:?}\nbranches = [\"feat/*\"]\n",
                repo.path().display().to_string()
            ),
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("lambda:cli-actor").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve no-embed config");

        assert_eq!(resolved.default_namespace.as_str(), "lambda:cli-actor");
        assert_eq!(resolved.actor_id.as_deref(), Some("lambda:cli-actor"));
        assert_eq!(resolved.git_write.allowed.len(), 1);
        assert_eq!(
            resolved.git_write.allowed[0].repo,
            repo.path().display().to_string()
        );
        assert_eq!(resolved.git_write.allowed[0].branches, vec!["feat/*"]);
    }

    /// The `"local"` default namespace must stay anonymous (actor_id None) even when
    /// passed explicitly, so `should_warn_unattributed` still flags an unset actor.
    #[test]
    #[serial]
    fn cli_actor_flag_local_stays_anonymous() {
        std::env::remove_var("KHIVE_ACTOR");

        // See the hermeticity note in `cli_actor_flag_populates_actor_id` above.
        let missing_config =
            std::path::PathBuf::from("/nonexistent/khive-cli-actor-local-test/config.toml");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id, None,
            "explicit --actor local must remain anonymous (no actor_id) so the \
             unattributed-comm warning still fires"
        );
    }

    // --- ADR-096 Fork 2: project/cwd-anchored actor restore ---
    //
    // These tests exercise the REAL config-discovery path (`std::env::current_dir`
    // / `HOME`), which #651 anchored to the resolved database's own directory for
    // `config_id` purposes. Because process cwd and `HOME` are global process
    // state, each test below temporarily redirects both via `SeatEnv` (a small
    // RAII guard) and is marked `#[serial]` so it never races another `#[serial]`
    // test in this file. No other test in this module reads `config: None`
    // (everything else pins an explicit path or a nonexistent one), so these are
    // the only tests in this binary that legitimately depend on process cwd/HOME.

    /// RAII guard: temporarily redirects process cwd to `project_root` and `HOME`
    /// to an isolated, empty tempdir (so tier 4 — `~/.khive/config.toml` — never
    /// reaches whatever the real machine running this suite happens to have
    /// configured globally). Restores both on drop, even on panic/unwind.
    struct SeatEnv {
        original_cwd: PathBuf,
        original_home: Option<std::ffi::OsString>,
        _isolated_home: tempfile::TempDir,
    }

    impl SeatEnv {
        fn enter(project_root: &std::path::Path) -> Self {
            let original_cwd = std::env::current_dir().expect("read cwd");
            let original_home = std::env::var_os("HOME");
            let isolated_home = tempfile::tempdir().expect("isolated HOME tempdir");
            std::env::set_current_dir(project_root).expect("chdir into seat project root");
            std::env::set_var("HOME", isolated_home.path());
            Self {
                original_cwd,
                original_home,
                _isolated_home: isolated_home,
            }
        }
    }

    impl Drop for SeatEnv {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original_cwd);
            match &self.original_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Unit-level proof that `resolve_project_actor_id` reads the cwd-anchored
    /// project config — the pre-#651 tier-3 location — independently of any
    /// database directory. This is the primitive Fork 2 restores.
    #[test]
    #[serial]
    fn resolve_project_actor_id_reads_cwd_anchored_project_config() {
        std::env::remove_var("KHIVE_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:seat-actor\"\n",
        )
        .expect("write seat config");

        let _seat_env = SeatEnv::enter(seat_dir.path());

        assert_eq!(
            khive_runtime::resolve_project_actor_id(None).expect("no config error"),
            Some("lambda:seat-actor".to_string()),
            "resolve_project_actor_id must read the cwd-anchored .khive/config.toml \
             regardless of any database directory"
        );
    }

    /// ADR-096 Fork 2 pinning regression test — the exact regression class that
    /// broke the fleet: a seat-shaped connection whose cwd carries its own
    /// `.khive/config.toml` with an `[actor] id`, while the resolved database (and
    /// its own db-anchored config directory) lives ELSEWHERE and carries no
    /// `[actor]` at all — exactly how daemon-multiplexed seats run in production
    /// (every seat's own project dir vs. one shared home database).
    ///
    /// Exercises the REAL discovery path end-to-end through `resolve_runtime_config`
    /// (not a synthetic roots-based helper), so a future change to config discovery
    /// that re-collapses this fails THIS test loudly instead of silently reducing
    /// every seat's attribution to `"local"` / anonymous.
    #[test]
    #[serial]
    fn seat_shaped_project_actor_resolves_through_full_tier_chain() {
        std::env::remove_var("KHIVE_ACTOR");

        // The seat: a project directory with its own `[actor] id`.
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:seat-actor\"\n",
        )
        .expect("write seat config");

        // The shared database: a DIFFERENT directory, with no config.toml at its
        // own db-anchored location (the shared-home-database fleet case).
        let db_dir = tempfile::tempdir().expect("db tempdir");
        let khive_dir = db_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir db .khive");
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").expect("touch db file");
        let db_str = db_path.to_str().expect("utf8 path").to_string();

        let _seat_env = SeatEnv::enter(seat_dir.path());

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_str),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve seat-shaped config");

        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:seat-actor"),
            "a seat-shaped cwd with its own [actor] must resolve that actor through \
             the full discovery path even when the shared db-anchored config \
             location carries none — got {:?}",
            resolved.actor_id
        );
        assert_ne!(
            resolved.actor_id.as_deref(),
            Some("local"),
            "must not collapse to the literal namespace string"
        );
    }

    /// #689 regression: an unset `--db`/`KHIVE_DB` must anchor tier-3
    /// `.khive/config.toml` discovery on the process cwd, not on
    /// `resolve_db_anchor(None)`'s materialized `$HOME/.khive/khive.db`
    /// default. Before the fix, `db_path_for_config` was cloned straight from
    /// `base_config.db_path`, so an unset db collapsed tier 3 onto
    /// `$HOME/.khive/config.toml` and silently ignored a real project-local
    /// config with no error of any kind.
    ///
    /// Uses `[runtime].brain_profile` — read from the db-anchored config load
    /// (`resolve_config`/`runtime_config_from_khive_config`), unlike `[actor]`
    /// which is resolved through a separate, always-cwd-anchored tier (see
    /// `seat_shaped_project_actor_resolves_through_full_tier_chain` above) and
    /// so cannot observe this bug on its own.
    #[test]
    #[serial]
    fn resolve_runtime_config_unset_db_discovers_cwd_config_over_home() {
        std::env::remove_var("KHIVE_ACTOR");

        let project_dir = tempfile::tempdir().expect("project tempdir");
        std::fs::create_dir_all(project_dir.path().join(".khive")).expect("mkdir project .khive");
        std::fs::write(
            project_dir.path().join(".khive/config.toml"),
            "[runtime]\nbrain_profile = \"cwd-profile\"\n",
        )
        .expect("write project config");

        let seat_env = SeatEnv::enter(project_dir.path());

        // A conflicting $HOME/.khive/config.toml — must NOT win when --db is unset.
        std::fs::create_dir_all(seat_env._isolated_home.path().join(".khive"))
            .expect("mkdir home .khive");
        std::fs::write(
            seat_env._isolated_home.path().join(".khive/config.toml"),
            "[runtime]\nbrain_profile = \"home-profile\"\n",
        )
        .expect("write home config");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: None,
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve unset-db config");

        assert_eq!(
            resolved.brain_profile.as_deref(),
            Some("cwd-profile"),
            "unset --db must resolve tier-3 discovery against the project cwd, \
             not $HOME/.khive/khive.db's directory — got {:?}",
            resolved.brain_profile
        );
    }

    /// CLI `--actor` (tier 1) must win over a discovered project-config `[actor]`
    /// (tier 2), per the ratified full precedence chain (ADR-096 Fork 2:
    /// CLI > project-config > KHIVE_ACTOR env > anonymous).
    #[test]
    #[serial]
    fn cli_actor_flag_wins_over_project_config_actor() {
        std::env::remove_var("KHIVE_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:project-actor\"\n",
        )
        .expect("write seat config");

        let _seat_env = SeatEnv::enter(seat_dir.path());

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace: Namespace::parse("lambda:cli-actor").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:cli-actor"),
            "an explicit --actor flag must win over a discovered project-config actor"
        );
    }

    /// Project-config `[actor] id` (tier 2) must win over `KHIVE_ACTOR` env
    /// (tier 3) when both are present, and env must still be used as a fallback
    /// when no project config exists — the precedence this ADR restores.
    #[test]
    #[serial]
    fn project_actor_config_beats_khive_actor_env_which_falls_back_to_anonymous() {
        std::env::remove_var("KHIVE_ACTOR");

        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(
            dir.path(),
            r#"
[actor]
id = "lambda:project-actor"
"#,
        );

        std::env::set_var("KHIVE_ACTOR", "lambda:env-actor");

        let with_project_config = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config with project actor");

        let missing_config =
            std::path::PathBuf::from("/nonexistent/khive-project-vs-env-test/config.toml");
        let without_project_config = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config without project actor");

        std::env::remove_var("KHIVE_ACTOR");

        assert_eq!(
            with_project_config.actor_id.as_deref(),
            Some("lambda:project-actor"),
            "a project-config [actor] id must win over KHIVE_ACTOR env"
        );
        assert_eq!(
            without_project_config.actor_id.as_deref(),
            Some("lambda:env-actor"),
            "KHIVE_ACTOR env must still be used when no project config actor exists"
        );
    }

    /// PR #657: drives the REAL `clap` parse of `Args`
    /// (not a hand-built `RuntimeConfigInputs`) to prove a bare shell-level
    /// `KHIVE_ACTOR` no longer occupies the tier-1 CLI slot. Before the fix,
    /// `args.rs` bound `--actor` to `env = "KHIVE_ACTOR"`, so this env var
    /// alone made `resolve_cli_namespace` report `explicit = true` and
    /// therefore beat the project-config tier — inverting the ratified
    /// chain (CLI flag > project config > `KHIVE_ACTOR` env > anonymous).
    #[test]
    #[serial]
    fn real_clap_path_khive_actor_env_no_longer_wins_over_project_config() {
        use clap::Parser;
        std::env::remove_var("KHIVE_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:project-actor\"\n",
        )
        .expect("write seat config");

        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::set_var("KHIVE_ACTOR", "lambda:env-actor");

        // The real arg vector `kkernel mcp` parses — no `--actor` flag, so a
        // pre-fix `env = "KHIVE_ACTOR"` binding would populate `args.actor`.
        let args = Args::try_parse_from(["mcp"]).expect("parse real mcp args");
        let (namespace_explicit, namespace) =
            resolve_cli_namespace(&args).expect("resolve cli namespace");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace,
            namespace_explicit,
            actor_explicit: namespace_explicit,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        });

        std::env::remove_var("KHIVE_ACTOR");
        let resolved = resolved.expect("resolve config");

        assert!(
            !namespace_explicit,
            "KHIVE_ACTOR env alone must NOT make the CLI namespace tier explicit"
        );
        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:project-actor"),
            "project-config [actor] id must win over KHIVE_ACTOR env on the real clap path"
        );
        assert_eq!(
            resolved.default_namespace.as_str(),
            "local",
            "KHIVE_ACTOR env must never set default_namespace, only actor_id"
        );
    }

    /// PR #657, second case: with no project config and
    /// no `--actor` flag, `KHIVE_ACTOR` must still land as the tier-3
    /// `actor_id` fallback (it is read directly by `RuntimeConfig::default()`,
    /// independent of the removed clap `env` binding) — and must still leave
    /// `default_namespace` at `"local"`.
    #[test]
    #[serial]
    fn real_clap_path_khive_actor_env_falls_back_to_tier3_actor_id() {
        use clap::Parser;
        std::env::remove_var("KHIVE_ACTOR");

        // No project config anywhere on the discovery path: an isolated,
        // empty seat dir + isolated HOME (SeatEnv), so tier 2 and tier 4
        // (~/.khive/config.toml) both come up empty.
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::set_var("KHIVE_ACTOR", "lambda:env-only-actor");

        let args = Args::try_parse_from(["mcp"]).expect("parse real mcp args");
        let (namespace_explicit, namespace) =
            resolve_cli_namespace(&args).expect("resolve cli namespace");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace,
            namespace_explicit,
            actor_explicit: namespace_explicit,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        });

        std::env::remove_var("KHIVE_ACTOR");
        let resolved = resolved.expect("resolve config");

        assert!(
            !namespace_explicit,
            "KHIVE_ACTOR env alone must NOT make the CLI namespace tier explicit"
        );
        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:env-only-actor"),
            "KHIVE_ACTOR env must still land as the tier-3 actor_id fallback \
             when no project config exists"
        );
        assert_eq!(
            resolved.default_namespace.as_str(),
            "local",
            "KHIVE_ACTOR env must never set default_namespace, only actor_id"
        );
    }

    /// PR #657: an explicit `--actor local` (an operator
    /// request for the anonymous identity) must suppress BOTH the project-config
    /// and the db-anchored-config actor tiers, not just the missing-flag default.
    /// Before the fix, `resolve_runtime_config`'s tier-3 fold used
    /// `cli_actor.or(project_actor).or(resolved.actor_id)` unconditionally, so an
    /// explicit `local` (which maps to `cli_actor = None`) still fell through to
    /// whatever project or db-anchored `[actor]` happened to be discovered.
    #[test]
    #[serial]
    fn explicit_actor_local_suppresses_project_and_db_actor_tiers() {
        std::env::remove_var("KHIVE_ACTOR");

        // The seat: a project directory with its own `[actor] id`.
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:seat-actor\"\n",
        )
        .expect("write seat config");

        // A DIFFERENT db-anchored directory that ALSO carries its own `[actor]`
        // (the db-anchored config load in `resolve_config` applies this
        // unconditionally, regardless of the CLI explicit flag).
        let db_dir = tempfile::tempdir().expect("db tempdir");
        let khive_dir = db_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir db .khive");
        std::fs::write(
            khive_dir.join("config.toml"),
            "[actor]\nid = \"lambda:db-actor\"\n",
        )
        .expect("write db-anchored config");
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").expect("touch db file");
        let db_str = db_path.to_str().expect("utf8 path").to_string();

        let _seat_env = SeatEnv::enter(seat_dir.path());

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_str),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: false,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id, None,
            "explicit --actor local must resolve to anonymous even when both a \
             project-config and a db-anchored config declare an [actor] id — got {:?}",
            resolved.actor_id
        );
        assert_eq!(
            resolved.default_namespace.as_str(),
            "local",
            "explicit --actor local must keep default_namespace local"
        );
    }

    /// `config_id` must stay byte-identical across two connections that share ONE
    /// database but declare DIFFERENT `[actor]` ids via their own project/cwd
    /// config (ADR-096 Fork 2 hard invariant — `actor_id` must never feed
    /// `compute_config_id`, and neither may the identity-derived
    /// `visible_namespaces` fold-in). `default_namespace` must also stay
    /// `"local"` for both (ADR-007 Rev 4 Rule 0), independent of the configured actor.
    ///
    /// Deliberately does NOT use an explicit `--config` override for the two
    /// connections: an explicit path is tier 1 and would make the db-anchored
    /// config load (which DOES fold its own `[actor]` into `visible_namespaces`,
    /// unchanged pre-existing behavior) and the new project-actor tier read the
    /// identical file, conflating "two different db-anchored configs" (a
    /// different, pre-existing concern) with "two different project-anchored
    /// actors on one shared db-anchored config" (what Fork 2 must keep
    /// config_id-inert). Real seats share ONE db-anchored config; only their
    /// project-anchored actor differs — this test mirrors that shape via `SeatEnv`.
    #[test]
    #[serial]
    fn config_id_byte_identical_across_different_actor_ids() {
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");

        // ONE shared database, anchored in its own directory with NO `[actor]` at
        // that db-anchored config location — mirrors the real fleet shape, where
        // every seat's project config differs but the shared home database's own
        // config carries no actor.
        let db_dir = tempfile::tempdir().expect("db tempdir");
        let khive_dir = db_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir db .khive");
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").expect("touch db file");
        let db_str = db_path.to_str().expect("utf8 path").to_string();

        // Two different seat project directories, each with its OWN distinct
        // [actor] id.
        let seat_a = tempfile::tempdir().expect("seat a");
        std::fs::create_dir_all(seat_a.path().join(".khive")).expect("mkdir seat a .khive");
        std::fs::write(
            seat_a.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:actor-a\"\n",
        )
        .expect("write seat a config");

        let seat_b = tempfile::tempdir().expect("seat b");
        std::fs::create_dir_all(seat_b.path().join(".khive")).expect("mkdir seat b .khive");
        std::fs::write(
            seat_b.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:actor-b\"\n",
        )
        .expect("write seat b config");

        let cfg_a = {
            let _seat_env = SeatEnv::enter(seat_a.path());
            resolve_runtime_config(RuntimeConfigInputs {
                db: Some(&db_str),
                config: None,
                namespace: Namespace::parse("local").expect("ns"),
                namespace_explicit: false,
                actor_explicit: false,
                no_embed: true,
                packs: Some(kg_test_packs()),
                brain_profile: None,
            })
            .expect("resolve config a")
        };

        let cfg_b = {
            let _seat_env = SeatEnv::enter(seat_b.path());
            resolve_runtime_config(RuntimeConfigInputs {
                db: Some(&db_str),
                config: None,
                namespace: Namespace::parse("local").expect("ns"),
                namespace_explicit: false,
                actor_explicit: false,
                no_embed: true,
                packs: Some(kg_test_packs()),
                brain_profile: None,
            })
            .expect("resolve config b")
        };

        assert_eq!(cfg_a.actor_id.as_deref(), Some("lambda:actor-a"));
        assert_eq!(cfg_b.actor_id.as_deref(), Some("lambda:actor-b"));
        assert_ne!(
            cfg_a.actor_id, cfg_b.actor_id,
            "precondition: the two connections must actually declare different actors"
        );

        assert_eq!(
            cfg_a.default_namespace.as_str(),
            "local",
            "default_namespace must stay local regardless of the configured actor"
        );
        assert_eq!(
            cfg_b.default_namespace.as_str(),
            "local",
            "default_namespace must stay local regardless of the configured actor"
        );

        assert_eq!(
            crate::server::compute_config_id(&cfg_a, None),
            crate::server::compute_config_id(&cfg_b, None),
            "config_id must be byte-identical across connections that differ ONLY \
             in [actor] id and folded visibility — identity fields must never feed compute_config_id"
        );
    }

    // --- multi-backend boot path (ADR-028) ---

    /// Build a `RuntimeConfig` suitable for multi-backend tests: in-memory db,
    /// AllowAllGate, "local" namespace, no embedder, both kg and template packs.
    ///
    /// `db_path` mirrors what `resolve_runtime_config` sets for a `--db`-unset
    /// invocation (every call site below passes `cli_db_override: None` to
    /// `build_server_multi_backend`/`build_registry_for_multi_backend`) — the
    /// db-anchor consistency guard those functions run requires `db_path` to
    /// agree with `resolve_db_anchor` for the same input.
    fn base_runtime_config_for_multi_backend() -> RuntimeConfig {
        use khive_runtime::{AllowAllGate, BackendId, Namespace};
        RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(None),
            gate: std::sync::Arc::new(AllowAllGate),
            default_namespace: Namespace::parse("local").expect("ns"),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "template".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        }
    }

    /// Two in-memory backends — `main` plus a second named `secondary`.
    /// The `template` pack is pinned to `secondary`; `kg` defaults to `main`.
    /// Positive test: `build_server_multi_backend` must return `Ok` and both
    /// packs must be functional.
    #[tokio::test]
    #[serial]
    async fn multi_backend_boots_ok_with_two_memory_backends() {
        use crate::tools::request::RequestParams;
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "template".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let server = build_server_multi_backend(base_cfg, &khive_cfg, None)
            .expect("multi-backend boot must succeed");

        // kg round-trip: create an entity on the main backend.
        let kg_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"create(kind="concept", name="MultiBackendTestEntity")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("kg dispatch must not error");

        let kg_json: serde_json::Value =
            serde_json::from_str(&kg_resp).expect("kg response is valid JSON");
        // Response shape: {"results": [{ok, tool, result}], "summary": {...}}
        let first_ok = kg_json["results"][0]["ok"].as_bool();
        assert_eq!(
            first_ok,
            Some(true),
            "kg create must succeed; response: {kg_resp}"
        );

        // template round-trip: dispatch template's stateless verb on the
        // secondary backend.
        let template_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"template.my_verb(name="multi-backend-test")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("template dispatch must not error");

        let template_json: serde_json::Value =
            serde_json::from_str(&template_resp).expect("template response is valid JSON");
        let first_template_ok = template_json["results"][0]["ok"].as_bool();
        assert_eq!(
            first_template_ok,
            Some(true),
            "template.my_verb must succeed; response: {template_resp}"
        );
    }

    /// Regression for #601, adapted for #603: both multi-backend boot paths —
    /// `build_server_multi_backend` (this file) and `kkernel`'s `Command::Mcp`
    /// coordinator branch — now finish through the single
    /// [`build_server_from_multi_backend_registry`] constructor instead of each
    /// hand-assembling `from_registry_with_meta` + `with_pool`. This test calls
    /// that shared constructor directly (`coordinator: None`, the same value
    /// `build_server_multi_backend` passes) rather than re-deriving the
    /// `is_file_backed`/`pool_arc` logic inline, so a regression in the shared
    /// constructor itself — or its callers drifting back to hand-assembly —
    /// fails here directly. The kkernel-vs-`build_server_multi_backend` parity
    /// itself is covered end-to-end by `kkernel`'s own
    /// `multi_backend_boot_paths_share_identical_wiring_surface` test, which
    /// exercises the actual coordinator branch.
    #[test]
    #[serial]
    fn kkernel_multi_backend_path_wires_pool_for_file_backed_main() {
        let dir = tempfile::tempdir().expect("temp dir");
        let main_path = dir.path().join("main.db");

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .expect("multi-backend registry build must succeed");
        let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);

        assert!(
            server.pool().is_some(),
            "file-backed multi-backend main must wire a checkpoint pool onto the server"
        );
    }

    /// Sibling guard: an in-memory main backend must never carry a checkpoint pool
    /// (checkpoint_once must never run on a non-WAL, in-memory connection). Also
    /// exercises `build_server_from_multi_backend_registry` — see the note on the
    /// sibling test above.
    #[test]
    #[serial]
    fn kkernel_multi_backend_path_leaves_pool_none_for_in_memory_main() {
        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .expect("multi-backend registry build must succeed");
        let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);

        assert!(
            server.pool().is_none(),
            "in-memory multi-backend main must never carry a checkpoint pool"
        );
    }

    // ── ADR-111 Amendment 2: `resolve_blob_store` must
    // actually be reached from the real boot paths, not only its own unit
    // tests. Both tests below assert against the credential-env error
    // `S3BlobStore::new` raises with no AWS creds in the environment --
    // exactly the technique `khive-runtime`'s own `resolve_blob_store` tests
    // use -- but reached through `build_server`/`build_registry_for_multi_backend`
    // themselves, proving the boot path resolves and installs the configured
    // `S3BlobStore` rather than silently keeping the default `FsBlobStore`.

    #[test]
    #[serial]
    fn single_backend_boot_wires_configured_s3_blob_store() {
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        let prev_access_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let prev_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");

        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = write_config(
            dir.path(),
            r#"
[storage.blob]
backend = "s3"
bucket = "khive-blobs"
region = "us-east-1"
"#,
        );

        use clap::Parser;
        let args = Args::parse_from([
            "mcp",
            "--db",
            ":memory:",
            "--pack",
            "kg",
            "--config",
            config_path.to_str().expect("utf8 path"),
        ]);

        let result = build_server(&args);

        match prev_access_key {
            Some(v) => std::env::set_var("AWS_ACCESS_KEY_ID", v),
            None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
        }
        match prev_secret_key {
            Some(v) => std::env::set_var("AWS_SECRET_ACCESS_KEY", v),
            None => std::env::remove_var("AWS_SECRET_ACCESS_KEY"),
        }

        let err = result.err().expect(
            "an s3 blob backend with no AWS credentials must fail boot through the real \
             single-backend path -- a silent fs fallback would return Ok here instead",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("AWS_ACCESS_KEY_ID"),
            "expected the credential-env error surfaced through build_server, got: {msg}"
        );
    }

    #[test]
    #[serial]
    fn multi_backend_boot_wires_configured_s3_blob_store() {
        let prev_access_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let prev_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::S3 {
                    bucket: "khive-blobs".to_string(),
                    region: "us-east-1".to_string(),
                    endpoint: None,
                    prefix: None,
                    allow_http: None,
                }),
            },
            ..KhiveConfig::default()
        };
        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, None);

        match prev_access_key {
            Some(v) => std::env::set_var("AWS_ACCESS_KEY_ID", v),
            None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
        }
        match prev_secret_key {
            Some(v) => std::env::set_var("AWS_SECRET_ACCESS_KEY", v),
            None => std::env::remove_var("AWS_SECRET_ACCESS_KEY"),
        }

        let err = result.err().expect(
            "an s3 blob backend with no AWS credentials must fail boot through the real \
             multi-backend path -- a silent fs fallback would return Ok here instead",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("AWS_ACCESS_KEY_ID"),
            "expected the credential-env error surfaced through \
             build_registry_for_multi_backend, got: {msg}"
        );
    }

    // ── ADR-111 Amendment 2: the two tests above only
    // prove the fail-closed error path. The three tests below exercise the
    // successful construction-and-install branch of `install_resolved_blob_store`
    // (the real call site: `:1826` single-backend, `:1567` multi-backend) plus
    // the no-`[storage.blob]` filesystem-default boot promised by ADR-111
    // Amendment 2. `BlobStore` carries a `Debug` supertrait (khive-storage)
    // for exactly this purpose: it lets these tests tell which concrete
    // backend got installed behind `Arc<dyn BlobStore>` via
    // `format!("{store:?}")` without adding a downcast/type-name method to
    // the production trait surface.

    /// Isolated dummy (non-secret, never-valid) AWS credentials for the
    /// success-path tests below. `S3BlobStore::new` only builds an
    /// `AmazonS3` client (`object_store`'s `AmazonS3Builder::build`); it
    /// performs no network I/O, so a syntactically-valid dummy key pair is
    /// enough to reach a successful `Ok` construction.
    const DUMMY_AWS_ACCESS_KEY_ID: &str = "AKIADUMMYWITNESSKEY00";
    const DUMMY_AWS_SECRET_ACCESS_KEY: &str = "dummy-witness-secret-access-key-never-real";

    /// RAII guard: sets the two AWS credential env vars to isolated dummy
    /// values for the duration of the test, restoring whatever was
    /// previously present (usually nothing) on drop. Paired with `#[serial]`
    /// on every test that uses it, matching the convention the two boot
    /// tests above already established for this same pair of env vars.
    struct DummyAwsCredsGuard {
        prev_access_key: Option<String>,
        prev_secret_key: Option<String>,
    }

    impl DummyAwsCredsGuard {
        fn set() -> Self {
            let prev_access_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
            let prev_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
            std::env::set_var("AWS_ACCESS_KEY_ID", DUMMY_AWS_ACCESS_KEY_ID);
            std::env::set_var("AWS_SECRET_ACCESS_KEY", DUMMY_AWS_SECRET_ACCESS_KEY);
            Self {
                prev_access_key,
                prev_secret_key,
            }
        }
    }

    impl Drop for DummyAwsCredsGuard {
        fn drop(&mut self) {
            match self.prev_access_key.take() {
                Some(v) => std::env::set_var("AWS_ACCESS_KEY_ID", v),
                None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
            }
            match self.prev_secret_key.take() {
                Some(v) => std::env::set_var("AWS_SECRET_ACCESS_KEY", v),
                None => std::env::remove_var("AWS_SECRET_ACCESS_KEY"),
            }
        }
    }

    fn s3_blob_config() -> BlobConfig {
        BlobConfig::S3 {
            bucket: "khive-blobs".to_string(),
            region: "us-east-1".to_string(),
            endpoint: None,
            prefix: None,
            allow_http: None,
        }
    }

    /// Positive counterpart to `multi_backend_boot_wires_configured_s3_blob_store`:
    /// with valid (dummy) AWS credentials present, the multi-backend startup
    /// path must resolve the configured `S3BlobStore` once (`:1567`) and
    /// install it on every per-pack runtime this boot produces.
    #[test]
    #[serial]
    fn multi_backend_boot_installs_s3_blob_store_on_successful_selection() {
        let _creds = DummyAwsCredsGuard::set();

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            storage: StorageSectionConfig {
                blob: Some(s3_blob_config()),
            },
            ..KhiveConfig::default()
        };
        let base_cfg = base_runtime_config_for_multi_backend();

        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .expect("valid dummy AWS credentials must resolve through the multi-backend path");

        assert!(
            !multi.per_pack_runtimes.is_empty(),
            "precondition: the base config declares at least one pack"
        );
        for (pack_name, rt) in &multi.per_pack_runtimes {
            let store = rt.blob_store().unwrap_or_else(|| {
                panic!("pack {pack_name:?} must have the S3 selection installed on its runtime")
            });
            let debug = format!("{store:?}");
            assert!(
                debug.contains("S3BlobStore"),
                "pack {pack_name:?}: expected the installed store to be an S3BlobStore, got: {debug}"
            );
        }
    }

    /// Regression for ADR-073: a pack assigned to a secondary backend must
    /// have `core_backend` wired at boot so that `rt.core().backend_id()` returns "main".
    ///
    /// Before the fix, `build_server_multi_backend` called `KhiveRuntime::from_backend`
    /// directly (without `with_core_backend`), so `core()` fell back to `self.clone()` and
    /// returned the secondary-backend handle — silently defeating the ADR-073 contract.
    /// Both boot paths now delegate to `build_pack_runtime`, which applies the wiring in
    /// one place and prevents any future path from drifting.
    #[test]
    #[serial]
    fn secondary_pack_runtime_core_resolves_to_main_after_build_registry() {
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "template".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .expect("multi-backend registry must boot");

        let template_rt = result
            .per_pack_runtimes
            .get("template")
            .expect("template pack runtime must be present in per_pack_runtimes");

        // Own backend_id is "secondary" — not main.
        assert_eq!(
            template_rt.backend_id().as_str(),
            "secondary",
            "template pack runtime's own backend_id must be \"secondary\""
        );

        // ADR-073 contract: core() on a secondary-backend pack must return a
        // main-bound handle, not a clone of self. Failure here means the
        // build_pack_runtime wiring was not applied.
        assert_eq!(
            template_rt.core().backend_id().as_str(),
            BackendId::MAIN,
            "secondary-backend pack must have core_backend wired to main (ADR-073); \
             core().backend_id() returned {:?} — build_pack_runtime wiring missing",
            template_rt.core().backend_id().as_str()
        );
    }

    /// ADR-091 Amendment 3 fan-out regression: two backends declared at
    /// alias spellings of the SAME database file (a direct path and a
    /// symlinked path) must mint the same canonical `DbIdentity` and
    /// therefore dedup to exactly one secondary pool. The pointer-identity
    /// dedup this replaced would have kept both `Arc<ConnectionPool>`
    /// instances distinct, letting two `SweepBackend`s race on one
    /// heartbeat file.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn secondary_pools_dedup_by_canonical_identity_across_alias_spellings() {
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let real_path = dir.path().join("khive.db");
        std::fs::write(&real_path, b"").unwrap();
        let alias_path = dir.path().join("khive_alias.db");
        std::os::unix::fs::symlink(&real_path, &alias_path).unwrap();

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "direct".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(real_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "alias".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(alias_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "kg".to_string(),
                    PackConfig {
                        backend: "direct".to_string(),
                    },
                );
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "alias".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();
        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .expect("multi-backend registry with alias-spelled backends must boot");

        let secondary = secondary_file_backed_pools(&multi);
        assert_eq!(
            secondary.len(),
            1,
            "two backends aliasing the same database file must dedup to exactly one \
             secondary pool by canonical identity, got {} pools",
            secondary.len()
        );

        let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);
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
        assert_eq!(
            backends.len(),
            1,
            "exactly one SweepBackend must survive dedup for the alias pair — the \
             in-memory main backend contributes no pool of its own"
        );
    }

    /// Issue #553: `--db :memory:` (or `KHIVE_DB=:memory:`) must not be silently
    /// ignored just because `[[backends]]` declares real sqlite backends. Passing
    /// `Some(":memory:")` as `cli_db_override` must force every declared backend
    /// in-memory for this invocation, and the declared sqlite paths must never be
    /// created on disk.
    #[test]
    #[serial]
    fn memory_override_forces_all_backends_in_memory_and_never_creates_sqlite_file() {
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main_should_never_be_created.db");
        let secondary_path = dir.path().join("secondary_should_never_be_created.db");

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(secondary_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, Some(":memory:"));
        if let Err(ref e) = result {
            panic!(
                "--db :memory: override must force both declared sqlite backends \
                 in-memory and boot successfully; got: {e}"
            );
        }

        assert!(
            !main_path.exists(),
            "main backend's declared sqlite path must never be created on disk when \
             --db :memory: overrides it; found file at {main_path:?}"
        );
        assert!(
            !secondary_path.exists(),
            "secondary backend's declared sqlite path must never be created on disk \
             when --db :memory: overrides it; found file at {secondary_path:?}"
        );
    }

    /// Issue #553: a concrete `--db` path override combined with declared
    /// `[[backends]]` is ambiguous (which of N declared backends should it apply
    /// to?) and must fail loud, pointing at khive.toml as the place to make the
    /// change, rather than silently collapsing distinct backends onto one path.
    #[test]
    #[serial]
    fn concrete_db_override_with_backends_declared_is_rejected() {
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        // `db_path` matches the concrete override passed below (the db-anchor
        // consistency guard requires this pairing) — the ambiguity rejection
        // this test exercises is a downstream check inside
        // `build_registry_for_multi_backend`, distinct from anchor drift.
        let base_cfg = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(Some("/tmp/some-explicit-override.db")),
            ..base_runtime_config_for_multi_backend()
        };

        let result = build_registry_for_multi_backend(
            base_cfg,
            &khive_cfg,
            Some("/tmp/some-explicit-override.db"),
        );
        assert!(
            result.is_err(),
            "a concrete --db path override combined with declared [[backends]] must \
             be rejected as ambiguous"
        );
        if let Err(err) = result {
            let msg = err.to_string();
            assert!(
                msg.contains("khive.toml"),
                "error message must point at khive.toml as where to make the change \
                 instead; got: {msg}"
            );
        }
    }

    /// Negative test: `[[backends]]` is declared but there is no entry named
    /// `"main"`. `build_server_multi_backend` must return an error whose
    /// message mentions `"main"` so operators know what to fix.
    #[test]
    #[serial]
    fn multi_backend_missing_main_returns_error_mentioning_main() {
        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "secondary".to_string(), // intentionally NOT "main"
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            packs: std::collections::HashMap::new(),
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_server_multi_backend(base_cfg, &khive_cfg, None);
        assert!(
            result.is_err(),
            "missing main backend must produce an error"
        );
        // Neither unwrap_err nor expect_err work because KhiveMcpServer is not Debug.
        // Extract the error via match instead.
        if let Err(err) = result {
            assert!(
                err.to_string().contains("main"),
                "error message must mention \"main\"; got: {err}"
            );
        }
    }

    /// Regression for MCP-AUD-001 / #419: a pack explicitly configured to a
    /// backend that has no matching `[[backends]]` entry must fail closed
    /// instead of silently falling back to `main`. `build_registry_for_multi_backend`
    /// must return an `Err` mentioning the pack, the requested backend, and the
    /// defined backends.
    #[test]
    #[serial]
    fn multi_backend_registry_rejects_undefined_pack_backend() {
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "template".to_string(),
                    PackConfig {
                        backend: "archive".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, None);
        assert!(
            result.is_err(),
            "an undeclared configured pack backend must be a startup error, not a silent \
             fallback to main"
        );
        // MultiBackendRegistry does not implement Debug, so expect_err/unwrap_err are
        // unavailable; extract the error via match instead (same pattern as
        // multi_backend_missing_main_returns_error_mentioning_main above).
        if let Err(err) = result {
            let msg = err.to_string();
            assert!(
                msg.contains("packs.template"),
                "error must name the pack; got: {msg}"
            );
            assert!(
                msg.contains("archive"),
                "error must name the undeclared backend; got: {msg}"
            );
            assert!(
                msg.contains("main"),
                "error must list the defined backends; got: {msg}"
            );
        }
    }

    /// Same regression as `multi_backend_registry_rejects_undefined_pack_backend`
    /// but through the `build_server_multi_backend` public builder, which has its
    /// own independent per-pack backend resolution loop.
    #[test]
    #[serial]
    fn multi_backend_server_rejects_undefined_pack_backend() {
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "template".to_string(),
                    PackConfig {
                        backend: "archive".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_server_multi_backend(base_cfg, &khive_cfg, None);
        assert!(
            result.is_err(),
            "an undeclared configured pack backend must be a startup error, not a silent \
             fallback to main"
        );
        if let Err(err) = result {
            let msg = err.to_string();
            assert!(
                msg.contains("packs.template"),
                "error must name the pack; got: {msg}"
            );
            assert!(
                msg.contains("archive"),
                "error must name the undeclared backend; got: {msg}"
            );
            assert!(
                msg.contains("main"),
                "error must list the defined backends; got: {msg}"
            );
        }
    }

    /// B-SHOULD-FIX-1 (SAFETY): A backend opened with `read_only = true` must
    /// reject write operations. Verified by opening the file backend read-only and
    /// confirming that writing through `apply_pack_ddl_statements` errors (the
    /// writer has PRAGMA query_only = ON).
    #[test]
    fn read_only_backend_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ro_test.db");

        // Create a writable backend first so the file exists.
        let rw = StorageBackend::sqlite(&db_path).expect("rw backend");
        rw.apply_pack_ddl_statements(&[
            "CREATE TABLE IF NOT EXISTS ro_check (id INTEGER PRIMARY KEY)",
        ])
        .expect("DDL on rw backend");
        drop(rw);

        // Re-open read-only and confirm writes fail.
        let ro = StorageBackend::sqlite_read_only(&db_path).expect("ro backend");
        let result = ro.apply_pack_ddl_statements(&["INSERT INTO ro_check (id) VALUES (1)"]);
        assert!(
            result.is_err(),
            "write to a read-only backend must fail; got Ok(())"
        );
    }

    /// RAII guard: redirects `HOME` and restores the prior value on drop.
    struct HomeGuard {
        original: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn redirect_to(dir: &std::path::Path) -> Self {
            let original = std::env::var_os("HOME");
            std::env::set_var("HOME", dir);
            Self { original }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn duplicate_sqlite_path_config(db_path: &std::path::Path) -> KhiveConfig {
        use khive_runtime::PackConfig;

        KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(db_path.to_path_buf()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "alias".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(db_path.to_path_buf()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut packs = std::collections::HashMap::new();
                packs.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "alias".to_string(),
                    },
                );
                packs
            },
            ..KhiveConfig::default()
        }
    }

    fn memory_main_backend_config() -> KhiveConfig {
        KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        }
    }

    fn assert_db_anchor_drift<T>(result: anyhow::Result<T>) {
        match result {
            Err(error) => assert!(
                error.to_string().contains("db-path resolution drift"),
                "legacy builder must reject raw db input that disagrees with the resolved config: {error}"
            ),
            Ok(_) => panic!("legacy builder accepted raw db input that disagrees with the resolved config"),
        }
    }

    #[test]
    fn legacy_registry_rejects_mismatched_explicit_db_override() {
        let base_cfg = RuntimeConfig {
            db_path: Some(PathBuf::from("/tmp/khive-resolved.db")),
            ..base_runtime_config_for_multi_backend()
        };

        assert_db_anchor_drift(build_registry_for_multi_backend(
            base_cfg,
            &memory_main_backend_config(),
            Some("/tmp/khive-raw.db"),
        ));
    }

    #[test]
    fn legacy_server_rejects_mismatched_explicit_db_override() {
        let base_cfg = RuntimeConfig {
            db_path: Some(PathBuf::from("/tmp/khive-resolved.db")),
            ..base_runtime_config_for_multi_backend()
        };

        assert_db_anchor_drift(build_server_multi_backend(
            base_cfg,
            &memory_main_backend_config(),
            Some("/tmp/khive-raw.db"),
        ));
    }

    #[test]
    #[serial]
    fn legacy_registry_rejects_unset_db_after_home_changes() {
        let first_home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::redirect_to(first_home.path());
        let base_cfg = base_runtime_config_for_multi_backend();
        let second_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", second_home.path());

        assert_db_anchor_drift(build_registry_for_multi_backend(
            base_cfg,
            &memory_main_backend_config(),
            None,
        ));
    }

    #[test]
    #[serial]
    fn legacy_server_rejects_unset_db_after_home_changes() {
        let first_home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::redirect_to(first_home.path());
        let base_cfg = base_runtime_config_for_multi_backend();
        let second_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", second_home.path());

        assert_db_anchor_drift(build_server_multi_backend(
            base_cfg,
            &memory_main_backend_config(),
            None,
        ));
    }

    /// B-SHOULD-FIX-2 (data safety): Two [[backends]] entries whose sqlite paths
    /// canonicalize to the same file must share a single Arc<StorageBackend> and
    /// run migrations only once. Verified by using two names that differ only by
    /// `./` prefix while pointing at the same absolute path.
    #[test]
    #[serial]
    fn duplicate_sqlite_paths_deduplicated_to_single_backend() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared.db");
        let khive_cfg = duplicate_sqlite_path_config(&db_path);

        let base_cfg = base_runtime_config_for_multi_backend();

        // Must boot successfully (dedup prevents double-migration / SQLITE_BUSY).
        let result = build_server_multi_backend(base_cfg, &khive_cfg, None);
        if let Err(ref e) = result {
            panic!(
                "two backends with the same canonical path must share one Arc and boot ok; got: {e}"
            );
        }
    }

    /// Regression for #720: changing `HOME` after runtime-config resolution but
    /// before multi-backend registry construction must not change the database
    /// anchor used by the consistency guard.
    #[test]
    #[serial]
    fn multi_backend_boot_uses_anchor_captured_by_runtime_config() {
        let first_home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::redirect_to(first_home.path());
        let config_path = first_home.path().join("config.toml");
        std::fs::write(&config_path, "").expect("write empty config");
        let (base_cfg, db_anchor) = resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
            db: None,
            config: Some(&config_path),
            namespace: Namespace::parse("local").expect("namespace"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve runtime config before HOME changes");

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("shared.db");
        let khive_cfg = duplicate_sqlite_path_config(&db_path);

        let second_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", second_home.path());
        let result = build_server_multi_backend_with_db_anchor(
            base_cfg,
            &khive_cfg,
            None,
            db_anchor.as_deref(),
        );
        if let Err(error) = result {
            panic!(
                "multi-backend construction must retain the anchor captured by \
                 resolve_runtime_config instead of re-reading HOME: {error}"
            );
        }
    }

    /// Issue #553 sibling gap: `build_server_multi_backend` is reachable from
    /// `build_server` -> `main.rs` whenever `[[backends]]` is non-empty (e.g.
    /// exactly one declared backend, which still routes through `build_server`'s
    /// "single-backend, zero-change path" in main.rs since that dispatch only
    /// checks `backends.len() <= 1`, while `build_server` itself checks
    /// `is_empty()`). Before this fix, `build_server_multi_backend` took no
    /// db-override parameter at all, so `--db :memory:` / `KHIVE_DB=:memory:`
    /// was silently discarded on this path exactly as issue #553 described.
    /// Passing `Some(":memory:")` as `cli_db_override` must force every
    /// declared backend in-memory for this invocation, and the declared sqlite
    /// paths must never be created on disk.
    #[test]
    #[serial]
    fn memory_override_forces_all_backends_in_memory_and_never_creates_sqlite_file_via_build_server_multi_backend(
    ) {
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main_should_never_be_created.db");
        let secondary_path = dir.path().join("secondary_should_never_be_created.db");

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(secondary_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_server_multi_backend(base_cfg, &khive_cfg, Some(":memory:"));
        if let Err(ref e) = result {
            panic!(
                "--db :memory: override must force both declared sqlite backends \
                 in-memory and boot successfully; got: {e}"
            );
        }

        assert!(
            !main_path.exists(),
            "main backend's declared sqlite path must never be created on disk when \
             --db :memory: overrides it; found file at {main_path:?}"
        );
        assert!(
            !secondary_path.exists(),
            "secondary backend's declared sqlite path must never be created on disk \
             when --db :memory: overrides it; found file at {secondary_path:?}"
        );
    }

    /// Issue #553 sibling gap: a concrete `--db` path override combined with
    /// declared `[[backends]]` is ambiguous (which of N declared backends
    /// should it apply to?) and must fail loud on the `build_server_multi_backend`
    /// path too, pointing at khive.toml as the place to make the change, rather
    /// than silently collapsing distinct backends onto one path.
    #[test]
    #[serial]
    fn concrete_db_override_with_backends_declared_is_rejected_via_build_server_multi_backend() {
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        // `db_path` matches the concrete override passed below (the db-anchor
        // consistency guard requires this pairing) — the ambiguity rejection
        // this test exercises is a downstream check inside
        // `build_registry_for_multi_backend`, distinct from anchor drift.
        let base_cfg = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(Some("/tmp/some-explicit-override.db")),
            ..base_runtime_config_for_multi_backend()
        };

        let result = build_server_multi_backend(
            base_cfg,
            &khive_cfg,
            Some("/tmp/some-explicit-override.db"),
        );
        assert!(
            result.is_err(),
            "a concrete --db path override combined with declared [[backends]] must \
             be rejected as ambiguous"
        );
        if let Err(err) = result {
            let msg = err.to_string();
            assert!(
                msg.contains("khive.toml"),
                "error message must point at khive.toml as where to make the change \
                 instead; got: {msg}"
            );
        }
    }

    // B-SHOULD-FIX-3 collision test lives in khive-runtime/src/pack.rs
    // (apply_schema_plans_with_map_collision_is_an_error) because
    // `VerbRegistryBuilder::register_boxed` is pub(crate) there.

    /// B-SHOULD-FIX-4 (daemon staleness): `compute_config_id` must produce
    /// different ids for two configs that differ only in pack→backend routing.
    /// The empty-backends case must be byte-identical to the pre-change baseline.
    #[test]
    fn config_id_folds_backend_topology_when_non_empty() {
        use khive_runtime::{BackendId, KhiveConfig, Namespace, PackConfig, RuntimeConfig};

        let base_rt = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        };

        // No backends — must be byte-identical to compute_config_id(base_rt, None).
        let id_no_backends = crate::server::compute_config_id(&base_rt, None);
        let id_empty_backends =
            crate::server::compute_config_id(&base_rt, Some(&KhiveConfig::default()));
        assert_eq!(
            id_no_backends, id_empty_backends,
            "empty-backends config_id must be byte-identical to None-config config_id"
        );

        // Two configs differing only in pack→backend assignment.
        let mut packs_a = std::collections::HashMap::new();
        packs_a.insert(
            "comm".to_string(),
            PackConfig {
                backend: "secondary".to_string(),
            },
        );

        let cfg_a = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: packs_a,
            ..KhiveConfig::default()
        };

        // cfg_b: no pack assignments — comm falls back to main.
        let cfg_b = KhiveConfig {
            backends: cfg_a.backends.clone(),
            packs: std::collections::HashMap::new(),
            ..KhiveConfig::default()
        };

        let id_a = crate::server::compute_config_id(&base_rt, Some(&cfg_a));
        let id_b = crate::server::compute_config_id(&base_rt, Some(&cfg_b));

        assert_ne!(
            id_a, id_b,
            "configs differing only in pack→backend routing must produce different config_ids; \
             both produced: {id_a}"
        );
    }

    // --- should_warn_unattributed predicate ---

    fn packs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn warn_when_actor_is_none_and_comm_loaded() {
        assert!(should_warn_unattributed(None, &packs(&["kg", "comm"])));
    }

    #[test]
    fn warn_when_actor_is_local_and_comm_loaded() {
        assert!(should_warn_unattributed(
            Some("local"),
            &packs(&["kg", "comm"])
        ));
    }

    #[test]
    fn no_warn_when_actor_is_configured() {
        assert!(!should_warn_unattributed(
            Some("lambda:khive"),
            &packs(&["kg", "comm"])
        ));
    }

    #[test]
    fn no_warn_when_comm_not_loaded() {
        assert!(!should_warn_unattributed(Some("local"), &packs(&["kg"])));
    }

    #[test]
    fn no_warn_when_actor_none_and_no_comm() {
        assert!(!should_warn_unattributed(None, &packs(&["kg", "memory"])));
    }

    // --- is_strict_actor_mode predicate ---
    // All three tests mutate the process-global KHIVE_REQUIRE_ATTRIBUTED_ACTOR;
    // #[serial] prevents races under parallel test execution.

    #[test]
    #[serial]
    fn strict_mode_off_by_default() {
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        assert!(
            !is_strict_actor_mode(),
            "strict mode must be OFF when KHIVE_REQUIRE_ATTRIBUTED_ACTOR is unset"
        );
        if let Some(v) = prev {
            std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v);
        }
    }

    #[test]
    #[serial]
    fn strict_mode_on_when_env_var_is_1() {
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        assert!(
            is_strict_actor_mode(),
            "strict mode must be ON when KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1"
        );
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
    }

    #[test]
    #[serial]
    fn strict_mode_off_when_env_var_is_not_1() {
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "0");
        assert!(
            !is_strict_actor_mode(),
            "strict mode must be OFF when KHIVE_REQUIRE_ATTRIBUTED_ACTOR=0"
        );
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
    }

    // --- enforce_strict_actor_mode: shared seam regression tests ---
    // These cover the enforcement seam itself (regression guard).

    #[test]
    #[serial]
    fn enforce_strict_actor_mode_returns_err_when_strict_and_no_actor() {
        // Strict mode ON + no actor + comm pack = Err.
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        let result = enforce_strict_actor_mode(None, &packs(&["kg", "comm", "memory"]));
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        assert!(
            result.is_err(),
            "enforce_strict_actor_mode must return Err when strict mode is ON \
             and no actor is configured (comm pack loaded)"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
            "error message must name the env var; got: {msg}"
        );
        assert!(
            msg.contains("KHIVE_ACTOR"),
            "error message must name the remedy; got: {msg}"
        );
    }

    #[test]
    #[serial]
    fn enforce_strict_actor_mode_ok_when_strict_and_actor_configured() {
        // Strict mode ON + proper actor = Ok (comm pack present is irrelevant).
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        let result = enforce_strict_actor_mode(Some("lambda:tenant-x"), &packs(&["kg", "comm"]));
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        assert!(
            result.is_ok(),
            "enforce_strict_actor_mode must return Ok when actor is properly configured"
        );
    }

    #[test]
    #[serial]
    fn enforce_strict_actor_mode_ok_when_strict_off_and_no_actor() {
        // Strict mode OFF + no actor = Ok (the DEFAULT / OSS path must be unchanged).
        // This is the most critical regression guard: ensure the default-off path
        // never fires the guard.
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        let result = enforce_strict_actor_mode(None, &packs(&["kg", "comm", "memory"]));
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        assert!(
            result.is_ok(),
            "enforce_strict_actor_mode must return Ok when strict mode is OFF \
             (default OSS path must be completely unchanged)"
        );
    }

    #[test]
    #[serial]
    fn enforce_strict_actor_mode_ok_when_strict_on_but_no_comm_pack() {
        // Strict mode ON but comm pack not loaded = Ok (no risk of party-line).
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        let result = enforce_strict_actor_mode(None, &packs(&["kg", "memory"]));
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        assert!(
            result.is_ok(),
            "enforce_strict_actor_mode must return Ok when comm pack is not loaded \
             (no party-line risk even without actor)"
        );
    }

    // --- build_server's returned schedule-tick runtime (ADR-106, PR #782) ---
    //
    // Before this fix, the daemon-resident tick (`schedule_tick_loop`)
    // reconstructed its OWN `RuntimeConfig::default()` from raw `args.db` and
    // an inferred namespace, discarding everything `build_server` resolves
    // from `--config`/`[[backends]]`/`--actor`/`--pack`. These regressions
    // exercise `build_server` itself (the exact function `run()` calls) and
    // assert the runtime it hands back for the tick to drain against carries
    // the SAME resolved db path, actor identity, and pack set the live
    // server itself was built with — not a silently different one.
    //
    // All use `SeatEnv` (defined above, ADR-096 Fork 2 section) to isolate
    // cwd/HOME so no ambient developer-machine `~/.khive/config.toml` or
    // project `.khive/config.toml` can leak into the resolution, and clear
    // every `KHIVE_*` env var these tests care about so a shell-level export
    // in the test-runner's environment cannot silently change the resolved
    // config out from under the assertion.

    #[test]
    #[serial]
    fn build_server_schedule_tick_is_none_when_schedule_pack_is_not_in_the_restricted_pack_set() {
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        use clap::Parser;
        // Restrict to a pack set that deliberately excludes "schedule".
        let args = Args::parse_from(["mcp", "--db", ":memory:", "--pack", "kg"]);

        let (_server, schedule_rt) = build_server(&args).expect("build_server must succeed");
        assert!(
            schedule_rt.is_none(),
            "when the operator restricts --pack to exclude \"schedule\", the tick must have \
             nothing to drain against — never silently falling back to a runtime that can \
             dispatch through a pack the daemon was not configured to load"
        );
    }

    /// The sweep-lifecycle guard funnels every serve-path early return
    /// through the unconditional sweep shutdown: transport-resolution
    /// failure must still complete promptly (the sweep task is shut down
    /// and awaited, not leaked past the error return).
    #[tokio::test]
    #[serial]
    async fn serve_with_session_sweep_completes_shutdown_on_unknown_transport() {
        use clap::Parser;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("khive.db");
        let config_path = write_config(dir.path(), "");
        let args = Args::parse_from([
            "kkernel",
            "--db",
            db_path.to_str().expect("utf8 path"),
            "--config",
            config_path.to_str().expect("utf8 path"),
            "--transport",
            "no-such-transport",
            "--no-embed",
            "--pack",
            "kg",
        ]);
        let (server, _schedule_rt) = build_server(&args).expect("build server");

        let registry = TransportRegistry::default();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            serve_with_session_sweep(server, &args, &registry),
        )
        .await
        .expect("guard must complete promptly, including sweep shutdown")
        .expect_err("unknown transport must fail resolution");
        assert!(
            err.to_string().contains("unknown transport"),
            "unexpected error: {err}"
        );

        // No heartbeat entry may survive the guard's explicit shutdown —
        // the sweep never went over-threshold here, and its clean-shutdown
        // path removes any heartbeat it did write.
        let heartbeat = dir
            .path()
            .join("khive.db.walpin")
            .join(format!("{}.json", std::process::id()));
        assert!(
            !heartbeat.exists(),
            "sweep heartbeat must not survive the serve guard"
        );
    }

    /// Discriminating regression for the sweep-lifecycle guard: the old
    /// code's early `?` return on transport resolution dropped the handle
    /// without awaiting shutdown, so the task's exit raced the caller's
    /// resume. The guard must await `SessionSweepHandle::shutdown` on the
    /// error path — observed here via an injected task whose completion
    /// flag flips only after it receives the shutdown signal, checked
    /// synchronously the moment the guard returns.
    #[tokio::test]
    #[serial]
    async fn serve_guard_awaits_sweep_shutdown_before_returning() {
        use clap::Parser;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("khive.db");
        let config_path = write_config(dir.path(), "");
        let args = Args::parse_from([
            "kkernel",
            "--db",
            db_path.to_str().expect("utf8 path"),
            "--config",
            config_path.to_str().expect("utf8 path"),
            "--transport",
            "no-such-transport",
            "--no-embed",
            "--pack",
            "kg",
        ]);
        let (server, _schedule_rt) = build_server(&args).expect("build server");

        let completed = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
        let join = tokio::spawn({
            let completed = Arc::clone(&completed);
            async move {
                let _ = shutdown_rx.changed().await;
                completed.store(true, Ordering::SeqCst);
            }
        });
        let handle = SessionSweepHandle { shutdown_tx, join };

        let registry = TransportRegistry::default();
        let err = serve_holding_sweep(Some(handle), server, &args, &registry)
            .await
            .expect_err("unknown transport must fail resolution");
        assert!(
            err.to_string().contains("unknown transport"),
            "unexpected error: {err}"
        );
        assert!(
            completed.load(Ordering::SeqCst),
            "the guard must await sweep shutdown before returning on the \
             transport-resolution error path — an unawaited (dropped) handle \
             leaves this flag unset at the moment the guard returns"
        );
    }
}
