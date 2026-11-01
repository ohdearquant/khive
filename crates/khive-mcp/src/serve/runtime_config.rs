//! Runtime construction and serve-time configuration resolution.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use khive_runtime::{
    config_from_env, parse_pack_list, runtime_config_from_khive_config, BackendConfig, BackendId,
    BackendKind, BlobHydrator, KhiveConfig, KhiveRuntime, OpenedDiagnosticBackend,
    OutboundEmailPolicy, OutputFormat, RuntimeConfig, StorageBackend,
};

#[cfg(unix)]
use super::preflight_events_socket_for_boot;
#[cfg(doc)]
use super::{build_server, validate_db_override_against_backends};
use super::{
    claimed_backend, declared_backend_db_paths, gate_boot_disclosure,
    resolve_blob_hydrator_for_boot, schema_admin_requires_blob_hydrator,
    validate_wal_ceiling_topology,
};

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

pub(super) async fn build_single_backend_runtime_with_max_readers(
    mut config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    max_readers: Option<usize>,
    daemon_claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<KhiveRuntime> {
    KhiveRuntime::validated_background_limits(&config)?;
    config.prepare_engines()?;
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
    crate::legacy_quarantine::repair_legacy_quarantine_at_boot(
        &runtime,
        &[("main", runtime.backend())],
        super::is_forwarding_client(),
    )
    .await?;
    Ok(runtime)
}

pub(super) fn open_single_backend(
    config: &mut RuntimeConfig,
    max_readers: Option<usize>,
) -> anyhow::Result<StorageBackend> {
    claimed_backend::open_single_backend(config, max_readers, None)
}

pub(super) async fn prepare_single_backend_for_schema_admin(
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

pub(super) async fn prepare_core_schema_for_boot(
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
pub(super) fn opened_diagnostic_backends(
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
pub(super) fn build_pack_runtime(
    backend: Arc<StorageBackend>,
    backend_name: &str,
    mut rt_config: RuntimeConfig,
    main_backend: &Arc<StorageBackend>,
    main_runtime: &KhiveRuntime,
    declared_backend_db_paths: Arc<[PathBuf]>,
    diagnostic_backends: Arc<[OpenedDiagnosticBackend]>,
) -> KhiveRuntime {
    // Map declarations are loaded once by the main runtime. Avoid repeating
    // their filesystem checks and rejection logs for each pack runtime.
    rt_config.code_map_root = None;
    rt_config.code_maps.clear();
    // Every pack runtime carries main's embedder wiring for core(): a
    // main-assigned pack has no core pointer, but with `no_embed` its own
    // registry is empty and core-routed concept writes must still embed.
    let rt = KhiveRuntime::from_backend_in_domain(
        backend,
        rt_config,
        main_runtime
            .background_root()
            .expect("main runtime owns its background domain"),
    )
    .with_outbound_email_policy(main_runtime.outbound_email_policy().clone())
    .with_declared_backend_db_paths(declared_backend_db_paths)
    .with_diagnostic_backends(diagnostic_backends)
    .with_diagnostic_observer_from(main_runtime)
    .with_core_embedders_from(main_runtime)
    .with_code_map_registry_from(main_runtime);
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

pub(super) fn volume_lock_disclosure(
    lock_dir: Option<&std::path::Path>,
    harness_scoped: bool,
) -> String {
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
                    target: "khive_mcp::serve",
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
                    target: "khive_mcp::serve",
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
pub(super) fn resolve_runtime_wal_ceiling(
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
            resolved.disable_embedding_models();
            Ok(resolved)
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

#[cfg(test)]
mod code_map_registry_tests {
    use super::*;

    #[test]
    fn pack_runtime_shares_the_main_code_map_registration() {
        let fixture = tempfile::tempdir().expect("fixture");
        let map_root = fixture.path().join("maps");
        std::fs::create_dir(&map_root).expect("map root");
        let map_path = map_root.join("workspace.db");
        std::fs::write(&map_path, b"map data").expect("map file");
        let db_path = fixture.path().join("main.db");
        let main_backend =
            Arc::new(StorageBackend::sqlite_for_test(&db_path).expect("main backend"));
        let config = RuntimeConfig {
            db_path: Some(db_path),
            code_map_root: Some(map_root.display().to_string()),
            code_maps: vec![khive_runtime::engine_config::CodeMapConfig {
                name: Some("workspace".to_string()),
                path: Some(map_path.display().to_string()),
                source_root: Some(fixture.path().display().to_string()),
                ..khive_runtime::engine_config::CodeMapConfig::default()
            }],
            ..RuntimeConfig::default()
        };
        let main_runtime = KhiveRuntime::from_backend(Arc::clone(&main_backend), config.clone());
        let pack_runtime = build_pack_runtime(
            Arc::clone(&main_backend),
            BackendId::MAIN,
            config,
            &main_backend,
            &main_runtime,
            Vec::new().into(),
            Vec::new().into(),
        );

        let main_generation = main_runtime
            .code_map_registry()
            .resolve("workspace")
            .expect("main registration")
            .generation();
        let pack_generation = pack_runtime
            .code_map_registry()
            .resolve("workspace")
            .expect("pack registration")
            .generation();
        assert_eq!(pack_generation, main_generation);
    }
}

#[cfg(test)]
mod background_limit_host_tests {
    use super::*;

    #[tokio::test]
    async fn raw_single_host_refuses_invalid_limits_before_creating_storage() {
        for (warm, maintenance) in [(0, 2), (3, 0), (usize::MAX, 2), (3, usize::MAX)] {
            let fixture = tempfile::tempdir().expect("isolated host target");
            let db_path = fixture.path().join("must-not-open.db");
            // Keep the opener lock-directory prerequisite independent of the environment so
            // removing early validation cannot stop at a missing lock directory.
            let lock_fixture = tempfile::tempdir().expect("isolated volume lock directory");
            let config = RuntimeConfig {
                db_path: Some(db_path.clone()),
                volume_lock_dir: Some(lock_fixture.path().to_path_buf()),
                warm_concurrency: warm,
                maintenance_concurrency: maintenance,
                ..RuntimeConfig::no_embeddings()
            };
            assert!(!db_path.exists());
            assert!(std::fs::read_dir(fixture.path()).unwrap().next().is_none());
            assert!(std::fs::read_dir(lock_fixture.path())
                .unwrap()
                .next()
                .is_none());
            // Exercise the raw public host with caller-built config, bypassing
            // loaded-TOML validation. No opener substitute or fake store is used.
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                build_single_backend_runtime(config, &KhiveConfig::default()),
            )
            .await
            .expect("invalid raw host config did not refuse within wall-clock budget");
            let error = match result {
                Ok(_) => panic!("invalid raw host background limits were accepted"),
                Err(error) => error,
            };
            assert!(
                matches!(
                    error.downcast_ref::<khive_runtime::RuntimeError>(),
                    Some(khive_runtime::RuntimeError::InvalidInput(_))
                ),
                "expected actual limit refusal, got {error:#}"
            );
            assert!(
                !db_path.exists(),
                "limit refusal occurred after the target was opened"
            );
            assert!(
                std::fs::read_dir(fixture.path()).unwrap().next().is_none(),
                "limit refusal left database, sidecar, or boot artifacts"
            );
            assert!(
                std::fs::read_dir(lock_fixture.path())
                    .unwrap()
                    .next()
                    .is_none(),
                "limit refusal reached volume-lock acquisition"
            );
        }
    }

    #[tokio::test]
    async fn raw_multi_host_refuses_invalid_limits_before_creating_storage() {
        for (warm, maintenance) in [(0, 2), (3, 0), (usize::MAX, 2), (3, usize::MAX)] {
            let fixture = tempfile::tempdir().expect("isolated multi-backend targets");
            let lock_fixture = tempfile::tempdir().expect("isolated volume lock directory");
            let main_path = fixture.path().join("main.db");
            let secondary_path = fixture.path().join("notes.db");
            let config = RuntimeConfig {
                db_path: Some(main_path.clone()),
                volume_lock_dir: Some(lock_fixture.path().to_path_buf()),
                warm_concurrency: warm,
                maintenance_concurrency: maintenance,
                packs: Vec::new(),
                events_split: None,
                wal_ceiling_bytes: 0,
                wal_ceiling_configured_bytes: 0,
                wal_ceiling_source: Default::default(),
                wal_ceiling_env_raw: None,
                disk_guard_environment: khive_db::DiskGuardEnvironment {
                    reserve: None,
                    legacy_reserve: None,
                    deadline: None,
                },
                ..RuntimeConfig::no_embeddings()
            };
            let declared_backend = |name: &str, path: PathBuf| BackendConfig {
                name: name.to_owned(),
                kind: BackendKind::Sqlite,
                path: Some(path),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: Some(0),
                disk_reserve_bytes: Some(0),
                disk_guard_deadline_ms: Some(100),
            };
            let khive_cfg = KhiveConfig {
                backends: vec![
                    declared_backend("main", main_path.clone()),
                    declared_backend("notes", secondary_path.clone()),
                ],
                ..KhiveConfig::default()
            };
            assert!(std::fs::read_dir(fixture.path()).unwrap().next().is_none());
            assert!(std::fs::read_dir(lock_fixture.path())
                .unwrap()
                .next()
                .is_none());
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                super::super::build_registry_for_multi_backend_inner_with_max_readers(
                    config, &khive_cfg, None, None, None,
                ),
            )
            .await
            .expect("invalid multi-backend limits did not refuse within the wall-clock budget");
            let error = match result {
                Ok(_) => panic!("invalid multi-backend background limits were accepted"),
                Err(error) => error,
            };
            assert!(
                matches!(
                    error.downcast_ref::<khive_runtime::RuntimeError>(),
                    Some(khive_runtime::RuntimeError::InvalidInput(_))
                ),
                "expected actual limit refusal, got {error:#}"
            );
            assert!(!main_path.exists() && !secondary_path.exists());
            assert!(
                std::fs::read_dir(fixture.path()).unwrap().next().is_none(),
                "limit refusal left database, sidecar, or boot artifacts"
            );
            assert!(
                std::fs::read_dir(lock_fixture.path())
                    .unwrap()
                    .next()
                    .is_none(),
                "limit refusal reached volume-lock acquisition"
            );
        }
    }
}
