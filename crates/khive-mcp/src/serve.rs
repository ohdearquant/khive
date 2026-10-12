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
    checkpoint_pool_for, enforce_strict_actor_mode, mcp_max_readers, WiringSurface,
};

#[cfg(unix)]
pub use server_build::{daemon_store_paths, prepare_daemon_store_plan, DaemonStorePlan};

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
#[path = "serve/email_host.rs"]
mod email_host;

/// Apply the same independent daemon/runtime admission as the email adapter:
/// both Telegram polling and outbound delivery follow the comm runtime, which
/// holds the message notes and durably marks delivery outcomes.
#[cfg(feature = "channel-telegram")]
fn spawn_telegram_channel_loops_if_daemon(server: &KhiveMcpServer, args: &Args) {
    let admission = channel_loop_plan(server, args);
    if !args.daemon {
        tracing::info!("telegram channel loops: skipped (client role; daemon owns channel loops)");
        return;
    }
    log_inbound_refused_for_read_only_blob("telegram", admission);
    if !admission.inbound_poll && !admission.outbound_delivery {
        tracing::info!(
            "telegram channel loops: skipped (assigned comm runtime does not admit writes)"
        );
        return;
    }
    tracing::info!(
        inbound_poll = admission.inbound_poll,
        outbound_delivery = admission.outbound_delivery,
        "telegram channel loops: applying daemon/runtime admission"
    );
    spawn_telegram_channel_loops(server, admission);
}

