//! Channel polling, cursor and heartbeat updates, and outbound delivery loops.

#[cfg(feature = "channel-email")]
use super::{
    channel_cycle_wait, channel_ingest_attempt_key, cleanup_expired_channel_quarantine,
    handle_channel_ingest_failure, outbox, quarantine_original_may_be_retained, Arc,
    CHANNEL_POLL_INTERVAL,
};
#[cfg(all(test, feature = "channel-email"))]
use super::{poll_timing_tests, OutboundEmailPolicy};

/// Background task that polls all registered channels every 5 seconds and
/// ingests new inbound messages via `comm.ingest`.
///
/// #605: the 5s cadence is the happy-path default only. A connect/auth
/// failure (classified by `khive_channel_email::is_backoff_eligible`) starts
/// a per-channel-kind jittered exponential backoff (`ImapBackoff`,
/// 5s -> 10s -> ... capped at ~10min) instead of retrying flat every 5s; a
/// success resets that channel's backoff to base, and the loop returns to
/// the normal 5s cadence. This is process-side pressure relief on top of the
/// per-credential single-flight guard inside `LiveImap` itself. Eligible
/// failures log via [`log_eligible_poll_failure`]: `warn!` only on an
/// escalation edge, `debug!` while riding the same capped step — never one
/// `warn!` per retry.
///
/// Only compiled when the `channel-email` feature is enabled.
#[cfg(feature = "channel-email")]
pub(super) async fn channel_poll_loop(
    channels: std::sync::Arc<khive_channel::ChannelRegistry>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
    default_inbound_actor: String,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use base64::Engine as _;
    use chrono::{DateTime, Utc};
    use khive_channel_email::{is_backoff_eligible, ImapBackoff};
    use serde_json::json;
    use std::collections::HashMap;
    // Per-channel bootstrap "since" floor (issue #449). This
    // only feeds the date-based SINCE search used while a channel has no
    // committed UID high-water yet (first-ever poll, or a UIDVALIDITY
    // reset); once a checkpoint has a high-water, polling is UID-ranged and
    // this floor is unused for that channel. Each entry only advances to the
    // poll tick's timestamp once that channel's full cycle -- cursor_get,
    // poll_page, every comm.ingest, and cursor_commit -- succeeds this tick.
    // Advancing it unconditionally (as a shared `last_poll` timestamp used
    // to) would drop the earlier floor on any bootstrap-cycle failure, and
    // if that failure spans a calendar-day boundary the next checkpoint-less
    // poll's SINCE clause would use the newer date, permanently skipping the
    // previous day's uncommitted mail.
    let mut bootstrap_since: HashMap<(String, String), DateTime<Utc>> = HashMap::new();
    // One backoff state per (kind, slug) — i.e. per credential (#606).
    // Keying by kind alone would throttle a
    // second same-kind credential (e.g. a second mailbox) whenever the first
    // one's connection fails, even though the two are independent
    // credentials with independent connectivity.
    let mut backoffs: HashMap<(String, String), ImapBackoff> = HashMap::new();
    // ADR-094: tracks the error class of the most recent unresolved failure
    // per (kind, slug), so `ChannelPollFailed` fires once per failure episode
    // (first failure since success, or a change in error class) rather than
    // once per retry. Cleared on every success.
    let mut last_error_class: HashMap<(String, String), &'static str> = HashMap::new();
    // Unknown ingest failures are bounded per external message ID. Entries
    // remain pinned through quarantine until the page cursor itself commits,
    // so a cursor-commit failure cannot restart the five-attempt wait.
    let mut unknown_ingest_attempts: HashMap<String, u8> = HashMap::new();
    let mut next_interval = CHANNEL_POLL_INTERVAL;
    let event_store = registry.event_store();
    // Captured before the loop's first sleep (issue #449 follow-up).
    // A channel's very first bootstrap floor must reflect
    // when the daemon actually started, not whenever its first tick happens
    // to fire: `tokio::time::sleep` below runs before any polling, so
    // computing `now` after it (as the loop used to) can land on the far
    // side of a calendar-day boundary the daemon started before. Every
    // vacant `bootstrap_since` entry -- on tick 1 or any later tick a
    // channel is first seen on -- uses this single startup timestamp
    // instead of that tick's own `now`.
    let startup_since = Utc::now();

    loop {
        if !channel_cycle_wait(next_interval, &shutdown).await {
            tracing::info!(target: "khive_mcp::serve", "email channel polling loop: daemon shutdown observed, stopping");
            return;
        }
        next_interval = CHANNEL_POLL_INTERVAL;

        let now = Utc::now();

        for (kind, slug, channel) in channels.iter() {
            let backoff_key = (kind.to_string(), slug.to_string());
            let since = *bootstrap_since
                .entry(backoff_key.clone())
                .or_insert(startup_since);
            // Set once this channel's cycle durably completes (a fresh
            // commit, or nothing new to commit); gates whether `since`
            // advances past this tick's `now` for next time.
            let mut bootstrap_floor_advances = false;

            append_channel_lifecycle_event(
                event_store.as_ref(),
                khive_types::EventKind::ChannelPollStarted,
                khive_storage::ChannelPollStartedPayload {
                    channel_kind: kind.to_string(),
                    channel_slug: slug.to_string(),
                    since_rfc3339: since.to_rfc3339(),
                },
            )
            .await;

            // One bounded expiry page per credential per tick, including
            // empty polls. A failure must hold this cycle before a success
            // heartbeat or cursor advance can be recorded.
            if let Err(error) =
                cleanup_expired_channel_quarantine(&registry, &ingest_namespace, kind, slug).await
            {
                tracing::warn!(target: "khive_mcp::serve", channel = kind, slug, error = %error,
                    "quarantine retention cleanup failed; holding channel poll");
                record_channel_heartbeat(
                    &registry,
                    kind,
                    slug,
                    HeartbeatOutcome::Failure {
                        class: "retention",
                        message: error.to_string(),
                    },
                    event_store.as_ref(),
                )
                .await;
                continue;
            }

            // Durable checkpoint path (issue #449): cursor_get -> poll_page ->
            // every comm.ingest -> cursor_commit, committing only when the
            // whole page durably ingested. A cursor_get failure means we
            // cannot trust what progress to poll from, so this channel is
            // skipped for the cycle rather than risk polling from an empty
            // checkpoint and silently discarding durable state.
            let checkpoint = match load_channel_cursor(&registry, kind, slug).await {
                Ok(cp) => cp,
                Err(e) => {
                    tracing::warn!(target: "khive_mcp::serve",
                        channel = kind,
                        "comm.cursor_get failed; skipping this channel's poll this cycle: {e}"
                    );
                    continue;
                }
            };

            #[cfg(test)]
            poll_timing_tests::at(poll_timing_tests::Boundary::EmailBeforePoll).await;
            // A transport poll may outlast the daemon drain budget. No
            // cursor or bootstrap floor advances until its page is ingested.
            let polled = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::info!(target: "khive_mcp::serve", "email channel polling loop: cancelled in-flight poll");
                    return;
                }
                result = channel.poll_page(since, checkpoint.as_ref()) => result,
            };
            match polled {
                Ok(page) => {
                    let prior_attempt =
                        backoffs.get(&backoff_key).map(|b| b.attempt()).unwrap_or(0);
                    if let Some(backoff) = backoffs.get_mut(&backoff_key) {
                        backoff.record_success();
                    }
                    last_error_class.remove(&backoff_key);

                    // Only a recovery from a prior failure/backoff episode is
                    // an interesting lifecycle transition — an unbroken
                    // string of healthy polls never had ChannelPollFailed
                    // fire, so there is nothing to report recovering from.
                    if prior_attempt > 0 {
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelPollSucceeded,
                            khive_storage::ChannelPollSucceededPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                envelope_count: page.envelopes.len(),
                                previous_backoff_attempt: prior_attempt,
                            },
                        )
                        .await;
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelBackoffReset,
                            khive_storage::ChannelBackoffResetPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                previous_backoff_attempt: prior_attempt,
                            },
                        )
                        .await;
                    }

                    record_channel_heartbeat(
                        &registry,
                        kind,
                        slug,
                        HeartbeatOutcome::Success,
                        event_store.as_ref(),
                    )
                    .await;

                    // Every envelope in the page must durably ingest before
                    // the cursor is allowed to advance past it (issue #449):
                    // a partial-page ingest failure must leave
                    // the checkpoint untouched so the next poll re-selects
                    // the whole page -- comm.ingest's `INSERT OR IGNORE`
                    // dedup then skips re-storing the messages that already
                    // succeeded, and only the failed one is retried.
                    let page_attempt_keys: Vec<String> = page
                        .envelopes
                        .iter()
                        .filter_map(|env| {
                            channel_ingest_attempt_key(kind, env.external_id.as_deref())
                        })
                        .collect();
                    let mut page_fully_ingested = true;
                    for env in page.envelopes {
                        let mut metadata = env.metadata.clone();
                        if kind == "email"
                            && metadata.get("quarantined").map(String::as_str) == Some("true")
                        {
                            let Some(replay) = env.quarantine_replay.as_ref() else {
                                tracing::warn!(target: "khive_mcp::serve",
                                    channel = kind,
                                    external_id = env.external_id.as_deref(),
                                    "quarantined email has no original-byte replay; holding channel progress"
                                );
                                page_fully_ingested = false;
                                continue;
                            };
                            let retain_original = match quarantine_original_may_be_retained(
                                &registry,
                                &ingest_namespace,
                                channel.quarantine_retention_limit(),
                            )
                            .await
                            {
                                Ok(retain) => retain,
                                Err(error) => {
                                    tracing::warn!(target: "khive_mcp::serve",
                                        channel = kind,
                                        external_id = env.external_id.as_deref(),
                                        %error,
                                        "could not read the retained quarantine count; holding channel progress"
                                    );
                                    page_fully_ingested = false;
                                    continue;
                                }
                            };
                            if !retain_original {
                                tracing::warn!(target: "khive_mcp::serve",
                                    channel = kind,
                                    external_id = env.external_id.as_deref(),
                                    limit = channel.quarantine_retention_limit(),
                                    "quarantine retention limit reached; recording the message without its original bytes"
                                );
                                metadata.insert(
                                    "quarantine_original_retained".to_string(),
                                    "false".to_string(),
                                );
                                metadata.insert(
                                    "quarantine_original_not_retained_reason".to_string(),
                                    "retention-limit".to_string(),
                                );
                            } else {
                                let put = registry
                                    .dispatch(
                                        "blob.put",
                                        json!({
                                            "bytes": base64::engine::general_purpose::STANDARD
                                                .encode(&replay.bytes)
                                        }),
                                    )
                                    .await;
                                let content_ref = match put {
                                    Ok(result) => {
                                        match result.get("content_ref").and_then(|v| v.as_str()) {
                                            Some(content_ref) => content_ref.to_string(),
                                            None => {
                                                tracing::warn!(target: "khive_mcp::serve",
                                                    channel = kind,
                                                    external_id = env.external_id.as_deref(),
                                                    "blob.put returned no quarantine content reference; holding channel progress"
                                                );
                                                page_fully_ingested = false;
                                                continue;
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(target: "khive_mcp::serve",
                                            channel = kind,
                                            external_id = env.external_id.as_deref(),
                                            %error,
                                            "failed to publish quarantined email original; holding channel progress"
                                        );
                                        page_fully_ingested = false;
                                        continue;
                                    }
                                };
                                metadata.insert("quarantine_content_ref".to_string(), content_ref);
                            }
                        }
                        let params = json!({
                            "namespace": ingest_namespace,
                            "from": env.from.clone(),
                            "to": env.to.clone(),
                            "content": env.content.clone(),
                            "subject": env.subject.clone(),
                            "channel_kind": kind,
                            "channel_slug": slug,
                            "external_id": env.external_id.clone(),
                            "legacy_external_id": env.legacy_external_id.clone(),
                            "sent_at": env.sent_at.as_ref().map(|ts| ts.to_rfc3339()),
                            "correlation_external_id": env.correlation_external_id.clone(),
                            "default_inbound_actor": default_inbound_actor,
                            "wire_message_id": env.wire_message_id.clone(),
                            "wire_references": env.wire_references.clone(),
                            "metadata": metadata,
                        });
                        if let Err(error) = registry.dispatch("comm.ingest", params).await {
                            let handled = handle_channel_ingest_failure(
                                &registry,
                                &ingest_namespace,
                                (kind, slug),
                                Some(&default_inbound_actor),
                                &env,
                                &error,
                                &mut unknown_ingest_attempts,
                                channel.quarantine_retention_limit(),
                            )
                            .await;
                            if !handled {
                                page_fully_ingested = false;
                            }
                        }
                    }

                    if page_fully_ingested {
                        match page.next_checkpoint {
                            Some(next_checkpoint) => {
                                #[cfg(test)]
                                poll_timing_tests::at(
                                    poll_timing_tests::Boundary::EmailBeforeCommit,
                                )
                                .await;
                                match commit_channel_cursor(&registry, kind, slug, &next_checkpoint)
                                    .await
                                {
                                    Ok(()) => bootstrap_floor_advances = true,
                                    Err(e) => {
                                        tracing::warn!(target: "khive_mcp::serve",
                                            channel = kind,
                                            "comm.cursor_commit failed; progress not durably \
                                             advanced, next poll will retry: {e}"
                                        );
                                    }
                                }
                            }
                            // Nothing new to commit this tick is not a
                            // failure -- safe to advance the bootstrap floor.
                            None => bootstrap_floor_advances = true,
                        }
                        if bootstrap_floor_advances {
                            for key in page_attempt_keys {
                                unknown_ingest_attempts.remove(&key);
                            }
                        }
                    } else {
                        tracing::warn!(target: "khive_mcp::serve",
                            channel = kind,
                            "not committing IMAP cursor: at least one message in this page \
                             failed comm.ingest; the whole page will be retried next poll"
                        );
                    }
                }
                Err(e) => {
                    let class = channel_error_class(&e);
                    record_channel_heartbeat(
                        &registry,
                        kind,
                        slug,
                        HeartbeatOutcome::Failure {
                            class,
                            message: e.to_string(),
                        },
                        event_store.as_ref(),
                    )
                    .await;

                    // First failure since success or since the error class
                    // changed — a run of identical retries at the same class
                    // does not re-fire this event.
                    if last_error_class.get(&backoff_key) != Some(&class) {
                        last_error_class.insert(backoff_key.clone(), class);
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelPollFailed,
                            khive_storage::ChannelPollFailedPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                error_class: class.to_string(),
                                error_message: e.to_string(),
                            },
                        )
                        .await;
                    }

                    if is_backoff_eligible(&e) {
                        let backoff = backoffs.entry(backoff_key).or_default();
                        let tick = backoff.record_failure();
                        log_eligible_poll_failure(kind, &e, &tick);
                        next_interval = next_interval.max(tick.delay);

                        if tick.should_warn {
                            append_channel_lifecycle_event(
                                event_store.as_ref(),
                                khive_types::EventKind::ChannelBackoffArmed,
                                khive_storage::ChannelBackoffArmedPayload {
                                    channel_kind: kind.to_string(),
                                    channel_slug: slug.to_string(),
                                    attempt: tick.attempt,
                                    step_ms: tick.step.as_millis() as u64,
                                    delay_ms: tick.delay.as_millis() as u64,
                                },
                            )
                            .await;
                        }
                    } else {
                        // Non-eligible failures (config/gate errors, never
                        // produced by poll/connect in practice) are not
                        // connectivity pressure, so they keep the pre-#605
                        // warn-every-retry behavior at the normal cadence.
                        tracing::warn!(target: "khive_mcp::serve", channel = kind, "channel poll failed: {e}");
                    }
                }
            }

            if bootstrap_floor_advances {
                bootstrap_since.insert((kind.to_string(), slug.to_string()), now);
            }
        }
    }
}

