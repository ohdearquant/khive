//! Email channel startup and shared ingest authorization and quarantine handling.

#[cfg(all(doc, feature = "channel-email"))]
use super::{channel_loop_plan, spawn_email_channel_loops_if_daemon};
#[cfg(feature = "channel-email")]
use super::{channel_outbox_loop, channel_poll_loop, KhiveMcpServer};
#[cfg(all(test, feature = "channel-email"))]
use super::{EMAIL_POLL_TEST_CHANNEL, EMAIL_POLL_TEST_SHUTDOWN};

#[cfg(feature = "channel-email")]
pub(super) fn email_poll_shutdown_token(
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
pub(super) fn spawn_email_channel_loops(
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
                            tracing::error!(target: "khive_mcp::serve",
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
                    tracing::info!(target: "khive_mcp::serve", "email channel polling loop started");
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
                            tracing::info!(target: "khive_mcp::serve", "email channel outbox loop started");
                        }
                        None => {
                            tracing::error!(target: "khive_mcp::serve",
                                "email outbox loop was NOT started: server has no comm-routed \
                                 runtime handle, which the loop needs to scan, claim, and mark \
                                 outbound notes; outbound mail will not be sent"
                            );
                        }
                    }
                }
            });
            if !spawned {
                tracing::error!(target: "khive_mcp::serve",
                    namespace = %ingest_ns,
                    "email channel loops NOT started: ingest namespace authorization failed (fail-closed)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(target: "khive_mcp::serve",
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
pub(super) fn ingest_namespace_from_env() -> String {
    nonblank_env_or("KHIVE_EMAIL_INGEST_NAMESPACE", "local")
}

/// Resolve the default inbound actor for fresh (uncorrelated) email messages.
#[cfg(feature = "channel-email")]
pub(super) fn email_default_inbound_actor_from_env() -> String {
    default_inbound_actor_from_env("KHIVE_EMAIL_DEFAULT_ACTOR", "channel:email")
}

/// Resolve the default inbound actor for fresh (uncorrelated) channel messages.
///
/// Reads the supplied environment variable; falls back to the supplied channel
/// actor when it is unset or blank. Both channel defaults are isolated
/// from the anonymous `local` mailbox.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) fn default_inbound_actor_from_env(actor_variable: &str, fallback: &str) -> String {
    nonblank_env_or(actor_variable, fallback)
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) fn nonblank_env_or(variable: &str, fallback: &str) -> String {
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
pub(super) fn run_if_authorized(
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
pub(super) fn preflight_ingest_namespace(
    ns_str: &str,
    registry: &khive_runtime::VerbRegistry,
) -> bool {
    match khive_runtime::Namespace::parse(ns_str) {
        Ok(ns) => match registry.authorize_namespace(ns) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(target: "khive_mcp::serve",
                    namespace = %ns_str,
                    error = %e,
                    "ingest namespace authorization denied; email polling will not start"
                );
                false
            }
        },
        Err(e) => {
            tracing::error!(target: "khive_mcp::serve",
                namespace = %ns_str,
                error = %e,
                "invalid ingest namespace string; email polling will not start"
            );
            false
        }
    }
}

#[cfg(feature = "channel-email")]
pub(super) const CHANNEL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) const UNKNOWN_INGEST_QUARANTINE_THRESHOLD: u8 = 5;

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChannelIngestDisposition {
    Hold { attempt: Option<u8> },
    Quarantine,
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) fn channel_ingest_attempt_key(
    channel_kind: &str,
    external_id: Option<&str>,
) -> Option<String> {
    external_id.map(|external_id| format!("{channel_kind}:{external_id}"))
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) fn channel_ingest_disposition(
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
pub(super) fn log_channel_quarantine(
    channel_kind: &str,
    classification: khive_runtime::ChannelIngestFailureClass,
    external_id: &str,
) {
    tracing::warn!(target: "khive_mcp::serve",
        channel = channel_kind,
        classification = classification.name(),
        reason = classification.reason(),
        external_id,
        "quarantined inbound message after comm.ingest refusal"
    );
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) async fn ensure_channel_quarantine_storage(
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
pub(super) async fn quarantine_channel_ingest_failure(
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
        tracing::warn!(target: "khive_mcp::serve",
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
pub(super) async fn handle_channel_ingest_failure(
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
            tracing::warn!(target: "khive_mcp::serve",
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
                    tracing::warn!(target: "khive_mcp::serve",
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
pub(super) async fn cleanup_expired_channel_quarantine(
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
pub(super) async fn channel_cycle_wait(
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
pub(super) async fn quarantine_original_may_be_retained(
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