/// Spawn the Telegram channel polling + outbox loops if the `channel-telegram`
/// feature is enabled and `KHIVE_TELEGRAM_*` config resolves. Non-fatal: logs
/// a warning and returns on incomplete config. Only call this with the
/// role-and-runtime admission returned by [`channel_loop_plan`] — use
/// [`spawn_telegram_channel_loops_if_daemon`].
///
/// Unlike the email adapter, Telegram's poll offset is held in memory inside
/// `TelegramChannel` itself (ADR-056 Amendment 2026-07-05, "Poll offset and
/// restart durability") — there is no per-channel checkpoint/cursor
/// persistence, backoff escalation, or ADR-094 lifecycle-event surface for
/// this adapter; those are email-specific hardening (#605/#606/ADR-094)
/// this ADR explicitly does not require for Telegram's simpler getUpdates
/// durability model.
#[cfg(feature = "channel-telegram")]
fn spawn_telegram_channel_loops(
    server: &KhiveMcpServer,
    admission: crate::server::ChannelLoopAdmission,
) {
    use khive_channel_telegram::TelegramChannel;
    use std::sync::Arc;

    match TelegramChannel::from_env() {
        Ok(tg_ch) => {
            let tg_ch = Arc::new(tg_ch);
            let verb_reg = server.verb_registry_clone();
            let ingest_ns = telegram_ingest_namespace_from_env();
            let default_actor =
                default_inbound_actor_from_env("KHIVE_TELEGRAM_DEFAULT_ACTOR", "telegram:bot");

            let verb_reg_poll = verb_reg.clone();
            let outbox_runtime = server.channel_outbox_runtime_clone();
            let ingest_ns_poll = ingest_ns.clone();
            let default_actor_poll = default_actor.clone();
            let ingest_ns_outbox = ingest_ns.clone();
            let tg_ch_poll = Arc::clone(&tg_ch);
            let tg_ch_outbox = Arc::clone(&tg_ch);

            let spawned = run_if_authorized(&ingest_ns, &verb_reg, || {
                if admission.inbound_poll {
                    khive_runtime::track_named_background_task(
                        "telegram_channel_poll",
                        async move {
                            if let Err(error) =
                                ensure_channel_quarantine_storage(&verb_reg_poll).await
                            {
                                tracing::error!(
                                    error = %error,
                                    "telegram polling disabled: quarantine storage readiness check failed"
                                );
                                return;
                            }
                            telegram_poll_loop(
                                tg_ch_poll,
                                verb_reg_poll,
                                ingest_ns_poll,
                                default_actor_poll,
                                khive_runtime::daemon_shutdown_token(),
                            )
                            .await;
                        },
                    );
                    tracing::info!("telegram channel polling loop started");
                }
                if admission.outbound_delivery {
                    match outbox_runtime {
                        Some(rt) => {
                            crate::components::start_channel_component(
                                "telegram-outbound",
                                server,
                                move |ctx| {
                                    Box::pin(telegram_outbox_loop(
                                        tg_ch_outbox.clone(),
                                        rt.clone(),
                                        ingest_ns_outbox.clone(),
                                        ctx,
                                    ))
                                },
                            );
                            tracing::info!("telegram channel outbox loop started");
                        }
                        None => {
                            tracing::error!(
                                "telegram outbox loop was NOT started: server has no \
                                 comm-routed runtime handle, which the loop needs to scan and \
                                 mark outbound notes; outbound telegram will not be sent"
                            );
                        }
                    }
                }
            });
            if !spawned {
                tracing::error!(
                    namespace = %ingest_ns,
                    "telegram channel loops NOT started: ingest namespace authorization failed (fail-closed)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                "channel-telegram feature is enabled but configuration is incomplete: {e}; \
                 telegram polling is disabled"
            );
        }
    }
}

/// Resolve the target namespace for ingested Telegram messages.
///
/// Reads `KHIVE_TELEGRAM_INGEST_NAMESPACE`; falls back to `"local"` when the
/// variable is unset or blank. Called once at server startup before the poll
/// loop is spawned.
#[cfg(feature = "channel-telegram")]
fn telegram_ingest_namespace_from_env() -> String {
    nonblank_env_or("KHIVE_TELEGRAM_INGEST_NAMESPACE", "local")
}

// Keep the offset acknowledgement coupled to the polled channel. This
// private seam lets the daemon loop be tested with a parked transport.
#[cfg(feature = "channel-telegram")]
trait TelegramPollChannel: khive_channel::Channel {
    fn commit_offset(&self);
}

#[cfg(feature = "channel-telegram")]
impl TelegramPollChannel for khive_channel_telegram::TelegramChannel {
    fn commit_offset(&self) {
        khive_channel_telegram::TelegramChannel::commit_offset(self);
    }
}

/// Background task that polls the Telegram channel via `getUpdates` long
/// polling and ingests new inbound messages via `comm.ingest`. No
/// backoff/heartbeat/lifecycle-event surface — see
/// [`spawn_telegram_channel_loops`]'s doc comment for why this is a
/// deliberately smaller loop than `channel_poll_loop`.
///
/// The Bot API `getUpdates` call itself blocks server-side for the
/// connector's long-poll timeout awaiting new updates (ADR-056 Amendment
/// 2026-07-05 requires long polling, not short polling), so the success path
/// adds no extra sleep between requests — the long poll paces the loop.
/// Only the error path sleeps, so a failing Bot API does not hot-loop.
///
/// A fetched batch's offset is committed (acknowledged to Telegram) only
/// after every authorized envelope in it durably ingests via `comm.ingest`
/// — mirrors the IMAP cursor-commit discipline at `channel_poll_loop`
/// without importing its IMAP-specific machinery (issue #113).
#[cfg(feature = "channel-telegram")]
async fn telegram_poll_loop(
    telegram_channel: std::sync::Arc<impl TelegramPollChannel>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
    default_inbound_actor: String,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use chrono::Utc;
    use serde_json::json;

    const ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
    let mut unknown_ingest_attempts = std::collections::HashMap::<String, u8>::new();

    loop {
        // This loop polls before it waits, so shutdown is read at the top as
        // well as inside the error backoff below.
        if shutdown.is_cancelled() {
            tracing::info!("telegram channel polling loop: daemon shutdown observed, stopping");
            return;
        }
        let kind = telegram_channel.kind();
        let slug = telegram_channel.slug();
        if let Err(error) =
            cleanup_expired_channel_quarantine(&registry, &ingest_namespace, kind, &slug).await
        {
            tracing::warn!(channel = kind, slug, error = %error,
                "quarantine retention cleanup failed; holding telegram poll");
            if !channel_cycle_wait(ERROR_BACKOFF, &shutdown).await {
                return;
            }
            continue;
        }
        #[cfg(all(test, feature = "test-channel-timing"))]
        poll_timing_tests::at(poll_timing_tests::Boundary::TelegramBeforePoll).await;
        // Dropping getUpdates leaves the confirmed offset unchanged; an
        // abandoned batch is requested again when polling resumes.
        let polled = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                tracing::info!("telegram channel polling loop: cancelled in-flight poll");
                return;
            }
            result = telegram_channel.poll(Utc::now()) => result,
        };
        match polled {
            Ok(envelopes) => {
                let batch_attempt_keys: Vec<String> = envelopes
                    .iter()
                    .filter_map(|env| channel_ingest_attempt_key(kind, env.external_id.as_deref()))
                    .collect();
                let mut all_ingested = true;
                for env in envelopes {
                    let params = json!({
                        "namespace": ingest_namespace,
                        "from": env.from.clone(),
                        "to": env.to.clone(),
                        "content": env.content.clone(),
                        "channel_kind": kind,
                        "channel_slug": &slug,
                        "external_id": env.external_id.clone(),
                        "sent_at": env.sent_at.as_ref().map(|ts| ts.to_rfc3339()),
                        "default_inbound_actor": default_inbound_actor,
                    });
                    if let Err(error) = registry.dispatch("comm.ingest", params).await {
                        let handled = handle_channel_ingest_failure(
                            &registry,
                            &ingest_namespace,
                            (kind, &slug),
                            Some(&default_inbound_actor),
                            &env,
                            &error,
                            &mut unknown_ingest_attempts,
                            // The Telegram adapter declares no retention bound.
                            None,
                        )
                        .await;
                        if !handled {
                            all_ingested = false;
                        }
                    }
                }

                if all_ingested {
                    #[cfg(all(test, feature = "test-channel-timing"))]
                    poll_timing_tests::at(poll_timing_tests::Boundary::TelegramBeforeCommit).await;
                    telegram_channel.commit_offset();
                    for key in batch_attempt_keys {
                        unknown_ingest_attempts.remove(&key);
                    }
                } else {
                    tracing::warn!(
                        channel = kind,
                        "not committing telegram offset: at least one message in this batch \
                         failed comm.ingest; the whole batch will be retried next poll"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    channel = telegram_channel.kind(),
                    "telegram channel poll failed: {e}"
                );
                if !channel_cycle_wait(ERROR_BACKOFF, &shutdown).await {
                    tracing::info!(
                        "telegram channel polling loop: daemon shutdown observed, stopping"
                    );
                    return;
                }
            }
        }
    }
}