/// Append one ADR-094 channel lifecycle event, namespaced and attributed the
/// same way as `record_channel_heartbeat`'s persisted rows.
///
/// Best-effort: `store == None` is a no-op, and a serialize/append failure is
/// logged and swallowed — no lifecycle-append error may ever interrupt or
/// slow down channel polling.
#[cfg(feature = "channel-email")]
pub(super) async fn append_channel_lifecycle_event<P: serde::Serialize>(
    store: Option<&std::sync::Arc<dyn khive_storage::EventStore>>,
    kind: khive_types::EventKind,
    payload: P,
) {
    let Some(store) = store else {
        return;
    };
    let payload_value = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "khive_mcp::serve",
                error = %e,
                event_kind = %kind.name(),
                "failed to serialize channel lifecycle event payload"
            );
            return;
        }
    };
    let event = khive_storage::Event::new(
        khive_pack_comm::CHANNEL_HEALTH_NAMESPACE,
        "channel.poll_lifecycle",
        kind,
        khive_types::SubstrateKind::Event,
        "daemon:channel_poll_loop",
    )
    .with_payload(payload_value);
    if let Err(err) = store.append_event(event).await {
        tracing::warn!(target: "khive_mcp::serve",
            error = %err,
            event_kind = %kind.name(),
            "channel lifecycle event append failed"
        );
    }
}

