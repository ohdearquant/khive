//! Build the runtime + server from CLI args and serve over the selected transport.
//!
//! This is the bootstrap that the `kkernel mcp` subcommand drives. Logging is
//! initialized by the binary, not here.

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[path = "serve/channel_poll.rs"]
mod channel_poll;
#[cfg(feature = "channel-email")]
pub(crate) use channel_poll::channel_outbox_loop;
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
use channel_poll::*;

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
#[cfg(all(test, feature = "channel-telegram"))]
pub(crate) use telegram::telegram_outbox_loop;
#[cfg(feature = "channel-telegram")]
use telegram::*;

#[path = "serve/storage_topology.rs"]
mod storage_topology;

pub use storage_topology::{
    build_registry_for_multi_backend, build_registry_for_multi_backend_with_db_anchor,
    build_registry_for_multi_backend_with_db_anchor_and_max_readers,
    build_registry_for_multi_backend_with_db_anchor_and_max_readers_and_claims,
    capture_existing_database_target, configured_storage_check_targets,
    migrate_configured_storage_topology, normalize_redundant_db_override,
    normalize_redundant_db_override_with_source, reject_conflicting_db_override_with_source,
    resolve_pack_backend_config, reverify_reindex_target_identity,
    validate_db_override_against_backends, validate_db_override_against_backends_with_source,
    validate_declared_backend_access_modes, validate_effective_backend_alias_modes,
    validate_reindex_db_target_with_source, validate_wal_ceiling_topology,
    BackendSchemaMigrationStatus, ValidatedReindexTarget,
};

use storage_topology::{
    backend_alias_identity, declared_backend_db_paths, effective_backend_configs,
    open_backend_bound_to_alias_identity_with, prepare_configured_storage_topology,
    read_applied_schema_version, schema_admin_requires_blob_hydrator,
    wal_ceiling_policy_for_backend, BackendAliasIdentity, PreparedStorageTopology,
    StorageTopologyPurpose,
};

#[cfg(all(test, any(unix, windows)))]
use storage_topology::{file_identity, same_database_target};

#[cfg(test)]
use storage_topology::open_effective_backends_with;

#[cfg(all(test, unix))]
use storage_topology::plan_configured_storage_targets;

#[path = "serve/server_build.rs"]
mod server_build;

pub use server_build::{
    build_server, build_server_from_multi_backend_registry, build_server_multi_backend,
    build_server_multi_backend_with_db_anchor, build_server_with_explicit_namespace,
    checkpoint_pool_for, enforce_strict_actor_mode, is_forwarding_client, mark_mcp_host_role,
    mcp_max_readers, WiringSurface,
};

#[cfg(unix)]
pub use server_build::{
    daemon_store_paths, maintenance_store_set, prepare_daemon_store_plan, DaemonStorePlan,
    MaintenanceTopologyRefusal,
};

#[cfg(test)]
pub(crate) use server_build::is_strict_actor_mode;

pub(crate) use server_build::{canonical_path_no_side_effects, should_warn_unattributed};

use server_build::{
    build_server_from_prepared, canonical_backend_path, prepare_server_boot,
    resolve_blob_hydrator_for_boot,
};

#[cfg(all(test, unix))]
use server_build::secondary_file_backed_pools;

#[path = "serve/runtime_config.rs"]
mod runtime_config;

pub use runtime_config::{
    apply_env_output_format, build_single_backend_runtime, config_discovery_db_anchor,
    enable_events_forwarding_for_daemon, install_resolved_blob_store, resolve_runtime_config,
    resolve_runtime_config_with_db_anchor, resolved_actor_disclosure, resolved_database_disclosure,
    resolved_volume_lock_disclosure, resolved_wal_ceiling_disclosure, RuntimeConfigInputs,
};

use runtime_config::{
    build_pack_runtime, build_single_backend_runtime_with_max_readers, opened_diagnostic_backends,
    prepare_core_schema_for_boot, prepare_single_backend_for_schema_admin,
};

#[cfg(test)]
use runtime_config::{open_single_backend, resolve_runtime_wal_ceiling, volume_lock_disclosure};