/// Background task that delivers undelivered outbound notes addressed to a
/// `telegram:` recipient every 5 seconds. Mirrors `channel_outbox_loop`'s
/// note-scan/send/mark-delivered shape without the Message-ID minting logic
/// (Telegram has no RFC 822 Message-ID concept).
#[cfg(feature = "channel-telegram")]
pub(crate) async fn telegram_outbox_loop(
    telegram_channel: Arc<dyn khive_channel::Channel>,
    runtime: khive_runtime::KhiveRuntime,
    ingest_namespace: String,
    ctx: crate::components::HostContext,
) -> Result<(), crate::components::ComponentError> {
    outbox::validate_loop_channel(telegram_channel.as_ref(), "telegram")?;
    let slug = telegram_channel.slug();
    let mut channels = khive_channel::ChannelRegistry::new();
    channels.register(telegram_channel);
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
            outbox::OutboxPolicy::Telegram(std::marker::PhantomData),
            &runtime,
            &namespace,
            ctx.cancellation(),
            &mut pause_until,
        )
        .await?;
        ctx.heartbeat();
    }
}

#[cfg(all(test, feature = "channel-email", feature = "channel-telegram"))]
async fn telegram_outbox_once(
    telegram_channel: &dyn khive_channel::Channel,
    runtime: &khive_runtime::KhiveRuntime,
    namespace: &khive_runtime::Namespace,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<(), crate::components::ComponentError> {
    let mut pause_until = None;
    outbox::outbox_once(
        outbox::OutboxChannels::Single(telegram_channel),
        outbox::OutboxPolicy::Telegram(std::marker::PhantomData),
        runtime,
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