/// One poll attempt's outcome, as reported to `comm.heartbeat` (#606).
#[cfg(feature = "channel-email")]
pub(super) enum HeartbeatOutcome {
    Success,
    Failure {
        class: &'static str,
        message: String,
    },
}

/// Map a [`khive_channel::ChannelError`] to the `comm.heartbeat` `error_class`
/// open string enum (#606: `auth | transport | config`
/// in v1, callers must tolerate unknown classes). `Auth`/`Transport` are the
/// connectivity classes `is_backoff_eligible` already distinguishes;
/// `Config`/`UnauthorizedSender`/`InvalidEnvelope` are static/attribution
/// failures, never produced by `poll`/`connect` in practice (see
/// `is_backoff_eligible`'s doc comment), so they all map to `"config"`.
#[cfg(feature = "channel-email")]
pub(super) fn channel_error_class(err: &khive_channel::ChannelError) -> &'static str {
    match err {
        khive_channel::ChannelError::Auth(_) | khive_channel::ChannelError::RetryableAuth(_) => {
            "auth"
        }
        khive_channel::ChannelError::Transport(_)
        | khive_channel::ChannelError::RateLimited { .. }
        | khive_channel::ChannelError::PermanentTransport(_) => "transport",
        khive_channel::ChannelError::Config(_)
        | khive_channel::ChannelError::UnauthorizedSender(_)
        | khive_channel::ChannelError::InvalidEnvelope(_) => "config",
    }
}

