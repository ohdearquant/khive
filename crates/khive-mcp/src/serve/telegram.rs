//! Telegram startup, inbound polling and outbound delivery loops.

#[cfg(all(test, feature = "test-channel-timing"))]
use super::poll_timing_tests;
use super::{
    channel_cycle_wait, channel_ingest_attempt_key, channel_loop_plan,
    cleanup_expired_channel_quarantine, default_inbound_actor_from_env,
    ensure_channel_quarantine_storage, handle_channel_ingest_failure,
    log_inbound_refused_for_read_only_blob, nonblank_env_or, outbox, run_if_authorized, Arc, Args,
    ChannelIngestFailureOutcome, KhiveMcpServer, OUTBOUND_RETRY_BASE,
};

#[cfg(all(test, feature = "test-channel-timing"))]
pub(super) mod heartbeat_test_seam {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    tokio::task_local! {
        pub static COMPLETED: Arc<AtomicUsize>;
    }

    pub fn completed() {
        let _ = COMPLETED.try_with(|count| count.fetch_add(1, Ordering::SeqCst));
    }

    thread_local! {
        static CONSTRUCTIONS: RefCell<Option<Arc<AtomicUsize>>> = const { RefCell::new(None) };
    }

    pub fn observe_construction() -> bool {
        CONSTRUCTIONS.with(|slot| {
            if let Some(count) = slot.borrow().as_ref() {
                count.fetch_add(1, Ordering::SeqCst);
                true
            } else {
                false
            }
        })
    }

