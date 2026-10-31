//! Server construction, daemon store planning, and shared boot wiring.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use khive_runtime::{
    BackendConfig, BackendId, BackendKind, BlobHydrator, ConnectionPool, KhiveConfig, KhiveRuntime,
    OutputFormat, RuntimeConfig, StorageBackend,
};

use crate::args::{resolve_cli_namespace, Args};
use crate::server::KhiveMcpServer;

#[cfg(doc)]
use super::build_registry_for_multi_backend;
#[cfg(unix)]
use super::validate_effective_backend_alias_modes;
use super::{
    apply_env_output_format, build_registry_for_multi_backend_inner,
    build_registry_for_multi_backend_with_db_anchor,
    build_registry_for_multi_backend_with_db_anchor_and_max_readers_and_claims,
    build_single_backend_runtime_with_max_readers, config_discovery_db_anchor,
    enable_events_forwarding_for_daemon, reject_conflicting_db_override_with_source,
    resolve_runtime_config_with_db_anchor, resolved_database_disclosure,
    resolved_volume_lock_disclosure, resolved_wal_ceiling_disclosure, writable_schedule_runtime,
    MultiBackendRegistry, RuntimeConfigInputs,
};
#[cfg(feature = "channel-email")]
use super::{ingest_namespace_from_env, preflight_ingest_namespace};

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
    explicit.or((forwarding_mcp_client(args) && file_backed).then_some(1))
}

/// Whether an MCP host started with `args` forwards its requests to a daemon
/// instead of serving its stores itself: not the daemon, and not opted out
/// with `KHIVE_NO_DAEMON`. Always false off Unix, which has no daemon.
fn forwarding_mcp_client(args: &Args) -> bool {
    #[cfg(unix)]
    {
        !args.daemon && !khive_runtime::daemon::env_truthy("KHIVE_NO_DAEMON")
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        false
    }
}

/// Whether the last MCP boot in this process was a forwarding client. Unset,
/// every boot runs the work a store-owning process runs.
static FORWARDING_CLIENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record the MCP host role of the boot about to run from `args`: the daemon
/// as the warm index host, and whether this boot is a forwarding client. Every
/// MCP boot path calls this before it builds the server, because boot work
/// that only a store-owning process runs reads these marks. Each call sets the
/// forwarding mark for its own boot, so a daemon or daemonless boot after a
/// forwarding one in the same process runs that work again.
pub fn mark_mcp_host_role(args: &Args) {
    if args.daemon {
        khive_runtime::daemon::mark_warm_index_host();
    }
    FORWARDING_CLIENT.store(
        forwarding_mcp_client(args),
        std::sync::atomic::Ordering::Release,
    );
}

/// Whether [`mark_mcp_host_role`] marked this process a forwarding client.
pub fn is_forwarding_client() -> bool {
    FORWARDING_CLIENT.load(std::sync::atomic::Ordering::Acquire)
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

pub(super) struct PreparedServerBoot {
    pub(super) config: RuntimeConfig,
    pub(super) db_anchor: Option<PathBuf>,
    pub(super) khive_cfg: KhiveConfig,
    pub(super) config_source: Option<PathBuf>,
}

/// Resolve storage topology without opening a database. The daemon takes its
/// store locks from this exact snapshot before `build_server_from_prepared`
/// starts migrations; it must not rediscover HOME/config between the two.
pub(super) fn prepare_server_boot(
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
        config_source: config_source.map(|path| path.to_path_buf()),
    })
}

pub(super) async fn build_server_from_prepared(
    args: &Args,
    prepared: PreparedServerBoot,
    mcp_host: bool,
    daemon_claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
    let PreparedServerBoot {
        config,
        db_anchor,
        khive_cfg,
        config_source,
    } = prepared;

    #[cfg(unix)]
    let (spawn_config, spawn_db) = crate::daemon::select_daemon_spawn_overrides(
        args.config.as_deref().map(std::path::Path::new),
        args.db.as_deref(),
        config_source.as_deref(),
        !khive_cfg.backends.is_empty(),
    );
    #[cfg(not(unix))]
    let _ = config_source;
    #[cfg(not(unix))]
    let (spawn_config, spawn_db): (Option<PathBuf>, Option<String>) = (None, None);

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
                target: "khive_mcp::serve",
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
        let server = KhiveMcpServer::new_with_mounts_and_config(runtime, Some(&khive_cfg))
            .await
            .map(|s| {
                s.with_default_output_format(fmt)
                    .with_daemon_spawn_overrides(spawn_config, spawn_db)
            })
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
    let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None)
        .with_daemon_spawn_overrides(spawn_config, spawn_db);
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
pub(super) fn canonical_backend_path(cfg: &BackendConfig) -> anyhow::Result<Option<PathBuf>> {
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

/// A maintenance-only topology that cannot have one persistent main record.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceTopologyRefusal {
    /// A memory main has persistent stores that must not be silently omitted.
    MemoryMainWithPersistentStores {
        /// Every canonical persistent member discovered in the topology.
        paths: Vec<PathBuf>,
    },
    /// Distinct canonical paths currently name one existing physical store.
    HardLinkedStores {
        /// First canonical member in raw pathname-byte order.
        first: PathBuf,
        /// Later canonical member with the same observed physical identity.
        second: PathBuf,
    },
}