/// Persist one poll attempt's outcome via the `comm.heartbeat` subhandler
/// (#606). Best-effort: a failed write is logged, never interrupts the poll
/// loop. Takes NO `namespace` param — heartbeat rows are always dispatched
/// against `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` regardless of the
/// daemon's configured `KHIVE_EMAIL_INGEST_NAMESPACE` (2026-07-04); an
/// explicitly-scoped `comm.health` read may see a different namespace
/// (khive #877).
#[cfg(feature = "channel-email")]
pub(super) async fn record_channel_heartbeat(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
    outcome: HeartbeatOutcome,
    event_store: Option<&std::sync::Arc<dyn khive_storage::EventStore>>,
) {
    use serde_json::json;

    let namespace = khive_pack_comm::CHANNEL_HEALTH_NAMESPACE;
    let params = match &outcome {
        HeartbeatOutcome::Success => json!({
            "namespace": namespace,
            "channel_kind": channel_kind,
            "channel_slug": channel_slug,
            "poll_interval_secs": CHANNEL_POLL_INTERVAL.as_secs(),
            "outcome": "success",
        }),
        HeartbeatOutcome::Failure { class, message } => json!({
            "namespace": namespace,
            "channel_kind": channel_kind,
            "channel_slug": channel_slug,
            "poll_interval_secs": CHANNEL_POLL_INTERVAL.as_secs(),
            "outcome": "failure",
            "error_class": class,
            "error_message": message,
        }),
    };
    if let Err(e) = registry.dispatch("comm.heartbeat", params).await {
        tracing::warn!(target: "khive_mcp::serve",
            channel = channel_kind,
            "comm.heartbeat failed to persist poll outcome: {e}"
        );
        append_channel_lifecycle_event(
            event_store,
            khive_types::EventKind::ChannelHeartbeatPersistFailed,
            khive_storage::ChannelHeartbeatPersistFailedPayload {
                channel_kind: channel_kind.to_string(),
                channel_slug: channel_slug.to_string(),
                error: e.to_string(),
            },
        )
        .await;
    }
}