    pub fn with_constructor_probe<T>(count: Arc<AtomicUsize>, action: impl FnOnce() -> T) -> T {
        struct Restore(Option<Arc<AtomicUsize>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CONSTRUCTIONS.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(CONSTRUCTIONS.with(|slot| slot.replace(Some(count))));
        action()
    }
}

/// Apply the same independent daemon/runtime admission as the email adapter:
/// both Telegram polling and outbound delivery follow the comm runtime, which
/// holds the message notes and durably marks delivery outcomes.
#[cfg(feature = "channel-telegram")]
pub(super) fn spawn_telegram_channel_loops_if_daemon_registered(
    server: &KhiveMcpServer,
    args: &Args,
    prepared: Option<Arc<khive_channel_telegram::TelegramChannel>>,
    channels: Arc<khive_channel::ChannelRegistry>,
) {
    let admission = channel_loop_plan(server, args);
    if !args.daemon {
        tracing::info!(target: "khive_mcp::serve","telegram channel loops: skipped (client role; daemon owns channel loops)");
        return;
    }
    log_inbound_refused_for_read_only_blob("telegram", admission);
    if !admission.inbound_poll && !admission.outbound_delivery {
        tracing::info!(target: "khive_mcp::serve",
            "telegram channel loops: skipped (assigned comm runtime does not admit writes)"
        );
        return;
    }
    tracing::info!(target: "khive_mcp::serve",
        inbound_poll = admission.inbound_poll,
        outbound_delivery = admission.outbound_delivery,
        "telegram channel loops: applying daemon/runtime admission"
    );
    if let Some(channel) = prepared {
        spawn_telegram_channel_loops(server, admission, channel, channels);
    }
}

/// Spawn the Telegram channel polling + outbox loops if the `channel-telegram`
/// feature is enabled and `KHIVE_TELEGRAM_*` config resolves. Non-fatal: logs
/// a warning and returns on incomplete config. Only call this with the
/// role-and-runtime admission returned by [`channel_loop_plan`] — use
/// [`spawn_telegram_channel_loops_if_daemon_registered`].
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
    tg_ch: Arc<khive_channel_telegram::TelegramChannel>,
    channels: Arc<khive_channel::ChannelRegistry>,
) {
    let poll_channels = channels.clone();
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
            khive_runtime::track_named_background_task("telegram_channel_poll", async move {
                if let Err(error) = ensure_channel_quarantine_storage(&verb_reg_poll).await {
                    tracing::error!(target: "khive_mcp::serve",
                        error = %error,
                        "telegram polling disabled: quarantine storage readiness check failed"
                    );
                    return;
                }
                telegram_poll_loop_registered(
                    tg_ch_poll,
                    poll_channels,
                    verb_reg_poll,
                    ingest_ns_poll,
                    default_actor_poll,
                    khive_runtime::daemon_shutdown_token(),
                )
                .await;
            });
            tracing::info!(target: "khive_mcp::serve","telegram channel polling loop started");
        }
        if admission.outbound_delivery {
            match outbox_runtime {
                Some(rt) => {
                    crate::components::start_channel_component(
                        "telegram-outbound",
                        server,
                        move |ctx| {
                            Box::pin(telegram_outbox_loop_registered(
                                tg_ch_outbox.clone(),
                                channels.clone(),
                                rt.clone(),
                                ingest_ns_outbox.clone(),
                                ctx,
                            ))
                        },
                    );
                    tracing::info!(target: "khive_mcp::serve","telegram channel outbox loop started");
                }
                None => {
                    tracing::error!(target: "khive_mcp::serve",
                        "telegram outbox loop was NOT started: server has no \
                         comm-routed runtime handle, which the loop needs to scan and \
                         mark outbound notes; outbound telegram will not be sent"
                    );
                }
            }
        }
    });
    if !spawned {
        tracing::error!(target: "khive_mcp::serve",
            namespace = %ingest_ns,
            "telegram channel loops NOT started: ingest namespace authorization failed (fail-closed)"
        );
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

// Keep the ordinary configured and slugless passes, then age out the named
// kind-only partition written by older Telegram adapters. The exact extra
// selector retains the handler's existing live/expiry/128-row bound.
async fn cleanup_telegram_quarantine(
    registry: &khive_runtime::VerbRegistry,
    ingest_namespace: &str,
    kind: &str,
    slug: &str,
) -> Result<(), khive_runtime::RuntimeError> {
    cleanup_expired_channel_quarantine(registry, ingest_namespace, kind, slug).await?;
    if slug != kind {
        registry
            .dispatch(
                "comm.cleanup_expired_quarantine",
                serde_json::json!({
                    "namespace": ingest_namespace,
                    "channel_kind": kind,
                    "channel_slug": kind,
                }),
            )
            .await?;
    }
    Ok(())
}

enum TelegramHeartbeatOutcome<'a> {
    Success {
        held_messages: u64,
        quarantined_after_hold: u64,
    },
    Failure(&'a khive_channel::ChannelError),
}

/// Persist only the completed poll observation. This does not govern delivery,
/// offset acknowledgement, retry timing or channel lifecycle events.
async fn record_telegram_heartbeat(
    registry: &khive_runtime::VerbRegistry,
    kind: &str,
    slug: &str,
    observed_at: &chrono::DateTime<chrono::Utc>,
    outcome: TelegramHeartbeatOutcome<'_>,
) {
    use khive_channel::ChannelError;
    use serde_json::json;

    let mut params = json!({
        "namespace": khive_pack_comm::CHANNEL_HEALTH_NAMESPACE,
        "channel_kind": kind,
        "channel_slug": slug,
        "at": observed_at.to_rfc3339(),
    });
    match outcome {
        TelegramHeartbeatOutcome::Success {
            held_messages,
            quarantined_after_hold,
        } => {
            params["outcome"] = json!("success");
            params["held_messages"] = json!(held_messages);
            params["quarantined_after_hold"] = json!(quarantined_after_hold);
        }
        TelegramHeartbeatOutcome::Failure(error) => {
            let class = match error {
                ChannelError::Auth(_) | ChannelError::RetryableAuth(_) => "auth",
                ChannelError::Transport(_)
                | ChannelError::RateLimited { .. }
                | ChannelError::PermanentTransport(_) => "transport",
                ChannelError::Config(_)
                | ChannelError::UnauthorizedSender(_)
                | ChannelError::InvalidEnvelope(_) => "config",
            };
            params["outcome"] = json!("failure");
            params["error_class"] = json!(class);
            params["error_message"] = json!("Telegram poll failed");
        }
    }
    // A long poll has no nominal success cadence. In particular, the five-second
    // error retry is not a poll_interval_secs observation.
    if let Err(error) = registry.dispatch("comm.heartbeat", params).await {
        tracing::warn!(target: "khive_mcp::serve", channel = kind, slug, error = %error,
            "telegram heartbeat could not be persisted");
    }
}

// Keep offset acknowledgement coupled to the polled channel.
#[cfg(feature = "channel-telegram")]
pub(super) trait TelegramPollChannel: khive_channel::Channel {
    fn commit_offset(&self);
}

#[cfg(feature = "channel-telegram")]
impl TelegramPollChannel for khive_channel_telegram::TelegramChannel {
    fn commit_offset(&self) {
        khive_channel_telegram::TelegramChannel::commit_offset(self);
    }
}

/// Background task that polls the Telegram channel via `getUpdates` long
/// polling and ingests new inbound messages via `comm.ingest`. Completed
/// outcomes report a best-effort heartbeat, without escalating retries or
/// lifecycle events — see
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
pub(super) async fn telegram_poll_loop_registered(
    telegram_channel: std::sync::Arc<impl TelegramPollChannel>,
    channels: Arc<khive_channel::ChannelRegistry>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
    default_inbound_actor: String,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use chrono::Utc;
    use serde_json::json;

    // Poll the registered object, and retain the typed owner solely for deferred
    // acknowledgement. Refuse a mismatched object even when its key is identical.
    let expected: Arc<dyn khive_channel::Channel> = telegram_channel.clone();
    let selected = match super::channel_registration::selected_channel(&channels, &expected) {
        Ok(channel) => channel,
        Err(error) => {
            tracing::error!(target: "khive_mcp::serve", %error, "telegram polling disabled");
            return;
        }
    };
    const ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
    let mut failed_polls = std::collections::HashMap::<String, u8>::new();

    loop {
        // This loop polls before it waits, so shutdown is read at the top as
        // well as inside the error backoff below.
        if shutdown.is_cancelled() {
            tracing::info!(target: "khive_mcp::serve","telegram channel polling loop: daemon shutdown observed, stopping");
            return;
        }
        let kind = telegram_channel.kind();
        let slug = telegram_channel.slug();
        if let Err(error) =
            cleanup_telegram_quarantine(&registry, &ingest_namespace, kind, &slug).await
        {
            tracing::warn!(target: "khive_mcp::serve",channel = kind, slug, error = %error,
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
                tracing::info!(target: "khive_mcp::serve","telegram channel polling loop: cancelled in-flight poll");
                return;
            }
            result = selected.poll(Utc::now()) => result,
        };
        let observed_at = Utc::now();
        match polled {
            Ok(envelopes) => {
                let batch_attempt_keys: Vec<String> = envelopes
                    .iter()
                    .filter_map(|env| channel_ingest_attempt_key(kind, env.external_id.as_deref()))
                    .collect();
                let mut all_ingested = true;
                let mut held_messages = 0;
                let mut quarantined_after_hold = 0;
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
                        let outcome = handle_channel_ingest_failure(
                            &registry,
                            &ingest_namespace,
                            (kind, &slug),
                            Some(&default_inbound_actor),
                            &env,
                            &error,
                            &mut failed_polls,
                            // The Telegram adapter declares no retention bound.
                            None,
                        )
                        .await;
                        if !outcome.released() {
                            all_ingested = false;
                            held_messages += 1;
                        }
                        if outcome == ChannelIngestFailureOutcome::QuarantinedAfterHold {
                            quarantined_after_hold += 1;
                        }
                    }
                }

                record_telegram_heartbeat(
                    &registry,
                    kind,
                    &slug,
                    &observed_at,
                    TelegramHeartbeatOutcome::Success {
                        held_messages,
                        quarantined_after_hold,
                    },
                )
                .await;

                if all_ingested {
                    #[cfg(all(test, feature = "test-channel-timing"))]
                    poll_timing_tests::at(poll_timing_tests::Boundary::TelegramBeforeCommit).await;
                    telegram_channel.commit_offset();
                    for key in batch_attempt_keys {
                        failed_polls.remove(&key);
                    }
                } else {
                    tracing::warn!(target: "khive_mcp::serve",
                        channel = kind,
                        "not committing telegram offset: at least one message in this batch \
                         failed comm.ingest; the whole batch will be retried next poll"
                    );
                }
                #[cfg(all(test, feature = "test-channel-timing"))]
                heartbeat_test_seam::completed();
            }
            Err(e) => {
                record_telegram_heartbeat(
                    &registry,
                    kind,
                    &slug,
                    &observed_at,
                    TelegramHeartbeatOutcome::Failure(&e),
                )
                .await;
                tracing::warn!(target: "khive_mcp::serve",
                    channel = telegram_channel.kind(),
                    "telegram channel poll failed: {e}"
                );
                #[cfg(all(test, feature = "test-channel-timing"))]
                heartbeat_test_seam::completed();
                if !channel_cycle_wait(ERROR_BACKOFF, &shutdown).await {
                    tracing::info!(target: "khive_mcp::serve",
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
pub(super) async fn telegram_outbox_loop_registered(
    telegram_channel: Arc<dyn khive_channel::Channel>,
    channels: Arc<khive_channel::ChannelRegistry>,
    runtime: khive_runtime::KhiveRuntime,
    ingest_namespace: String,
    ctx: crate::components::HostContext,
) -> Result<(), crate::components::ComponentError> {
    outbox::validate_loop_channel(telegram_channel.as_ref(), "telegram")?;
    let slug = telegram_channel.slug();
    super::channel_registration::selected_channel(&channels, &telegram_channel)?;
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
pub(super) async fn telegram_outbox_once(
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

/// Resolve the builtin before registry composition, without starting any loop.
pub(super) fn prepare_telegram_channel(
    server: &KhiveMcpServer,
    args: &Args,
) -> Option<Arc<khive_channel_telegram::TelegramChannel>> {
    let admission = channel_loop_plan(server, args);
    if !args.daemon
        || args.daemon_options().lifetime == khive_runtime::daemon::DaemonLifetime::Demand
        || (!admission.inbound_poll && !admission.outbound_delivery)
    {
        return None;
    }
    #[cfg(all(test, feature = "test-channel-timing"))]
    if heartbeat_test_seam::observe_construction() {
        return None;
    }
    match khive_channel_telegram::TelegramChannel::from_env() {
        Ok(channel) => Some(Arc::new(channel)),
        Err(error) => {
            tracing::warn!(target: "khive_mcp::serve", %error,
                "channel-telegram feature is enabled but configuration is incomplete; telegram polling is disabled");
            None
        }
    }
}

#[cfg(test)]
pub(super) fn spawn_telegram_channel_loops_if_daemon(server: &KhiveMcpServer, args: &Args) {
    let prepared = prepare_telegram_channel(server, args);
    let builtins: Vec<Arc<dyn khive_channel::Channel>> = prepared
        .iter()
        .map(|channel| {
            let channel: Arc<dyn khive_channel::Channel> = channel.clone();
            channel
        })
        .collect();
    let channels = super::ServeRegistrations::default()
        .merge_channels(builtins)
        .expect("isolated telegram fixture registry");
    spawn_telegram_channel_loops_if_daemon_registered(server, args, prepared, channels);
}

#[cfg(test)]
pub(super) async fn telegram_poll_loop(
    channel: Arc<impl TelegramPollChannel>,
    registry: khive_runtime::VerbRegistry,
    namespace: String,
    actor: String,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let channels = super::channel_registration::isolated_test_registry(channel.clone());
    telegram_poll_loop_registered(channel, channels, registry, namespace, actor, shutdown).await;
}

#[cfg(test)]
pub(crate) async fn telegram_outbox_loop(
    channel: Arc<dyn khive_channel::Channel>,
    runtime: khive_runtime::KhiveRuntime,
    namespace: String,
    ctx: crate::components::HostContext,
) -> Result<(), crate::components::ComponentError> {
    let channels = super::channel_registration::isolated_test_registry(channel.clone());
    telegram_outbox_loop_registered(channel, channels, runtime, namespace, ctx).await
}