#[cfg(unix)]
impl std::fmt::Display for MaintenanceTopologyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MemoryMainWithPersistentStores { paths } => write!(
                f,
                "maintenance requires a persistent main database; memory main has persistent stores: {paths:?}"
            ),
            Self::HardLinkedStores { first, second } => write!(
                f,
                "maintenance store paths {first:?} and {second:?} name the same physical database; configure one canonical spelling per physical store"
            ),
        }
    }
}

#[cfg(unix)]
impl std::error::Error for MaintenanceTopologyRefusal {}

/// Resolve the complete physical store set without opening stores or creating paths.
///
/// Inputs must already be resolved, including the captured force-memory choice.
/// Declared backends own storage instead of a HOME-derived discovery anchor.
/// This prepares metadata only: it publishes no declaration and grants no lease.
/// Existing members with distinct canonical spellings but equal physical file
/// identities are refused. Missing files remain members by spelling; discovery
/// creates nothing. These metadata observations neither lock pathnames nor close
/// replacement races between the observations or after this function returns.
#[cfg(unix)]
pub fn maintenance_store_set(
    resolved: &RuntimeConfig,
    config: &KhiveConfig,
    force_memory: bool,
) -> anyhow::Result<Option<khive_db::maintenance::StoreSet>> {
    use super::storage_topology::{
        effective_backend_configs, reanchor_declared_events_split, select_declared_main_db_path,
    };
    use std::os::unix::ffi::OsStrExt;

    if force_memory {
        return Ok(None);
    }
    let backends = effective_backend_configs(&config.backends, force_memory);
    let mut normalized = resolved.clone();
    let mut paths = Vec::new();
    if backends.is_empty() {
        if let Some(path) = &normalized.db_path {
            paths.push(canonical_path_no_side_effects(path)?);
        }
    } else {
        anyhow::ensure!(
            backends
                .iter()
                .any(|backend| backend.name == BackendId::MAIN),
            "maintenance topology requires a declared main backend"
        );
        select_declared_main_db_path(&mut normalized, &backends);
        for backend in &backends {
            if backend.kind == BackendKind::Sqlite {
                let path = backend.path.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("sqlite backend {} requires a path field", backend.name)
                })?;
                paths.push(canonical_path_no_side_effects(path)?);
            }
        }
        reanchor_declared_events_split(&mut normalized, &backends);
    }
    if let Some(split) = &normalized.events_split {
        paths.push(canonical_path_no_side_effects(&split.db_path)?);
    }
    if paths.is_empty() {
        return Ok(None);
    }
    let Some(main) = &normalized.db_path else {
        paths.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
        paths.dedup();
        return Err(MaintenanceTopologyRefusal::MemoryMainWithPersistentStores { paths }.into());
    };
    let main = canonical_path_no_side_effects(main)?;
    let stores = khive_db::maintenance::StoreSet::from_canonical(main, paths)?;
    validate_maintenance_store_identities_with(
        &stores,
        khive_db::file_identity::database_file_identity,
    )?;
    Ok(Some(stores))
}

/// Check already sorted, deduplicated members without changing their spellings.
/// The probe is injectable for deterministic absence and I/O-error coverage.
#[cfg(unix)]
pub(super) fn validate_maintenance_store_identities_with<F>(
    stores: &khive_db::maintenance::StoreSet,
    mut identity_for: F,
) -> anyhow::Result<()>
where
    F: FnMut(&std::path::Path) -> std::io::Result<khive_db::file_identity::DatabaseFileIdentity>,
{
    use std::collections::HashMap;

    use khive_db::file_identity::DatabaseFileIdentity;

    let mut paths_by_identity: HashMap<DatabaseFileIdentity, PathBuf> = HashMap::new();
    for path in stores.paths() {
        let identity = match identity_for(path) {
            Ok(identity) => identity,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "cannot inspect maintenance database identity at {}",
                    path.display()
                )));
            }
        };
        if let Some(first) = paths_by_identity.insert(identity, path.clone()) {
            return Err(MaintenanceTopologyRefusal::HardLinkedStores {
                first,
                second: path.clone(),
            }
            .into());
        }
    }
    Ok(())
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
    #[cfg(any(
        feature = "channel-email",
        feature = "channel-telegram",
        feature = "channel-node"
    ))]
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

    #[cfg(any(
        feature = "channel-email",
        feature = "channel-telegram",
        feature = "channel-node"
    ))]
    let server = server.with_channel_outbox_runtime(channel_outbox_runtime);
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    let server = server.with_channel_loop_admission(channel_loop_admission);

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
pub(super) fn secondary_file_backed_pools(
    multi: &MultiBackendRegistry,
) -> Vec<Arc<ConnectionPool>> {
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
pub(super) fn resolve_blob_hydrator_for_boot(
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
                target: "khive_mcp::serve",
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