/// Load the durable poll checkpoint for `(channel_kind, channel_slug)` via
/// `comm.cursor_get` (issue #449). Returns `Ok(None)` on first-run
/// (`comm.cursor_get` returns JSON `null`). A dispatch failure or a
/// malformed response is returned as `Err` so the caller skips this
/// channel's poll for the cycle rather than risk polling with empty
/// progress and silently discarding durable state.
#[cfg(feature = "channel-email")]
pub(super) async fn load_channel_cursor(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
) -> Result<Option<khive_channel::StoredChannelCheckpoint>, khive_runtime::RuntimeError> {
    use serde_json::json;

    let value = registry
        .dispatch(
            "comm.cursor_get",
            json!({
                "channel_kind": channel_kind,
                "channel_slug": channel_slug,
            }),
        )
        .await?;
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(|e| {
        khive_runtime::RuntimeError::Internal(format!(
            "comm.cursor_get returned a malformed checkpoint: {e}"
        ))
    })
}

/// Persist the durable poll checkpoint for `(channel_kind, channel_slug)`
/// via `comm.cursor_commit` (issue #449).
///
/// Callers MUST only call this after every envelope in the page has
/// returned `Ok` from `comm.ingest` -- see `channel_poll_loop`. Committing
/// on a partial page would advance the cursor past a message that was never
/// durably ingested, permanently skipping it.
#[cfg(feature = "channel-email")]
pub(super) async fn commit_channel_cursor(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
    checkpoint: &khive_channel::ChannelCheckpoint,
) -> Result<(), khive_runtime::RuntimeError> {
    use serde_json::json;

    registry
        .dispatch(
            "comm.cursor_commit",
            json!({
                "channel_kind": channel_kind,
                "channel_slug": channel_slug,
                "source": checkpoint.source,
                "generation": checkpoint.generation,
                "high_water": checkpoint.high_water,
            }),
        )
        .await?;
    Ok(())
}