use disk_policy::{disk_guard_numbers, open_backend_with_policies, validate_disk_guard_topology};
#[path = "serve/runtime_hooks.rs"]
mod runtime_hooks;
#[cfg(test)]
use disk_policy::{open_backend, open_backend_with_wal_ceiling};
#[cfg(unix)]
pub use events_socket_preflight::{
    preflight_events_socket_for_boot, prepare_preflighted_daemon_store_plan,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(all(test, feature = "channel-email"))]
use khive_runtime::runtime_config_from_khive_config;
use khive_runtime::{
    BackendConfig, BackendId, BackendKind, KhiveConfig, KhiveRuntime, OutboundEmailPolicy,
    RuntimeConfig, StorageBackend,
};

use crate::args::{resolve_cli_namespace, Args};
use crate::server::KhiveMcpServer;
use crate::transport::{ServeOptions, TransportRegistry};

#[path = "serve/startup.rs"]
mod startup;

use startup::*;
pub use startup::{
    db_override_refusal_envelope, run, DatabaseOverrideConflict, MultiBackendRegistry,
    DB_OVERRIDE_CONFLICT_CODE,
};

#[cfg(all(test, unix))]
#[path = "serve/demand_startup_tests.rs"]
mod demand_startup_tests;

#[cfg(all(test, feature = "channel-email"))]
#[path = "serve/adr200_inbound_tests.rs"]
mod adr200_inbound_tests;

#[cfg(all(test, feature = "channel-email"))]
thread_local! {
    static EMAIL_POLL_TEST_CHANNEL: std::cell::RefCell<Option<std::sync::Arc<dyn khive_channel::Channel>>> =
        const { std::cell::RefCell::new(None) };
    static EMAIL_POLL_TEST_SHUTDOWN: std::cell::RefCell<Option<tokio_util::sync::CancellationToken>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[path = "serve/channel_ingest.rs"]
mod channel_ingest;
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
use channel_ingest::*;

#[cfg(feature = "channel-email")]
#[path = "serve/email_delivery.rs"]
mod email_delivery;
#[cfg(feature = "channel-email")]
use email_delivery::*;

#[cfg(feature = "channel-email")]
#[path = "serve/email_host.rs"]
mod email_host;

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
    let prepared_background = prepare_host_background_tasks(args, &server).await?;
    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon_with_options_and_boot_guard_and_start(
            server,
            boot_guard,
            args.daemon_options(),
            |server| start_host_background_tasks(args, server, schedule_rt, prepared_background),
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
    start_host_background_tasks(args, &server, schedule_rt, prepared_background);
    serve_with_session_sweep(server, args, registry).await
}

#[path = "serve_targeted_migration.rs"]
mod targeted_migration;

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
    mut base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    max_readers: Option<usize>,
    daemon_claims: Option<&[khive_runtime::daemon::DaemonStoreGuard]>,
) -> anyhow::Result<MultiBackendRegistry> {
    KhiveRuntime::validated_background_limits(&base_config)?;
    base_config.prepare_engines()?;
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
    let default_runtime = KhiveRuntime::try_from_backend(main_backend.clone(), {
        let mut cfg = base_config.clone();
        cfg.backend_id = BackendId::main();
        cfg
    })?
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
            rt_config.disable_embedding_models();
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

    for (pack, policy) in &khive_cfg.packs {
        builder.with_disabled_verbs(pack, &policy.verbs_disabled);
    }
    khive_mounts::register_mounts(&default_runtime, &mut builder).await?;

    let registry = builder
        .build()
        .map_err(|e| anyhow::anyhow!("registry build: {e}"))?;

    runtime_hooks::initialize_runtime_hooks(&registry, &default_runtime, &per_pack_runtimes_local)?;

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
    crate::legacy_quarantine::repair_legacy_quarantine_at_boot(
        &default_runtime,
        &quarantine_sources,
        is_forwarding_client(),
    )
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

#[cfg(all(test, feature = "channel-email", feature = "channel-telegram"))]
#[path = "serve_outbox_health_tests.rs"]
mod outbox_health_tests;

#[cfg(all(test, unix))]
#[path = "serve_reader_pool_tests.rs"]
mod reader_pool_tests;

#[cfg(test)]
#[path = "serve_email_policy_tests.rs"]
mod email_policy_tests;

#[cfg(all(test, feature = "channel-email"))]
#[path = "serve_suppression_mirror_tests.rs"]
mod suppression_mirror_tests;

#[cfg(test)]
#[path = "serve_disk_guard_tests.rs"]
mod disk_guard_tests;
