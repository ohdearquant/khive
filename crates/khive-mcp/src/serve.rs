//! Build the runtime + server from CLI args and serve over the selected transport.
//!
//! This is the bootstrap that the `kkernel mcp` subcommand drives. Logging is
//! initialized by the binary, not here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use khive_runtime::{
    config_from_env, run_migrations, runtime_config_from_khive_config, BackendConfig, BackendId,
    BackendKind, KhiveConfig, KhiveRuntime, PackRegistry, RuntimeConfig, StorageBackend,
    VerbRegistryBuilder,
};

use crate::args::{resolve_cli_namespace, Args};
use crate::server::KhiveMcpServer;
use crate::transport::{ServeOptions, TransportRegistry};

/// Output of [`build_registry_for_multi_backend`] — carries the registry and
/// the per-pack runtimes so `kkernel` can build a `BackendRegistry` for the
/// coordinator (ADR-029 Phase 2).
pub struct MultiBackendRegistry {
    /// The assembled [`VerbRegistry`] ready to be passed to a server.
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
pub async fn run(args: Args, registry: &TransportRegistry) -> anyhow::Result<()> {
    let server = build_server(&args)?;

    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon(server).await?;
        return Ok(());
    }
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!(
            "--daemon mode requires Unix (macOS/Linux). On Windows, use the stdio transport."
        );
    }

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

/// Serve a pre-built server (ADR-029 Phase 2 boot path).
///
/// Extracted from `run()` so that `kkernel`'s `Command::Mcp` arm can build a
/// coordinator-equipped server and then call this to drive the
/// daemon/transport dispatch. The `Args` object is still needed for `--daemon`,
/// `--transport`, and `--bind` flags.
pub async fn serve_server(
    server: KhiveMcpServer,
    args: &Args,
    registry: &TransportRegistry,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon(server).await?;
        return Ok(());
    }
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!(
            "--daemon mode requires Unix (macOS/Linux). On Windows, use the stdio transport."
        );
    }

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