/// Log a backoff-eligible poll failure at the level ADR-091's `crossing_warn`
/// discipline calls for: `warn!` only on an escalation edge
/// (`tick.should_warn`, i.e. the computed step just changed), `debug!` on a
/// repeat at the same step. Regression fix (2026-07-04): the poll loop
/// previously emitted a generic `warn!` on every eligible retry in addition
/// to the escalation-edge warn, so sustained pressure spammed warn-level logs
/// once per retry instead of once per escalation. Extracted to a standalone
/// function so the level decision is unit-testable without driving the full
/// poll loop.
#[cfg(feature = "channel-email")]
pub(super) fn log_eligible_poll_failure(
    kind: &str,
    err: &khive_channel::ChannelError,
    tick: &khive_channel_email::BackoffTick,
) {
    if tick.should_warn {
        tracing::warn!(target: "khive_mcp::serve",
            channel = kind,
            attempt = tick.attempt,
            delay_secs = tick.delay.as_secs_f64(),
            "IMAP poll backoff escalating after connect/auth failure: {err}"
        );
    } else {
        tracing::debug!(target: "khive_mcp::serve",
            channel = kind,
            attempt = tick.attempt,
            delay_secs = tick.delay.as_secs_f64(),
            "channel poll failed, holding at current backoff step: {err}"
        );
    }
}

