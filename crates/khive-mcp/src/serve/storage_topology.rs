use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use khive_runtime::{
    BackendConfig, BackendId, BackendKind, BlobHydrator, KhiveConfig, RuntimeConfig, StorageBackend,
};

#[cfg(doc)]
use super::mcp_max_readers;
#[cfg(unix)]
use super::preflight_events_socket_for_boot;
use super::{
    build_registry_for_multi_backend_inner,
    build_registry_for_multi_backend_inner_with_max_readers, canonical_backend_path,
    canonical_path_no_side_effects, claimed_backend, disk_guard_numbers,
    open_backend_with_policies, prepare_core_schema_for_boot,
    prepare_single_backend_for_schema_admin, resolve_blob_hydrator_for_boot, targeted_migration,
    validate_disk_guard_topology, DatabaseOverrideConflict, MultiBackendRegistry,
};

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
pub(super) struct ConfiguredStorageTargetPlan {
    pub(super) effective_backends: Vec<BackendConfig>,
    pub(super) full_topology: bool,
}

pub(super) fn effective_backend_configs(
    backends: &[BackendConfig],
    force_memory: bool,
) -> Vec<BackendConfig> {
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
pub(super) enum BackendAliasIdentity {
    /// An existing file can have several distinct canonical hard-link paths.
    #[cfg(any(unix, windows))]
    File(FileIdentity),
    /// A not-yet-created file has only a resolved path to compare.
    Path(PathBuf),
}

pub(super) fn backend_alias_identity(
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
pub(super) fn open_backend_bound_to_alias_identity_with<F>(
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
pub(super) fn open_effective_backends_with<F>(
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

pub(super) fn wal_ceiling_policy_for_backend(
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

pub(super) fn plan_configured_storage_targets(
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
pub(super) fn same_database_target(
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

pub(super) async fn read_applied_schema_version(
    sql: &dyn khive_storage::SqlAccess,
) -> anyhow::Result<u32> {
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
                target: "khive_mcp::serve",
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
                    target: "khive_mcp::serve",
                    "--db {other:?} (or KHIVE_DB) with no declared [[backends]]: the \
                     override names the database directly (ordinary single-backend case)"
                );
            } else {
                tracing::info!(
                    target: "khive_mcp::serve",
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
pub(super) type FileIdentity = khive_db::file_identity::DatabaseFileIdentity;

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct FileIdentity;

pub(super) fn file_identity(path: &std::path::Path) -> Option<FileIdentity> {
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
        select_declared_main_db_path(config, backends);
    }
    Ok(force_memory)
}

/// Apply the declared main choice without re-reading CLI or environment inputs.
pub(super) fn select_declared_main_db_path(config: &mut RuntimeConfig, backends: &[BackendConfig]) {
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

pub(super) struct PreparedStorageTopology {
    pub(super) base_config: RuntimeConfig,
    pub(super) backends: HashMap<String, Arc<StorageBackend>>,
    pub(super) main_backend: Arc<StorageBackend>,
    pub(super) shared_hydrator: Option<Arc<BlobHydrator>>,
}

pub(super) fn declared_backend_db_paths(config: &KhiveConfig) -> Arc<[PathBuf]> {
    config
        .backends
        .iter()
        .filter(|backend| backend.kind == BackendKind::Sqlite)
        .filter_map(|backend| backend.path.as_deref().map(khive_runtime::expand_tilde))
        .collect::<Vec<_>>()
        .into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StorageTopologyPurpose {
    Serving,
    SchemaAdministration,
}

pub(super) async fn schema_admin_requires_blob_hydrator(
    backend: Arc<StorageBackend>,
) -> anyhow::Result<bool> {
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

/// Re-anchor an already resolved split for the declared-backend boot path.
/// The implicit single-backend path must retain its captured split unchanged.
pub(super) fn reanchor_declared_events_split(
    base_config: &mut RuntimeConfig,
    effective_backends: &[BackendConfig],
) {
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
}

pub(super) async fn prepare_configured_storage_topology(
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

    reanchor_declared_events_split(&mut base_config, &effective_backends);

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