/// Build the VerbRegistry and per-pack runtimes for a multi-backend deployment
/// (ADR-028 + ADR-029 Phase 2).
///
/// Returns a [`MultiBackendRegistry`] that `kkernel` uses to both:
/// 1. Construct the `KhiveMcpServer` (via `from_registry_with_meta`), and
/// 2. Build the `BackendRegistry` for the `SubstrateCoordinator`.
///
/// This is a refactor-extraction of the registry-building logic from
/// `build_server_multi_backend`, keeping the existing tests intact.
pub fn build_registry_for_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> anyhow::Result<MultiBackendRegistry> {
    // Open and migrate each declared backend, deduplicating SQLite backends by
    // canonical path (ADR-028 §8).
    let mut backends: HashMap<String, Arc<StorageBackend>> = HashMap::new();
    let mut path_to_backend: HashMap<std::path::PathBuf, Arc<StorageBackend>> = HashMap::new();
    for backend_cfg in &khive_cfg.backends {
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
        let backend_name = khive_cfg
            .packs
            .get(pack_name.as_str())
            .map(|pc| pc.backend.as_str())
            .unwrap_or(BackendId::MAIN);
        let backend = backends
            .get(backend_name)
            .cloned()
            .unwrap_or_else(|| main_backend.clone());
        let mut rt_config = base_config.clone();
        rt_config.backend_id = BackendId::new(backend_name);
        per_pack_runtimes_local.insert(
            pack_name.clone(),
            KhiveRuntime::from_backend(backend, rt_config),
        );
    }

    let default_runtime = KhiveRuntime::from_backend(main_backend.clone(), {
        let mut cfg = base_config.clone();
        cfg.backend_id = BackendId::main();
        cfg
    });

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
pub(crate) fn should_warn_unattributed(actor_id: Option<&str>, loaded_packs: &[String]) -> bool {
    let is_local = actor_id.map(|id| id == "local").unwrap_or(true);
    is_local && loaded_packs.iter().any(|p| p == "comm")
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
/// - `kkernel pending_events` (`crates/kkernel/src/pending_events.rs`) — drains
///   and dispatches scheduled events
///
/// **Pure-introspection registry construction is intentionally EXEMPT** because it
/// never dispatches verbs or reads comm/tenant data, so it carries no
/// tenant-isolation risk. Requiring an actor identity there would make
/// `kkernel pack list` and `kkernel kg validate` fail under strict mode without
/// any security benefit — an operator must be able to introspect a strict-mode
/// deployment. Exempt paths: `build_registry` in `crates/kkernel/src/pack_introspect.rs`
/// and `build_taxonomy` in `crates/kkernel/src/kg/validate.rs`. Each of those
/// functions carries an inline comment explaining why.
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
pub fn build_server(args: &Args) -> anyhow::Result<KhiveMcpServer> {
    let (cli_namespace_explicit, cli_namespace) =
        resolve_cli_namespace(args).map_err(|e| anyhow::anyhow!("{e}"))?;

    let config = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace: cli_namespace,
        namespace_explicit: cli_namespace_explicit,
        no_embed: args.no_embed,
        packs: if args.pack.is_empty() {
            None
        } else {
            Some(args.pack.clone())
        },
        brain_profile: args.brain_profile.clone(),
    })?;

    // Load the KhiveConfig to check for multi-backend declarations (ADR-028).
    // When no [[backends]] are declared, fall through to the existing single-backend path
    // to preserve byte-for-byte backward compatibility.
    let khive_cfg = KhiveConfig::load_with_home_fallback(args.config.as_deref())
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
        .unwrap_or_default();

    if khive_cfg.backends.is_empty() {
        // Single-backend path — identical to pre-ADR-028 behavior.
        let runtime = KhiveRuntime::new(config)?;
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
        return KhiveMcpServer::new(runtime).map_err(|e| anyhow::anyhow!("{e}"));
    }

    // Multi-backend path (ADR-028).
    build_server_multi_backend(config, &khive_cfg)
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

/// Open backends, run migrations, build per-pack runtimes, register packs.
///
/// Called only when `[[backends]]` is non-empty in `khive.toml`.
fn build_server_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> anyhow::Result<KhiveMcpServer> {
    // Open and migrate each declared backend, deduplicating SQLite backends by
    // canonical path (ADR-028 §8). Two [[backends]] entries that canonicalize to
    // the same file share one Arc<StorageBackend> and run migrations once.
    let mut backends: HashMap<String, Arc<StorageBackend>> = HashMap::new();
    let mut path_to_backend: HashMap<PathBuf, Arc<StorageBackend>> = HashMap::new();
    for backend_cfg in &khive_cfg.backends {
        let canonical = canonical_backend_path(backend_cfg)?;
        // Check for an already-opened backend with the same canonical path.
        if let Some(ref canon) = canonical {
            if let Some(existing) = path_to_backend.get(canon) {
                backends.insert(backend_cfg.name.clone(), existing.clone());
                continue;
            }
        }

        let backend = open_backend(backend_cfg)?;
        // Run migrations before passing backend to any runtime (risk §8 line 433).
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

    // Ensure the `main` backend exists (required fallback for unconfigured packs).
    let main_backend = backends
        .get(BackendId::MAIN)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[[backends]] is declared but no backend named \"main\" was found; \
             add a [[backends]] entry with name = \"main\""
            )
        })?
        .clone();

    // Build per-pack runtimes: each pack gets its assigned backend, or `main` as fallback.
    let pack_names = &base_config.packs;
    let mut per_pack_runtimes: HashMap<String, KhiveRuntime> = HashMap::new();
    for pack_name in pack_names {
        let backend_name = khive_cfg
            .packs
            .get(pack_name.as_str())
            .map(|pc| pc.backend.as_str())
            .unwrap_or(BackendId::MAIN);
        let backend = backends
            .get(backend_name)
            .cloned()
            .unwrap_or_else(|| main_backend.clone());
        let mut rt_config = base_config.clone();
        rt_config.backend_id = BackendId::new(backend_name);
        per_pack_runtimes.insert(
            pack_name.clone(),
            KhiveRuntime::from_backend(backend, rt_config),
        );
    }

    // Build the default runtime (for the main backend) — used for EventStore wiring.
    let default_runtime = KhiveRuntime::from_backend(main_backend.clone(), {
        let mut cfg = base_config.clone();
        cfg.backend_id = BackendId::main();
        cfg
    });

    #[cfg(feature = "bench-embedder")]
    {
        for rt in per_pack_runtimes.values() {
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

    // Build the VerbRegistry using per-pack runtimes.
    let gate = default_runtime.config().gate.clone();
    let default_namespace = default_runtime.config().default_namespace.clone();
    let config_id = crate::server::compute_config_id(default_runtime.config(), Some(khive_cfg));
    let visible_namespaces = default_runtime.config().visible_namespaces.clone();

    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(gate);
    builder.with_default_namespace(default_namespace.as_str());
    builder.with_visible_namespaces(visible_namespaces);
    // Thread authenticated actor identity (issue #75) into the multi-backend
    // registry exactly as the single-backend path does (server.rs). Without
    // this, dispatch mints ActorRef::anonymous() and comm.inbox reverts to
    // party-line, silently disabling actor-addressed delivery (ADR-057).
    builder.with_actor_id(default_runtime.config().actor_id.clone());

    // Wire EventStore from the default (main) runtime.
    if let Ok(tok) = default_runtime.authorize(khive_runtime::Namespace::local()) {
        if let Ok(event_store) = default_runtime.events(&tok) {
            builder.with_event_store(event_store);
        }
    }

    PackRegistry::register_packs_with_runtimes(
        pack_names,
        &per_pack_runtimes,
        &default_runtime,
        &mut builder,
    )
    .map_err(|e| anyhow::anyhow!("pack registration: {e}"))?;

    let registry = builder
        .build()
        .map_err(|e| anyhow::anyhow!("registry build: {e}"))?;

    // Install edge rules and embedders.
    default_runtime.install_edge_rules(registry.all_edge_rules());
    for rt in per_pack_runtimes.values() {
        rt.install_edge_rules(registry.all_edge_rules());
    }
    registry.call_register_embedders(&default_runtime);

    // Apply schema plans to each pack's assigned backend (ADR-028 §7: collision = boot failure).
    let backend_for_pack: HashMap<&str, &StorageBackend> = per_pack_runtimes
        .iter()
        .map(|(name, rt)| (name.as_str(), rt.backend()))
        .collect();
    let main_ref: &StorageBackend = main_backend.as_ref();
    registry
        .apply_schema_plans_with_map(&backend_for_pack, main_ref)
        .map_err(|e| anyhow::anyhow!("pack schema boot failure: {e}"))?;

    Ok(KhiveMcpServer::from_registry_with_meta(
        registry,
        default_namespace.as_str(),
        &config_id,
    ))
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
    let db_path = match inputs.db {
        Some(":memory:") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            Some(PathBuf::from(format!("{home}/.khive/khive.db")))
        }
    };

    let packs = inputs
        .packs
        .unwrap_or_else(|| RuntimeConfig::default().packs);

    // Tier-1: explicit CLI --brain-profile only (not env — env is tier-3, after TOML).
    // We must NOT read KHIVE_BRAIN_PROFILE here; RuntimeConfig::default() reads it, so
    // we exclude brain_profile from the default spread and set it to None (CLI-only).
    let cli_brain_profile = inputs.brain_profile.filter(|s| !s.trim().is_empty());

    let base_config = RuntimeConfig {
        db_path,
        default_namespace: inputs.namespace,
        packs,
        // Explicit CLI flag only at this tier — env and config-file tiers are applied
        // below in resolve_config / resolve_actor_from_config and apply_env_brain_profile.
        brain_profile: cli_brain_profile,
        ..RuntimeConfig::default()
    };

    let resolved = if inputs.no_embed {
        let no_embed_base = RuntimeConfig {
            embedding_model: None,
            additional_embedding_models: vec![],
            ..base_config
        };
        resolve_actor_from_config(inputs.config, no_embed_base, inputs.namespace_explicit)?
    } else {
        resolve_config(inputs.config, base_config)?
    };

    // ADR-057: the `--actor` / `--namespace` CLI flag must populate `actor_id`
    // (token attribution), matching how the `KHIVE_ACTOR` env already does via
    // RuntimeConfig::default(). Without this, `--actor lambda:x` alone — no env,
    // no config-file `[actor] id` — leaves actor_id None, so the request token
    // carries ActorRef::anonymous(): ADR-057 actor-addressed delivery degrades to
    // the party line and the unattributed-comm startup warning fires despite an
    // actor having been set. Fill only when still None so the env (base spread) and
    // a config-file `[actor] id` keep precedence; the `"local"` guard leaves the
    // default namespace anonymous (consistent with should_warn_unattributed).
    let resolved = {
        let mut resolved = resolved;
        let ns = resolved.default_namespace.as_str().to_string();
        if resolved.actor_id.is_none() && inputs.namespace_explicit && ns != "local" {
            resolved.actor_id = Some(ns);
        }
        resolved
    };

    // Tier-3 env fallback: KHIVE_BRAIN_PROFILE is applied AFTER CLI (tier-1) and
    // config-file (tier-2) so that a project or global TOML always wins over the env var.
    Ok(apply_env_brain_profile(resolved))
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
fn resolve_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
) -> anyhow::Result<RuntimeConfig> {
    match KhiveConfig::load_with_home_fallback(config_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            let env_primary = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
            let env_additional = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS").ok();
            if env_primary.is_some() || env_additional.is_some() {
                tracing::warn!(
                    "khive config file is present; KHIVE_EMBEDDING_MODEL and \
                     KHIVE_ADDITIONAL_EMBEDDING_MODELS env vars are ignored"
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

/// Resolve only the actor namespace from a config file (no-embed path).
fn resolve_actor_from_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    cli_namespace_explicit: bool,
) -> anyhow::Result<RuntimeConfig> {
    if cli_namespace_explicit {
        return Ok(base);
    }
    match KhiveConfig::load_with_home_fallback(config_path)
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
    use khive_runtime::Namespace;
    use serial_test::serial;
    use std::io::Write;

    fn write_config(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("khive.toml");
        let mut f = std::fs::File::create(&path).expect("create config file");
        f.write_all(body.as_bytes()).expect("write config");
        path
    }

    // The resolver MUST honor config-file `[[engines]]` over RuntimeConfig
    // defaults — otherwise `kkernel reindex` embeds for the wrong model set
    // versus what `kkernel mcp` serves recall from. Regression for PR #8
    // blocker.
    #[test]
    #[serial]
    fn resolver_uses_config_file_engines_over_defaults() {
        // Ensure env vars cannot leak into either branch.
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");

        let default_cfg = RuntimeConfig::default();
        let default_primary = format!("{:?}", default_cfg.embedding_model);
        // Default ships a non-empty additional-engine list (the multilingual
        // model). The single-engine config file below must override it.
        assert!(
            !default_cfg.additional_embedding_models.is_empty(),
            "precondition: default config has additional engines"
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
            no_embed: false,
            packs: None,
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
    }

    /// Regression for BLOCKER-1 (PR #52 codex review): project-toml brain_profile
    /// MUST win over KHIVE_BRAIN_PROFILE env var.
    ///
    /// Merged ADR-035 §Precedence: CLI > project toml > global toml > env > default.
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
            no_embed: false,
            packs: None,
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
            no_embed: false,
            packs: None,
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
            no_embed: false,
            packs: None,
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

    /// Regression for code-review Finding 1 (#203): the `--actor` / `--namespace`
    /// CLI flag must set `actor_id`, not just `default_namespace`. Before the fix,
    /// `--actor lambda:x` with no `KHIVE_ACTOR` env and no config-file `[actor] id`
    /// left actor_id None → anonymous token → degraded ADR-057 comm + false warning.
    #[test]
    #[serial]
    fn cli_actor_flag_populates_actor_id() {
        std::env::remove_var("KHIVE_ACTOR");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace: Namespace::parse("lambda:agent-x").expect("ns"),
            namespace_explicit: true,
            no_embed: true,
            packs: None,
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

    /// The `"local"` default namespace must stay anonymous (actor_id None) even when
    /// passed explicitly, so `should_warn_unattributed` still flags an unset actor.
    #[test]
    #[serial]
    fn cli_actor_flag_local_stays_anonymous() {
        std::env::remove_var("KHIVE_ACTOR");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id, None,
            "explicit --actor local must remain anonymous (no actor_id) so the \
             unattributed-comm warning still fires"
        );
    }

    // --- multi-backend boot path (ADR-028) ---

    /// Build a `RuntimeConfig` suitable for multi-backend tests: in-memory db,
    /// AllowAllGate, "local" namespace, no embedder, both kg and comm packs.
    fn base_runtime_config_for_multi_backend() -> RuntimeConfig {
        use khive_runtime::{AllowAllGate, BackendId, Namespace};
        RuntimeConfig {
            db_path: None,
            gate: std::sync::Arc::new(AllowAllGate),
            default_namespace: Namespace::parse("local").expect("ns"),
            embedding_model: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        }
    }

    /// Two in-memory backends — `main` plus a second named `secondary`.
    /// The `comm` pack is pinned to `secondary`; `kg` defaults to `main`.
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

        let server = build_server_multi_backend(base_cfg, &khive_cfg)
            .expect("multi-backend boot must succeed");

        // kg round-trip: create an entity on the main backend.
        let kg_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"create(kind="concept", name="MultiBackendTestEntity")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
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

        // comm round-trip: send a message on the secondary backend.
        let comm_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"comm.send(to="local", content="multi-backend-test")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
            })
            .await
            .expect("comm dispatch must not error");

        let comm_json: serde_json::Value =
            serde_json::from_str(&comm_resp).expect("comm response is valid JSON");
        let first_comm_ok = comm_json["results"][0]["ok"].as_bool();
        assert_eq!(
            first_comm_ok,
            Some(true),
            "comm.send must succeed; response: {comm_resp}"
        );
    }

    /// Regression for B-BLOCKER-1 (HC-7 critic): the multi-backend boot path
    /// MUST thread the configured actor identity (issue #75) into the registry,
    /// exactly as the single-backend path does. If `with_actor_id` is dropped,
    /// dispatch mints `ActorRef::anonymous()` and `comm.inbox` reverts to
    /// party-line — silently re-opening the cross-actor leak #75 fixed. With a
    /// configured actor `"actor-b"`, a message addressed to `"actor-a"` must NOT
    /// appear in `actor-b`'s inbox, while one addressed to `"actor-b"` must.
    #[tokio::test]
    #[serial]
    async fn multi_backend_preserves_actor_filtering() {
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
                    "comm".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        // Configured actor — the value #75 threads end-to-end.
        let base_cfg = RuntimeConfig {
            actor_id: Some("actor-b".to_string()),
            ..base_runtime_config_for_multi_backend()
        };

        let server = build_server_multi_backend(base_cfg, &khive_cfg)
            .expect("multi-backend boot must succeed");

        let dispatch = |ops: String| {
            let server = &server;
            async move {
                let resp = server
                    .dispatch_request_local(RequestParams {
                        ops,
                        presentation: None,
                        presentation_per_op: None,
                        save_to: None,
                    })
                    .await
                    .expect("dispatch must not error");
                serde_json::from_str::<serde_json::Value>(&resp).expect("valid JSON")
            }
        };

        // One message to a different actor, one to ourselves.
        let to_a = dispatch(r#"comm.send(to="actor-a", content="for-a")"#.to_string()).await;
        assert_eq!(to_a["results"][0]["ok"].as_bool(), Some(true), "{to_a}");
        let to_b = dispatch(r#"comm.send(to="actor-b", content="for-b")"#.to_string()).await;
        assert_eq!(to_b["results"][0]["ok"].as_bool(), Some(true), "{to_b}");

        // Inbox for the configured actor (actor-b) must be filtered by to_actor.
        let inbox = dispatch(r#"comm.inbox()"#.to_string()).await;
        let result = &inbox["results"][0]["result"];
        let messages = result["messages"]
            .as_array()
            .expect("inbox returns a messages array");

        let contents: Vec<&str> = messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(
            contents.contains(&"for-b"),
            "actor-b must see the message addressed to it; got {contents:?}"
        );
        assert!(
            !contents.contains(&"for-a"),
            "actor-b must NOT see the message addressed to actor-a (leak #75 / B-BLOCKER-1); \
             got {contents:?} — actor identity was not threaded into the multi-backend registry"
        );
    }

    /// Negative test: `[[backends]]` is declared but there is no entry named
    /// `"main"`. `build_server_multi_backend` must return an error whose
    /// message mentions `"main"` so operators know what to fix.
    #[test]
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

        let result = build_server_multi_backend(base_cfg, &khive_cfg);
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

    /// B-SHOULD-FIX-2 (data safety): Two [[backends]] entries whose sqlite paths
    /// canonicalize to the same file must share a single Arc<StorageBackend> and
    /// run migrations only once. Verified by using two names that differ only by
    /// `./` prefix while pointing at the same absolute path.
    #[test]
    fn duplicate_sqlite_paths_deduplicated_to_single_backend() {
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared.db");
        let db_path_str = db_path.to_str().unwrap();

        // Two backend names pointing to the same file (one with ./ prefix).
        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(db_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "alias".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(db_path.clone()),
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
                        backend: "alias".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };
        let _ = db_path_str; // used above to show intent

        let base_cfg = base_runtime_config_for_multi_backend();

        // Must boot successfully (dedup prevents double-migration / SQLITE_BUSY).
        let result = build_server_multi_backend(base_cfg, &khive_cfg);
        if let Err(ref e) = result {
            panic!(
                "two backends with the same canonical path must share one Arc and boot ok; got: {e}"
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

    /// Physical isolation guard: a record written through a pack pinned to backend B's
    /// SQLite file MUST NOT appear in backend A's file, and vice versa.
    ///
    /// This is the "billing data must not mix with agent memory" guarantee.
    /// The test opens each file independently with rusqlite after the server is
    /// dropped to confirm cross-file absence in both directions.
    ///
    /// Schema facts discovered from crates/khive-db/sql/:
    ///   entities table — column `name` holds the entity name (entities-ddl.sql)
    ///   notes table    — column `content` holds the message body; `kind` = "message"
    ///                    for comm.send output (notes-ddl.sql + comm handlers.rs)
    ///
    /// Relies on `base_runtime_config_for_multi_backend` leaving `embedding_model`
    /// unset: no embedder means no `vec0` virtual table is created, so the plain
    /// `rusqlite::Connection::open` below (which does not load the vec0 extension)
    /// can read both files. If an embedder is ever added to that helper, this test
    /// must load the extension or query through a runtime instead.
    #[tokio::test]
    #[serial]
    async fn multi_backend_isolates_pack_data_to_separate_files() {
        use crate::tools::request::RequestParams;
        use khive_runtime::PackConfig;
        use rusqlite::Connection;

        let dir = tempfile::tempdir().expect("temp dir");
        let main_path = dir.path().join("main.db");
        let second_path = dir.path().join("second.db");

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
                    name: "second".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(second_path.clone()),
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
                        backend: "second".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let server = build_server_multi_backend(base_cfg, &khive_cfg)
            .expect("multi-backend boot must succeed");

        let dispatch = |ops: String| {
            let server = &server;
            async move {
                server
                    .dispatch_request_local(RequestParams {
                        ops,
                        presentation: None,
                        presentation_per_op: None,
                        save_to: None,
                    })
                    .await
                    .expect("dispatch must not error")
            }
        };

        // kg → main.db: create an entity
        let kg_resp =
            dispatch(r#"create(kind="concept", name="MainOnlyEntity")"#.to_string()).await;
        let kg_json: serde_json::Value =
            serde_json::from_str(&kg_resp).expect("kg response is valid JSON");
        assert_eq!(
            kg_json["results"][0]["ok"].as_bool(),
            Some(true),
            "kg create must succeed; response: {kg_resp}"
        );

        // comm → second.db: send a message
        let comm_resp =
            dispatch(r#"comm.send(to="local", content="SecondOnlyMsg")"#.to_string()).await;
        let comm_json: serde_json::Value =
            serde_json::from_str(&comm_resp).expect("comm response is valid JSON");
        assert_eq!(
            comm_json["results"][0]["ok"].as_bool(),
            Some(true),
            "comm.send must succeed; response: {comm_resp}"
        );

        // Drop the server so WAL is checkpointed and files are fully flushed
        // before we open them with rusqlite.
        drop(server);

        // --- Verify main.db ---
        let main_conn = Connection::open(&main_path).expect("open main.db");

        let main_entity_count: i64 = main_conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE name = 'MainOnlyEntity' AND deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("query entities in main.db");
        assert_eq!(
            main_entity_count, 1,
            "main.db MUST contain MainOnlyEntity (written via kg pack); got count={main_entity_count}"
        );

        let main_msg_count: i64 = main_conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE kind = 'message'",
                [],
                |row| row.get(0),
            )
            .expect("query notes in main.db");
        assert_eq!(
            main_msg_count, 0,
            "main.db MUST NOT contain any message notes (comm is pinned to second.db); \
             got count={main_msg_count}"
        );

        // --- Verify second.db ---
        let second_conn = Connection::open(&second_path).expect("open second.db");

        let second_msg_count: i64 = second_conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE kind = 'message' AND content = 'SecondOnlyMsg'",
                [],
                |row| row.get(0),
            )
            .expect("query notes in second.db");
        assert_eq!(
            second_msg_count, 2,
            "second.db MUST contain SecondOnlyMsg (dual-write: 1 outbound + 1 inbound copy); \
             got count={second_msg_count}"
        );

        let second_entity_count: i64 = second_conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE name = 'MainOnlyEntity'",
                [],
                |row| row.get(0),
            )
            .expect("query entities in second.db");
        assert_eq!(
            second_entity_count, 0,
            "second.db MUST NOT contain MainOnlyEntity (kg is pinned to main.db); \
             got count={second_entity_count}"
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
    // These cover the enforcement seam itself (finding 1 regression guard).

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
}