/// True if a note's `delivered_at` property marks it as already delivered.
///
/// Must match the outbox-scan pending predicate
/// (`list_undelivered_outbound_messages`): a present-but-null `delivered_at`
/// is undelivered, not delivered (checking `.is_some()` alone would treat an
/// explicit null — e.g. left by a curation `update` — as delivered and strand
/// the note in the outbox forever), and a terminal `properties.delivery`
/// state (`"delivered"` / `"failed"`, ADR-122 §1) is not pending even when
/// `delivered_at` is absent.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) fn note_already_delivered(props: &serde_json::Map<String, serde_json::Value>) -> bool {
    let delivered_at_set = props
        .get("delivered_at")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let terminal_delivery = props
        .get("delivery")
        .and_then(|v| v.as_str())
        .is_some_and(|state| state == "delivered" || state == "failed");
    delivered_at_set || terminal_delivery
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) const OUTBOUND_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) const OUTBOUND_RETRY_CEILING: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) async fn record_outbound_send_failure(
    runtime: &khive_runtime::KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    note_id: uuid::Uuid,
    error: &khive_channel::ChannelError,
) -> khive_runtime::RuntimeResult<khive_storage::note::Note> {
    use khive_channel::DeliveryFailureClass;

    match error.delivery_failure_class() {
        DeliveryFailureClass::Transient => {
            let (base_delay, max_delay) = match error {
                khive_channel::ChannelError::RateLimited { retry_after, .. } => (
                    OUTBOUND_RETRY_BASE.max(*retry_after),
                    OUTBOUND_RETRY_CEILING.max(*retry_after),
                ),
                _ => (OUTBOUND_RETRY_BASE, OUTBOUND_RETRY_CEILING),
            };
            runtime
                .mark_outbound_message_transient_failure(
                    token,
                    note_id,
                    chrono::Utc::now(),
                    error.to_string(),
                    base_delay,
                    max_delay,
                )
                .await
        }
        DeliveryFailureClass::Permanent => {
            runtime
                .mark_outbound_message_failed(
                    token,
                    note_id,
                    chrono::Utc::now().to_rfc3339(),
                    error.to_string(),
                )
                .await
        }
    }
}

#[cfg(feature = "channel-email")]
pub(super) fn outbound_claim_failure_is_permanent(error: &khive_runtime::RuntimeError) -> bool {
    fn invalid_storage_input(error: &khive_storage::StorageError) -> bool {
        match error {
            khive_storage::StorageError::InvalidInput { .. } => true,
            khive_storage::StorageError::WriterTaskRequestFailed { source, .. } => {
                invalid_storage_input(source)
            }
            _ => false,
        }
    }
    match error {
        khive_runtime::RuntimeError::InvalidInput(_) => true,
        khive_runtime::RuntimeError::Khive(error) => {
            error.kind() == khive_types::ErrorKind::InvalidInput
        }
        khive_runtime::RuntimeError::Storage(error) => invalid_storage_input(error),
        // Pressure, conflicts, and unclassified backend failures can recover;
        // they get the existing bounded backoff, never a per-note terminal mark.
        _ => false,
    }
}

#[cfg(feature = "channel-email")]
pub(super) async fn record_outbound_claim_failure(
    runtime: &khive_runtime::KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    note_id: uuid::Uuid,
    error: &khive_runtime::RuntimeError,
) -> khive_runtime::RuntimeResult<khive_storage::note::Note> {
    if outbound_claim_failure_is_permanent(error) {
        runtime
            .mark_outbound_message_claim_failed(
                token,
                note_id,
                chrono::Utc::now().to_rfc3339(),
                error.to_string(),
            )
            .await
    } else {
        runtime
            .mark_outbound_message_claim_transient_failure(
                token,
                note_id,
                chrono::Utc::now(),
                error.to_string(),
                OUTBOUND_RETRY_BASE,
                OUTBOUND_RETRY_CEILING,
            )
            .await
    }
}

/// Background task that delivers undelivered outbound email notes every 5 seconds.
///
/// Implements AT-LEAST-ONCE delivery: the `external_id` (= RFC 822 Message-ID) is
/// persisted to the note BEFORE sending. A crash between the SMTP success and the
/// `delivered_at` write causes a duplicate send on restart; the duplicate carries
/// the same Message-ID so receiving MTAs typically collapse it.
///
/// Every storage touch (scan, `external_id` claim, `delivered_at` mark) goes
/// through `runtime`'s non-wire owner-side APIs rather than
/// `registry.dispatch(...)`: the generic wire verbs run on the kg pack's
/// runtime, which under a `[packs.comm]` backend assignment is a different
/// backend than the one holding comm's notes, and `external_id` is
/// additionally one of the owner-established properties the generic `update`
/// verb refuses to patch on a pack-owned note kind.
///
/// Only compiled when the `channel-email` feature is enabled.
#[cfg(feature = "channel-email")]
pub(crate) async fn channel_outbox_loop(
    email_channel: Arc<dyn khive_channel::Channel>,
    runtime: khive_runtime::KhiveRuntime,
    ingest_namespace: String,
    mailbox: String,
    ctx: crate::components::HostContext,
) -> Result<(), crate::components::ComponentError> {
    outbox::require_email_delivery_policy(&runtime)?;
    let historical = match std::env::var(khive_runtime::HISTORICAL_DOMAINS_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(crate::components::ComponentError::Permanent(format!(
                "{} must contain valid Unicode text",
                khive_runtime::HISTORICAL_DOMAINS_ENV
            )));
        }
    };
    let domains =
        khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(&mailbox, &historical)
            .map_err(crate::components::ComponentError::Permanent)?;
    outbox::validate_loop_channel(email_channel.as_ref(), "email")?;
    let slug = email_channel.slug();
    let mut channels = khive_channel::ChannelRegistry::new();
    channels.register(email_channel);
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
            outbox::OutboxPolicy::Email {
                mailbox: &mailbox,
                domains: &domains,
            },
            &runtime,
            &namespace,
            ctx.cancellation(),
            &mut pause_until,
        )
        .await?;
        ctx.heartbeat();
    }
}

/// Execute one email outbox scan. Kept separate from the five-second loop so
/// routing and owner-claim behavior can be verified without sleeping or
/// opening a network transport. Account-wide authentication errors propagate
/// to the supervisor without becoming per-message terminal failures.
#[cfg(all(test, feature = "channel-email"))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn channel_outbox_once(
    email_channel: &dyn khive_channel::Channel,
    runtime: &khive_runtime::KhiveRuntime,
    namespace: &khive_runtime::Namespace,
    mailbox: &str,
    domain: &str,
    allowlist: &[String],
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<(), crate::components::ComponentError> {
    let domains = khive_runtime::EmailMessageIdDomains::from_mailbox_and_history(mailbox, "")
        .map_err(crate::components::ComponentError::Permanent)?;
    debug_assert_eq!(domains.current(), domain);
    let policy = OutboundEmailPolicy::configured(allowlist.to_vec())
        .map_err(crate::components::ComponentError::Permanent)?;
    let runtime = runtime.clone().with_outbound_email_policy(policy);
    let mut pause_until = None;
    outbox::outbox_once(
        outbox::OutboxChannels::Single(email_channel),
        outbox::OutboxPolicy::Email {
            mailbox,
            domains: &domains,
        },
        &runtime,
        namespace,
        cancellation,
        &mut pause_until,
    )
    .await
}
