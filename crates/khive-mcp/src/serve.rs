//! Build the runtime + server from CLI args and serve over the selected transport.
//!
//! This is the bootstrap that the `kkernel mcp` subcommand drives. Logging is
//! initialized by the binary, not here.

use std::collections::HashMap;
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
pub async fn run(args: Args, registry: &TransportRegistry) -> anyhow::Result<()> {
    let server = build_server(&args)?;

    #[cfg(feature = "channel-email")]
    spawn_email_channel_loops_if_daemon(&server, &args);

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

/// Whether this process owns the email channel loops (#602).
///
/// Channel loops (IMAP poll + outbox scan) are a daemon-role responsibility:
/// before this gate, `spawn_email_channel_loops` was called unconditionally
/// from EVERY serve entrypoint, so every stdio `kkernel mcp` client process
/// (one per Claude Code session, agent, etc.) spawned its own independent IMAP
/// poll loop against the same mailbox. Nine concurrent pollers exhausted
/// Exchange Online's per-mailbox connection slots and took inbound email down
/// for ~19h on 2026-07-04. `args.daemon` is the same flag `run`/`serve_server`
/// already use to decide whether to hand off to
/// `khive_runtime::daemon::run_daemon`, so gating on it keeps daemon-role
/// detection in one place shared by both boot paths, matching the
/// `checkpoint_pool_for` pattern (#601/#604).
#[cfg(feature = "channel-email")]
fn is_daemon_role(args: &Args) -> bool {
    args.daemon
}

/// Spawn the email channel loops if — and only if — `args` indicates this
/// process is the daemon (#602). Shared by both serve entrypoints (`run` and
/// `serve_server`) so the role gate lives in exactly one place instead of
/// being duplicated at each call site. Emits one `tracing::info!` line either
/// way so the decision is visible at startup (seeds #606's health surface).
///
/// If no daemon is running, mail is simply not polled until one starts — that
/// is the intended behavior, not a silent failure; the log line makes it
/// observable.
#[cfg(feature = "channel-email")]
fn spawn_email_channel_loops_if_daemon(server: &KhiveMcpServer, args: &Args) {
    if is_daemon_role(args) {
        tracing::info!("email channel loops: spawning (daemon role)");
        spawn_email_channel_loops(server);
    } else {
        tracing::info!("email channel loops: skipped (client role; daemon owns channel loops)");
    }
}

/// Spawn the email channel polling + outbox loops if the `channel-email`
/// feature is enabled and `KHIVE_EMAIL_*` config resolves. Non-fatal: logs a
/// warning and returns on incomplete config. Only call this when
/// [`is_daemon_role`] is true — use [`spawn_email_channel_loops_if_daemon`],
/// which both serve entrypoints (`run` and `serve_server`) call.
#[cfg(feature = "channel-email")]
fn spawn_email_channel_loops(server: &KhiveMcpServer) {
    use khive_channel::ChannelRegistry;
    use khive_channel_email::EmailChannel;
    use std::sync::Arc;

    match EmailChannel::from_env() {
        Ok(email_ch) => {
            let email_ch = Arc::new(email_ch);
            let mut ch_registry = ChannelRegistry::new();
            let dyn_ch: Arc<dyn khive_channel::Channel> = email_ch.clone();
            ch_registry.register(dyn_ch);
            let ch_registry = Arc::new(ch_registry);
            let verb_reg = server.verb_registry_clone();
            let ingest_ns = ingest_namespace_from_env();
            let default_actor = default_inbound_actor_from_env();
            let mut allowlist = allowed_recipients_from_env();
            if allowlist.is_empty() {
                allowlist.push(email_ch.maintainer_address().to_string());
            }
            let mailbox = email_ch.mailbox().to_string();

            let ingest_ns_clone = ingest_ns.clone();
            let default_actor_clone = default_actor.clone();
            let verb_reg_poll = verb_reg.clone();
            let verb_reg_outbox = verb_reg.clone();
            let ingest_ns_outbox = ingest_ns.clone();
            let allowlist_clone = allowlist.clone();
            let mailbox_clone = mailbox.clone();
            let email_ch_clone = Arc::clone(&email_ch);

            let spawned = run_if_authorized(&ingest_ns, &verb_reg, || {
                tokio::task::spawn(channel_poll_loop(
                    ch_registry,
                    verb_reg_poll,
                    ingest_ns_clone,
                    default_actor_clone,
                ));
                tokio::task::spawn(channel_outbox_loop(
                    email_ch_clone,
                    verb_reg_outbox,
                    ingest_ns_outbox,
                    mailbox_clone,
                    allowlist_clone,
                ));
                tracing::info!("email channel polling and outbox loops started");
            });
            if !spawned {
                tracing::error!(
                    namespace = %ingest_ns,
                    "email channel loops NOT started: ingest namespace authorization failed (fail-closed)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
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
fn ingest_namespace_from_env() -> String {
    std::env::var("KHIVE_EMAIL_INGEST_NAMESPACE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// Resolve the default inbound actor for fresh (uncorrelated) email messages.
///
/// Reads `KHIVE_EMAIL_DEFAULT_ACTOR`; falls back to `"lambda:leo"` when the
/// variable is unset or blank. Called once at server startup alongside
/// `ingest_namespace_from_env`.
#[cfg(feature = "channel-email")]
fn default_inbound_actor_from_env() -> String {
    std::env::var("KHIVE_EMAIL_DEFAULT_ACTOR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "lambda:leo".to_string())
}

/// Parse the outbox allowlist from `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS`.
///
/// Returns a `Vec` of trimmed, non-empty address strings. When the env var is
/// unset or blank the returned vec is empty; callers should fall back to the
/// channel maintainer address in that case.
#[cfg(feature = "channel-email")]
fn allowed_recipients_from_env() -> Vec<String> {
    std::env::var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Run `on_authorized` only when the ingest namespace passes the preflight check.
///
/// Returns `true` when the closure was called (preflight passed), `false`
/// otherwise.  Tests can inject a counting closure to verify the loop is not
/// started when preflight fails (ADR-056 §6 fail-closed contract).
#[cfg(feature = "channel-email")]
fn run_if_authorized(
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
#[cfg(feature = "channel-email")]
fn preflight_ingest_namespace(ns_str: &str, registry: &khive_runtime::VerbRegistry) -> bool {
    match khive_runtime::Namespace::parse(ns_str) {
        Ok(ns) => match registry.authorize_namespace(ns) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    namespace = %ns_str,
                    error = %e,
                    "ingest namespace authorization denied; email polling will not start"
                );
                false
            }
        },
        Err(e) => {
            tracing::error!(
                namespace = %ns_str,
                error = %e,
                "invalid ingest namespace string; email polling will not start"
            );
            false
        }
    }
}

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
async fn channel_poll_loop(
    channels: std::sync::Arc<khive_channel::ChannelRegistry>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
    default_inbound_actor: String,
) {
    use chrono::Utc;
    use khive_channel_email::{is_backoff_eligible, ImapBackoff};
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;

    const HAPPY_PATH_INTERVAL: Duration = Duration::from_secs(5);

    let mut last_poll = Utc::now();
    // One backoff state per (kind, slug) — i.e. per credential (#606 round-1
    // internal review, High finding). Keying by kind alone would throttle a
    // second same-kind credential (e.g. a second mailbox) whenever the first
    // one's connection fails, even though the two are independent
    // credentials with independent connectivity.
    let mut backoffs: HashMap<(String, String), ImapBackoff> = HashMap::new();
    let mut next_interval = HAPPY_PATH_INTERVAL;

    loop {
        tokio::time::sleep(next_interval).await;
        next_interval = HAPPY_PATH_INTERVAL;

        let since = last_poll;
        last_poll = Utc::now();

        for (kind, slug, channel) in channels.iter() {
            let backoff_key = (kind.to_string(), slug.to_string());
            match channel.poll(since).await {
                Ok(envelopes) => {
                    if let Some(backoff) = backoffs.get_mut(&backoff_key) {
                        backoff.record_success();
                    }
                    record_channel_heartbeat(&registry, kind, slug, HeartbeatOutcome::Success)
                        .await;
                    for env in envelopes {
                        let params = json!({
                            "namespace": ingest_namespace,
                            "from": env.from,
                            "to": env.to,
                            "content": env.content,
                            "subject": env.subject,
                            "channel_kind": kind,
                            "external_id": env.external_id,
                            "sent_at": env.sent_at.map(|ts| ts.to_rfc3339()),
                            "correlation_external_id": env.correlation_external_id,
                            "default_inbound_actor": default_inbound_actor,
                            "wire_message_id": env.wire_message_id,
                            "wire_references": env.wire_references,
                            "metadata": env.metadata,
                        });
                        if let Err(e) = registry.dispatch("comm.ingest", params).await {
                            tracing::warn!(
                                channel = kind,
                                "comm.ingest failed for inbound message: {e}"
                            );
                        }
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
                    )
                    .await;
                    if is_backoff_eligible(&e) {
                        let backoff = backoffs.entry(backoff_key).or_default();
                        let tick = backoff.record_failure();
                        log_eligible_poll_failure(kind, &e, &tick);
                        next_interval = next_interval.max(tick.delay);
                    } else {
                        // Non-eligible failures (config/gate errors, never
                        // produced by poll/connect in practice) are not
                        // connectivity pressure, so they keep the pre-#605
                        // warn-every-retry behavior at the normal cadence.
                        tracing::warn!(channel = kind, "channel poll failed: {e}");
                    }
                }
            }
        }
    }
}

/// One poll attempt's outcome, as reported to `comm.heartbeat` (#606).
#[cfg(feature = "channel-email")]
enum HeartbeatOutcome {
    Success,
    Failure {
        class: &'static str,
        message: String,
    },
}

/// Map a [`khive_channel::ChannelError`] to the `comm.heartbeat` `error_class`
/// open string enum (#606 design review amendment 4: `auth | transport | config`
/// in v1, callers must tolerate unknown classes). `Auth`/`Transport` are the
/// connectivity classes `is_backoff_eligible` already distinguishes;
/// `Config`/`UnauthorizedSender`/`InvalidEnvelope` are static/attribution
/// failures, never produced by `poll`/`connect` in practice (see
/// `is_backoff_eligible`'s doc comment), so they all map to `"config"`.
#[cfg(feature = "channel-email")]
fn channel_error_class(err: &khive_channel::ChannelError) -> &'static str {
    match err {
        khive_channel::ChannelError::Auth(_) => "auth",
        khive_channel::ChannelError::Transport(_) => "transport",
        khive_channel::ChannelError::Config(_)
        | khive_channel::ChannelError::UnauthorizedSender(_)
        | khive_channel::ChannelError::InvalidEnvelope(_) => "config",
    }
}

/// Persist one poll attempt's outcome via the `comm.heartbeat` subhandler
/// (#606). Best-effort: a failed heartbeat write is logged and does not
/// interrupt the poll loop — the heartbeat row is an observability surface,
/// not a correctness dependency for message delivery.
///
/// Takes NO `namespace` parameter (round-2, design review Blocker fix, example actor
/// 2026-07-04): heartbeat rows are an operational surface, not message data,
/// so they are ALWAYS dispatched against
/// `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` — the same constant
/// `handle_health` reads from — regardless of what
/// `KHIVE_EMAIL_INGEST_NAMESPACE` this daemon is configured with for message
/// ingestion. `handle_heartbeat` additionally hardcodes the persisted row's
/// namespace to this same constant, so the guarantee holds even if a future
/// caller changes what this dispatch call passes.
#[cfg(feature = "channel-email")]
async fn record_channel_heartbeat(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
    outcome: HeartbeatOutcome,
) {
    use serde_json::json;

    let namespace = khive_pack_comm::CHANNEL_HEALTH_NAMESPACE;
    let params = match outcome {
        HeartbeatOutcome::Success => json!({
            "namespace": namespace,
            "channel_kind": channel_kind,
            "channel_slug": channel_slug,
            "outcome": "success",
        }),
        HeartbeatOutcome::Failure { class, message } => json!({
            "namespace": namespace,
            "channel_kind": channel_kind,
            "channel_slug": channel_slug,
            "outcome": "failure",
            "error_class": class,
            "error_message": message,
        }),
    };
    if let Err(e) = registry.dispatch("comm.heartbeat", params).await {
        tracing::warn!(
            channel = channel_kind,
            "comm.heartbeat failed to persist poll outcome: {e}"
        );
    }
}

/// Log a backoff-eligible poll failure at the level ADR-091's `crossing_warn`
/// discipline calls for: `warn!` only on an escalation edge
/// (`tick.should_warn`, i.e. the computed step just changed), `debug!` on a
/// repeat at the same step. internal review round 1 finding (2026-07-04): the poll loop
/// previously emitted a generic `warn!` on every eligible retry in addition
/// to the escalation-edge warn, so sustained pressure spammed warn-level logs
/// once per retry instead of once per escalation. Extracted to a standalone
/// function so the level decision is unit-testable without driving the full
/// poll loop.
#[cfg(feature = "channel-email")]
fn log_eligible_poll_failure(
    kind: &str,
    err: &khive_channel::ChannelError,
    tick: &khive_channel_email::BackoffTick,
) {
    if tick.should_warn {
        tracing::warn!(
            channel = kind,
            attempt = tick.attempt,
            delay_secs = tick.delay.as_secs_f64(),
            "IMAP poll backoff escalating after connect/auth failure: {err}"
        );
    } else {
        tracing::debug!(
            channel = kind,
            attempt = tick.attempt,
            delay_secs = tick.delay.as_secs_f64(),
            "channel poll failed, holding at current backoff step: {err}"
        );
    }
}

/// True if a note's `delivered_at` property marks it as already delivered.
///
/// Must match the `list` query predicate's null handling (`list.rs`): a
/// present-but-null `delivered_at` is undelivered, not delivered. Checking
/// `.is_some()` alone would treat an explicit null (e.g. left by a curation
/// `update`) as delivered and strand the note in the outbox forever.
#[cfg(feature = "channel-email")]
fn note_already_delivered(props: &serde_json::Map<String, serde_json::Value>) -> bool {
    props
        .get("delivered_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

/// Background task that delivers undelivered outbound email notes every 5 seconds.
///
/// Implements AT-LEAST-ONCE delivery: the `external_id` (= RFC 822 Message-ID) is
/// persisted to the note BEFORE sending. A crash between the SMTP success and the
/// `delivered_at` write causes a duplicate send on restart; the duplicate carries
/// the same Message-ID so receiving MTAs typically collapse it.
///
/// Only compiled when the `channel-email` feature is enabled.
#[cfg(feature = "channel-email")]
async fn channel_outbox_loop(
    email_channel: std::sync::Arc<khive_channel_email::EmailChannel>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
    mailbox: String,
    allowlist: Vec<String>,
) {
    use chrono::Utc;
    use khive_channel::{Channel, ChannelEnvelope};
    use serde_json::json;

    let domain = mailbox.split('@').nth(1).unwrap_or("localhost").to_string();

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        // Query outbound messages via the registry. The note `list` handler applies
        // the `direction` filter server-side (scanning up to its internal cap) and
        // returns a bare JSON array of full note objects. There is no `delivered_at`
        // or recipient-prefix filter, so the `email:` prefix and the
        // already-delivered check are applied per-note below.
        let list_params = json!({
            "namespace": ingest_namespace,
            "kind": "message",
            "direction": "outbound",
            "delivered": false,
            "limit": 200,
        });
        let list_result = match registry.dispatch("list", list_params).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "outbox loop: list failed");
                continue;
            }
        };

        let notes = match list_result.as_array() {
            Some(arr) => arr.clone(),
            None => continue,
        };

        for note_val in notes {
            let props = match note_val.get("properties") {
                Some(serde_json::Value::Object(m)) => m.clone(),
                _ => continue,
            };

            // Only outbound direction. The `delivered=false` filter on the list query
            // ensures only undelivered notes are returned; this check is a cheap
            // defensive guard for any note that slips through.
            if props.get("direction").and_then(|v| v.as_str()) != Some("outbound") {
                continue;
            }

            // Only email-addressed notes.
            let to_actor = match props.get("to_actor").and_then(|v| v.as_str()) {
                Some(a) if a.starts_with("email:") => a.to_string(),
                _ => continue,
            };

            // Defensive: skip already-delivered notes in case the query filter missed any.
            if note_already_delivered(&props) {
                continue;
            }

            let note_id = match note_val.get("id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };

            let recipient = to_actor
                .strip_prefix("email:")
                .unwrap_or(to_actor.as_str())
                .to_string();

            // Allowlist check.
            if !allowlist.is_empty() && !allowlist.contains(&recipient) {
                tracing::warn!(
                    note_id = %note_id,
                    recipient = %recipient,
                    "outbox loop: recipient not in allowlist; skipping"
                );
                continue;
            }

            let subject = props
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("(no subject)")
                .to_string();

            let content = match note_val.get("content").and_then(|v| v.as_str()) {
                Some(c) => c.to_string(),
                None => continue,
            };

            let thread_id = props
                .get("thread_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // Issue #403: the parent's wire Message-ID, computed at reply time by
            // comm.reply (khive-pack-comm) and stored on this note. Forwarded
            // verbatim so the SMTP layer can set In-Reply-To for native MUA
            // conversation grouping; absent for non-reply sends.
            let in_reply_to = props
                .get("in_reply_to_message_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // Issue #403 finding 1: the full References chain (parent's existing
            // chain, if any, followed by the parent's Message-ID), computed at
            // reply time by comm.reply. Forwarded verbatim so the SMTP layer can
            // set References without truncating ancestry; absent for non-reply sends.
            let references = props
                .get("references_chain")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // Mint-before-send: derive or reuse the Message-ID.
            let message_id = match props.get("external_id").and_then(|v| v.as_str()) {
                Some(eid) if !eid.is_empty() => eid.to_string(),
                _ => {
                    let mid = format!("<{note_id}@{domain}>");
                    // Persist the claimed external_id before sending.
                    let claim_result = registry
                        .dispatch(
                            "update",
                            json!({
                                "namespace": ingest_namespace,
                                "id": note_id,
                                "properties": { "external_id": mid.clone() },
                            }),
                        )
                        .await;
                    if let Err(e) = claim_result {
                        tracing::warn!(
                            note_id = %note_id,
                            error = %e,
                            "outbox loop: failed to claim external_id; skipping"
                        );
                        continue;
                    }
                    mid
                }
            };

            // Build and send the envelope.
            let mut env = ChannelEnvelope::new(
                format!("email:{mailbox}"),
                format!("email:{recipient}"),
                content,
            )
            .with_subject(subject)
            .with_message_id(message_id.clone());

            if let Some(tid) = thread_id {
                env = env.with_correlation(tid);
            }
            if let Some(irt) = in_reply_to {
                env = env.with_in_reply_to(irt);
            }
            if let Some(refs) = references {
                env = env.with_references(refs);
            }

            match email_channel.send(env).await {
                Ok(()) => {
                    let delivered_at = Utc::now().to_rfc3339();
                    let mark_result = registry
                        .dispatch(
                            "update",
                            json!({
                                "namespace": ingest_namespace,
                                "id": note_id,
                                "properties": { "delivered_at": delivered_at },
                            }),
                        )
                        .await;
                    match mark_result {
                        Ok(_) => {
                            tracing::info!(
                                note_id = %note_id,
                                recipient = %recipient,
                                message_id = %message_id,
                                "outbox loop: delivered"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                note_id = %note_id,
                                error = %e,
                                "outbox loop: failed to set delivered_at (AT-LEAST-ONCE: will retry)"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        note_id = %note_id,
                        recipient = %recipient,
                        error = %e,
                        "outbox loop: send failed; will retry next cycle"
                    );
                }
            }
        }
    }
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
    #[cfg(feature = "channel-email")]
    spawn_email_channel_loops_if_daemon(&server, args);

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
    let backend_count = khive_cfg.backends.len();
    let force_memory = match cli_db_override {
        Some(":memory:") => {
            tracing::warn!(
                "--db :memory: (or KHIVE_DB=:memory:) is overriding {backend_count} \
                 configured [[backends]] entries to in-memory storage for this invocation; \
                 khive.toml's declared backend paths will not be used this run"
            );
            true
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
        None => false,
    };

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
    //
    // `config.db_path` (already resolved above) anchors tier-3 project-local config
    // discovery to the database's own directory rather than the process cwd, so this
    // reload agrees with the one inside `resolve_runtime_config` regardless of cwd.
    let khive_cfg =
        KhiveConfig::load_with_home_fallback(args.config.as_deref(), config.db_path.as_deref())
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
        let fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
        return KhiveMcpServer::new(runtime)
            .map(|s| s.with_default_output_format(fmt))
            .map_err(|e| anyhow::anyhow!("{e}"));
    }

    // Multi-backend path (ADR-028).
    build_server_multi_backend(config, &khive_cfg, args.db.as_deref())
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
    let multi = build_registry_for_multi_backend(base_config, khive_cfg, cli_db_override)?;
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
    let fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);

    let server = KhiveMcpServer::from_registry_with_meta(
        multi.registry,
        &multi.default_namespace,
        &multi.config_id,
    )
    .with_default_output_format(fmt);

    let server = match coordinator {
        Some(c) => server.with_coordinator(c),
        None => server,
    };

    match pool {
        Some(p) => server.with_pool(p),
        None => server,
    }
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
    /// Whether the default ingest namespace would authorize the email
    /// channel loops to start if this process runs in the daemon role
    /// (#503/#602). The actual spawn is arg-driven at `run`/`serve_server`
    /// (#610), not construction time, but the *authorization* outcome is a
    /// function of how the registry's gate was wired during construction —
    /// this field is the construction-time state that decision reads.
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
    if main_backend.is_file_backed() {
        Some(main_backend.pool_arc())
    } else {
        None
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
    let db_path = khive_runtime::resolve_db_anchor(inputs.db);

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

    // Captured before `base_config` (which owns `db_path`) is consumed below —
    // threaded into the config-file resolvers so tier-3 project-local config
    // discovery anchors to the resolved database's directory rather than the
    // process cwd (kills config_id drift between a client and the daemon
    // serving the same database at a different working directory).
    let db_path_for_config = base_config.db_path.clone();

    let resolved = if inputs.no_embed {
        let no_embed_base = RuntimeConfig {
            embedding_model: None,
            additional_embedding_models: vec![],
            ..base_config
        };
        resolve_actor_from_config(
            inputs.config,
            no_embed_base,
            inputs.namespace_explicit,
            db_path_for_config.as_deref(),
        )?
    } else {
        resolve_config(inputs.config, base_config, db_path_for_config.as_deref())?
    };

    // ADR-096 Fork 2 — per-connection `actor_id` precedence chain (highest to
    // lowest), ratified 2026-07-05:
    //
    //   1. Explicit CLI `--actor` / `--namespace` flag (ADR-057). `args.actor`
    //      also carries its `KHIVE_ACTOR` env-arg alias — clap binds `--actor`
    //      and `env = "KHIVE_ACTOR"` to the SAME field, so the two are
    //      indistinguishable at this layer; both are "the CLI tier" here.
    //      Threaded via `inputs.namespace` / `inputs.namespace_explicit`
    //      (`resolve_cli_namespace`). The `"local"` guard leaves the default
    //      namespace anonymous (consistent with `should_warn_unattributed`):
    //      an explicit `--actor local` must NOT populate `actor_id`.
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
    //      default()` above) — the pre-#651-drift fallback tier.
    //   4. Anonymous (`None`).
    //
    // Attribution-only: none of these tiers may feed `config_id` (`actor_id` is
    // not read by `compute_config_id`) or `default_namespace` (tier 1 already
    // sets `default_namespace` via `inputs.namespace` — unchanged pre-existing
    // behavior; tiers 2-4 never touch it, per ADR-007 Rev 4 Rule 0).
    let resolved = {
        let mut resolved = resolved;
        let ns = resolved.default_namespace.as_str().to_string();
        let cli_actor = (inputs.namespace_explicit && ns != "local").then_some(ns);
        let project_actor = khive_runtime::resolve_project_actor_id(inputs.config)
            .map_err(|e| anyhow::anyhow!("config error: {e}"))?;
        resolved.actor_id = cli_actor.or(project_actor).or(resolved.actor_id);
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

/// Resolve only the actor namespace from a config file (no-embed path).
///
/// `db_path` anchors tier-3 project-local config discovery to the database's
/// own directory instead of the process cwd (see [`resolve_config`]).
fn resolve_actor_from_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    cli_namespace_explicit: bool,
    db_path: Option<&std::path::Path>,
) -> anyhow::Result<RuntimeConfig> {
    if cli_namespace_explicit {
        return Ok(base);
    }
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
            no_embed: false,
            packs: None,
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

    /// Regression for BLOCKER-1 (PR #52 internal review): project-toml brain_profile
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

        // See the hermeticity note in `cli_actor_flag_populates_actor_id` above.
        let missing_config =
            std::path::PathBuf::from("/nonexistent/khive-cli-actor-local-test/config.toml");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
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
            no_embed: true,
            packs: None,
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
            no_embed: true,
            packs: None,
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
            no_embed: true,
            packs: None,
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
            no_embed: true,
            packs: None,
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

    /// `config_id` must stay byte-identical across two connections that share ONE
    /// database but declare DIFFERENT `[actor]` ids via their own project/cwd
    /// config (ADR-096 Fork 2 hard invariant — `actor_id` must never feed
    /// `compute_config_id`, whether directly or via the `visible_namespaces`
    /// fold-in). `default_namespace` must also stay `"local"` for both
    /// (ADR-007 Rev 4 Rule 0), independent of the configured actor.
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
                no_embed: true,
                packs: None,
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
                no_embed: true,
                packs: None,
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
             in [actor] id — actor_id must never feed compute_config_id"
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
            additional_embedding_models: vec![],
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
                format: None,
                format_per_op: None,
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

    /// Regression for ADR-073: a pack assigned to a secondary backend must
    /// have `core_backend` wired at boot so that `rt.core().backend_id()` returns "main".
    ///
    /// Before the fix, `build_server_multi_backend` called `KhiveRuntime::from_backend`
    /// directly (without `with_core_backend`), so `core()` fell back to `self.clone()` and
    /// returned the secondary-backend handle — silently defeating the ADR-073 contract.
    /// Both boot paths now delegate to `build_pack_runtime`, which applies the wiring in
    /// one place and prevents any future path from drifting.
    #[test]
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

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .expect("multi-backend registry must boot");

        let comm_rt = result
            .per_pack_runtimes
            .get("comm")
            .expect("comm pack runtime must be present in per_pack_runtimes");

        // Own backend_id is "secondary" — not main.
        assert_eq!(
            comm_rt.backend_id().as_str(),
            "secondary",
            "comm pack runtime's own backend_id must be \"secondary\""
        );

        // ADR-073 contract: core() on a secondary-backend pack must return a
        // main-bound handle, not a clone of self. Failure here means the
        // build_pack_runtime wiring was not applied.
        assert_eq!(
            comm_rt.core().backend_id().as_str(),
            BackendId::MAIN,
            "secondary-backend pack must have core_backend wired to main (ADR-073); \
             core().backend_id() returned {:?} — build_pack_runtime wiring missing",
            comm_rt.core().backend_id().as_str()
        );
    }

    /// Issue #553: `--db :memory:` (or `KHIVE_DB=:memory:`) must not be silently
    /// ignored just because `[[backends]]` declares real sqlite backends. Passing
    /// `Some(":memory:")` as `cli_db_override` must force every declared backend
    /// in-memory for this invocation, and the declared sqlite paths must never be
    /// created on disk.
    #[test]
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

        let base_cfg = base_runtime_config_for_multi_backend();

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

    /// Regression for B-BLOCKER-1 (design review critic): the multi-backend boot path
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

        let server = build_server_multi_backend(base_cfg, &khive_cfg, None)
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
                        format: None,
                        format_per_op: None,
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
                    "comm".to_string(),
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
                msg.contains("packs.comm"),
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
                    "comm".to_string(),
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
                msg.contains("packs.comm"),
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
        let result = build_server_multi_backend(base_cfg, &khive_cfg, None);
        if let Err(ref e) = result {
            panic!(
                "two backends with the same canonical path must share one Arc and boot ok; got: {e}"
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

        let base_cfg = base_runtime_config_for_multi_backend();

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

        let server = build_server_multi_backend(base_cfg, &khive_cfg, None)
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
                        format: None,
                        format_per_op: None,
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

    // --- ingest_namespace_from_env (Fix 4: namespace env var) ---

    #[cfg(feature = "channel-email")]
    mod ingest_ns_tests {
        use super::*;

        #[test]
        #[serial]
        fn ingest_namespace_defaults_to_local() {
            std::env::remove_var("KHIVE_EMAIL_INGEST_NAMESPACE");
            assert_eq!(ingest_namespace_from_env(), "local");
        }

        #[test]
        #[serial]
        fn ingest_namespace_reads_env_var() {
            std::env::set_var("KHIVE_EMAIL_INGEST_NAMESPACE", "lambda:mybot");
            let ns = ingest_namespace_from_env();
            std::env::remove_var("KHIVE_EMAIL_INGEST_NAMESPACE");
            assert_eq!(ns, "lambda:mybot");
        }

        #[test]
        #[serial]
        fn ingest_namespace_ignores_blank_env_var() {
            std::env::set_var("KHIVE_EMAIL_INGEST_NAMESPACE", "  ");
            let ns = ingest_namespace_from_env();
            std::env::remove_var("KHIVE_EMAIL_INGEST_NAMESPACE");
            assert_eq!(ns, "local", "blank env var must fall back to default");
        }

        #[test]
        fn preflight_fails_on_invalid_namespace_string() {
            let registry = khive_runtime::VerbRegistryBuilder::new()
                .build()
                .expect("build empty registry");
            // An empty string is not a valid namespace; parse must fail.
            assert!(
                !preflight_ingest_namespace("", &registry),
                "preflight must return false for an invalid namespace string"
            );
        }

        #[test]
        fn preflight_fails_when_gate_denies_namespace() {
            use khive_runtime::{Gate, GateDecision, GateError, GateRequest};
            use std::fmt;

            #[derive(Debug)]
            struct AlwaysDenyGate;
            impl fmt::Display for AlwaysDenyGate {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "AlwaysDenyGate")
                }
            }
            impl Gate for AlwaysDenyGate {
                fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                    Ok(GateDecision::deny("test: always deny"))
                }
            }

            let mut builder = khive_runtime::VerbRegistryBuilder::new();
            builder.with_gate(std::sync::Arc::new(AlwaysDenyGate));
            let registry = builder.build().expect("build registry with deny gate");
            assert!(
                !preflight_ingest_namespace("local", &registry),
                "preflight must return false when the gate denies the namespace"
            );
        }

        #[test]
        fn preflight_succeeds_with_allow_gate_and_valid_namespace() {
            let registry = khive_runtime::VerbRegistryBuilder::new()
                .build()
                .expect("build registry with default allow-all gate");
            assert!(
                preflight_ingest_namespace("local", &registry),
                "preflight must return true for a valid namespace when the gate allows"
            );
        }

        // --- spawn-seam tests: verify the loop is NOT started on preflight failure ---

        #[test]
        fn spawn_not_called_when_gate_denies() {
            use khive_runtime::{Gate, GateDecision, GateError, GateRequest};
            use std::fmt;

            #[derive(Debug)]
            struct AlwaysDenyGate2;
            impl fmt::Display for AlwaysDenyGate2 {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "AlwaysDenyGate2")
                }
            }
            impl Gate for AlwaysDenyGate2 {
                fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                    Ok(GateDecision::deny("spawn seam test: always deny"))
                }
            }

            let mut builder = khive_runtime::VerbRegistryBuilder::new();
            builder.with_gate(std::sync::Arc::new(AlwaysDenyGate2));
            let registry = builder.build().expect("build registry with deny gate");

            let mut spawn_count = 0usize;
            let authorized = run_if_authorized("local", &registry, || {
                spawn_count += 1;
            });

            assert!(
                !authorized,
                "run_if_authorized must return false when gate denies"
            );
            assert_eq!(
                spawn_count, 0,
                "spawn must not be called when preflight fails"
            );
        }

        #[test]
        fn spawn_not_called_when_namespace_invalid() {
            let registry = khive_runtime::VerbRegistryBuilder::new()
                .build()
                .expect("build empty registry");

            let mut spawn_count = 0usize;
            let authorized = run_if_authorized("", &registry, || {
                spawn_count += 1;
            });

            assert!(
                !authorized,
                "run_if_authorized must return false for invalid namespace"
            );
            assert_eq!(
                spawn_count, 0,
                "spawn must not be called when namespace is invalid"
            );
        }
    }

    // --- log_eligible_poll_failure: edge-triggered warn (internal review round 1 finding 2) ---

    #[cfg(feature = "channel-email")]
    mod eligible_poll_failure_log_tests {
        use super::*;
        use khive_channel::ChannelError;
        use khive_channel_email::BackoffTick;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use tracing::field::{Field, Visit};

        #[derive(Clone, Debug, Default)]
        struct CapturedEvent {
            level: Option<tracing::Level>,
            message: Option<String>,
        }

        #[derive(Default)]
        struct CapturedEventVisitor(Option<String>);

        impl Visit for CapturedEventVisitor {
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "message" {
                    self.0 = Some(value.to_string());
                }
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    let formatted = format!("{value:?}");
                    self.0 = Some(
                        formatted
                            .trim_start_matches('"')
                            .trim_end_matches('"')
                            .to_string(),
                    );
                }
            }
        }

        /// Minimal `tracing::Subscriber` capturing (level, message) pairs into
        /// a thread-local vec, installed via `tracing::subscriber::with_default`.
        /// Mirrors `khive-db/src/checkpoint.rs`'s `CaptureSubscriber` (same
        /// ADR-091 `crossing_warn` test discipline: prove the log fires on
        /// the escalation edge and stays silent on a same-step repeat).
        struct CaptureSubscriber {
            events: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl tracing::Subscriber for CaptureSubscriber {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut visitor = CapturedEventVisitor::default();
                event.record(&mut visitor);
                self.events.lock().unwrap().push(CapturedEvent {
                    level: Some(*event.metadata().level()),
                    message: visitor.0,
                });
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        fn tick(should_warn: bool, attempt: u32) -> BackoffTick {
            BackoffTick {
                delay: Duration::from_secs(10),
                step: Duration::from_secs(10),
                attempt,
                should_warn,
            }
        }

        #[test]
        fn escalation_edge_logs_warn_not_debug() {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };
            let err = ChannelError::Transport("boom".into());

            tracing::subscriber::with_default(subscriber, || {
                log_eligible_poll_failure("email", &err, &tick(true, 1));
            });

            let events = buffer.lock().unwrap();
            assert_eq!(
                events.len(),
                1,
                "expected exactly one log event, got {events:?}"
            );
            assert_eq!(events[0].level, Some(tracing::Level::WARN));
        }

        #[test]
        fn same_step_repeat_logs_debug_not_warn() {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };
            let err = ChannelError::Transport("boom".into());

            tracing::subscriber::with_default(subscriber, || {
                log_eligible_poll_failure("email", &err, &tick(false, 2));
            });

            let events = buffer.lock().unwrap();
            assert_eq!(
                events.len(),
                1,
                "expected exactly one log event, got {events:?}"
            );
            assert_eq!(events[0].level, Some(tracing::Level::DEBUG));
        }

        #[test]
        fn sustained_capped_pressure_produces_exactly_one_warn() {
            // Simulate one escalation edge followed by several repeats at the
            // same (capped) step, as the poll loop would emit them across
            // consecutive ticks -- exactly the "riding the cap" scenario
            // internal review round 1 flagged as spamming a WARN per retry.
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };
            let err = ChannelError::Auth("authenticated but not connected".into());

            tracing::subscriber::with_default(subscriber, || {
                log_eligible_poll_failure("email", &err, &tick(true, 8)); // escalation edge
                for attempt in 9..=15 {
                    log_eligible_poll_failure("email", &err, &tick(false, attempt));
                    // repeats at cap
                }
            });

            let events = buffer.lock().unwrap();
            let warn_count = events
                .iter()
                .filter(|e| e.level == Some(tracing::Level::WARN))
                .count();
            let debug_count = events
                .iter()
                .filter(|e| e.level == Some(tracing::Level::DEBUG))
                .count();
            assert_eq!(
                warn_count, 1,
                "exactly one WARN expected across the whole sequence, got {warn_count} in {events:?}"
            );
            assert_eq!(
                debug_count, 7,
                "the 7 same-step repeats must log at debug, not warn"
            );
        }

        #[test]
        fn warn_message_contains_error_text() {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };
            let err = ChannelError::Auth("IMAP LOGIN failed: slot exhausted".into());

            tracing::subscriber::with_default(subscriber, || {
                log_eligible_poll_failure("email", &err, &tick(true, 1));
            });

            let events = buffer.lock().unwrap();
            let message = events[0].message.as_deref().unwrap_or_default();
            assert!(
                message.contains("slot exhausted"),
                "escalation warn must carry the underlying error text, got: {message}"
            );
        }
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

    // --- channel_error_class / record_channel_heartbeat (khive #606) ---

    #[cfg(feature = "channel-email")]
    mod channel_heartbeat_tests {
        use super::*;
        use khive_channel::ChannelError;
        use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

        #[test]
        fn auth_maps_to_auth_class() {
            assert_eq!(channel_error_class(&ChannelError::Auth("x".into())), "auth");
        }

        #[test]
        fn transport_maps_to_transport_class() {
            assert_eq!(
                channel_error_class(&ChannelError::Transport("x".into())),
                "transport"
            );
        }

        #[test]
        fn config_maps_to_config_class() {
            assert_eq!(
                channel_error_class(&ChannelError::Config("x".into())),
                "config"
            );
        }

        #[test]
        fn unauthorized_sender_and_invalid_envelope_map_to_config_class() {
            // Never produced by poll/connect in practice (see is_backoff_eligible's doc
            // comment), but the mapping must still be total and defensible if it ever
            // does surface from a future adapter.
            assert_eq!(
                channel_error_class(&ChannelError::UnauthorizedSender("x".into())),
                "config"
            );
            assert_eq!(
                channel_error_class(&ChannelError::InvalidEnvelope("x".into())),
                "config"
            );
        }

        /// `record_channel_heartbeat` must be best-effort: a heartbeat dispatch
        /// failure (comm pack not loaded) must not panic the poll loop.
        #[tokio::test]
        async fn record_heartbeat_is_best_effort_when_comm_pack_absent() {
            let registry = VerbRegistryBuilder::new()
                .build()
                .expect("empty registry builds");

            // comm pack is not loaded, so "comm.heartbeat" is an unknown verb — this
            // must return without panicking (the caller only logs a warning).
            record_channel_heartbeat(
                &registry,
                "email",
                "recipient@example.com",
                HeartbeatOutcome::Success,
            )
            .await;
        }

        /// End-to-end: a successful poll outcome persists via `comm.heartbeat` and
        /// is readable via `comm.health` — the same wiring the live poll loop uses.
        #[tokio::test]
        async fn record_heartbeat_success_is_visible_via_comm_health() {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
            builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
            let registry = builder.build().expect("registry builds");

            record_channel_heartbeat(
                &registry,
                "email",
                "recipient@example.com",
                HeartbeatOutcome::Success,
            )
            .await;

            let health = registry
                .dispatch("comm.health", serde_json::json!({}))
                .await
                .expect("health succeeds");
            let channels = health["channels"].as_array().expect("channels array");
            assert_eq!(channels.len(), 1);
            assert_eq!(channels[0]["channel_kind"].as_str(), Some("email"));
            assert_eq!(
                channels[0]["channel_slug"].as_str(),
                Some("recipient@example.com")
            );
        }

        /// #606 round-1 internal review, Blocker finding: a daemon polling under a
        /// non-local `KHIVE_EMAIL_INGEST_NAMESPACE` must not cause a client-role
        /// no-arg `comm.health()` to report empty state. `record_channel_heartbeat`
        /// takes no `namespace` parameter — the write is unconditionally pinned to
        /// `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` — so this regression proves
        /// the heartbeat row is visible via the default (local-scoped) `comm.health`
        /// read even though this daemon's *messages* are configured to ingest into
        /// a completely different namespace.
        #[tokio::test]
        async fn heartbeat_visible_via_health_regardless_of_configured_ingest_namespace() {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
            builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
            let registry = builder.build().expect("registry builds");

            // Simulates `KHIVE_EMAIL_INGEST_NAMESPACE=lambda:mybot`: comm.ingest for
            // an inbound message would target this namespace, but the heartbeat
            // write must ignore it entirely.
            let configured_ingest_namespace = "lambda:mybot";
            let ingest_params = serde_json::json!({
                "namespace": configured_ingest_namespace,
                "from": "email:sender@example.com",
                "to": "email:recipient@example.com",
                "content": "hello",
                "channel_kind": "email",
                "external_id": "test-msg-1",
                "default_inbound_actor": "lambda:leo",
            });
            registry
                .dispatch("comm.ingest", ingest_params)
                .await
                .expect("message ingest into the configured namespace succeeds");

            record_channel_heartbeat(
                &registry,
                "email",
                "recipient@example.com",
                HeartbeatOutcome::Success,
            )
            .await;

            // A no-arg client-role comm.health() call must see the heartbeat row —
            // it must NOT report role="client" with an empty channels array just
            // because messages are configured to ingest into a non-local namespace.
            let health = registry
                .dispatch("comm.health", serde_json::json!({}))
                .await
                .expect("health succeeds");
            assert_eq!(health["role"].as_str(), Some("daemon"));
            let channels = health["channels"].as_array().expect("channels array");
            assert_eq!(
                channels.len(),
                1,
                "heartbeat row must be visible to a no-arg client comm.health() call \
                 regardless of the configured message-ingest namespace"
            );
            assert_eq!(channels[0]["channel_kind"].as_str(), Some("email"));
            assert_eq!(
                channels[0]["channel_slug"].as_str(),
                Some("recipient@example.com")
            );
        }
    }

    // --- ChannelRegistry composite-key production path (round-1 internal review, High finding) ---

    #[cfg(feature = "channel-email")]
    mod composite_key_registry_tests {
        use super::*;
        use async_trait::async_trait;
        use chrono::{DateTime, Utc};
        use khive_channel::{Channel, ChannelEnvelope, ChannelError, ChannelRegistry};
        use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
        use std::sync::Arc;

        /// Two independent inboxes that both report `kind() == "email"` but
        /// distinct `slug()` mailbox addresses, exactly like two configured
        /// `EmailChannel` credentials would.
        struct TwoMailboxChannel {
            slug: String,
        }

        #[async_trait]
        impl Channel for TwoMailboxChannel {
            fn kind(&self) -> &'static str {
                "email"
            }

            fn slug(&self) -> String {
                self.slug.clone()
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                Ok(vec![])
            }
        }

        /// #606 regression guard: a PRODUCTION `ChannelRegistry` populated
        /// through the real `register()` path (not a pack-level dispatch
        /// bypassing registration) with two same-kind, different-slug adapters.
        /// Both must be registered (not collapsed) and both must be pollable by
        /// the production poll loop, producing two independent `comm.health()`
        /// rows and two independent backoff states. This test FAILS against a
        /// `kind`-only-keyed `ChannelRegistry` (both registrations collapse to
        /// `len() == 1`) and PASSES against the `(kind, slug)`-composite-keyed
        /// registry.
        #[tokio::test]
        async fn two_same_kind_channels_both_poll_and_both_persist_health_rows() {
            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(TwoMailboxChannel {
                slug: "mailbox-a@example.com".to_string(),
            }));
            ch_registry.register(Arc::new(TwoMailboxChannel {
                slug: "mailbox-b@example.com".to_string(),
            }));
            assert_eq!(
                ch_registry.len(),
                2,
                "two same-kind, different-slug adapters must both register, not collapse"
            );

            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
            builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
            let registry = builder.build().expect("registry builds");

            // Drive exactly what the production poll loop does per tick: iterate
            // the registry and record a heartbeat for every (kind, slug, channel).
            for (kind, slug, _channel) in ch_registry.iter() {
                record_channel_heartbeat(&registry, kind, slug, HeartbeatOutcome::Success).await;
            }

            let health = registry
                .dispatch("comm.health", serde_json::json!({}))
                .await
                .expect("health succeeds");
            let channels = health["channels"].as_array().expect("channels array");
            assert_eq!(
                channels.len(),
                2,
                "both mailboxes must produce independent comm.health rows"
            );
            let slugs: std::collections::BTreeSet<&str> = channels
                .iter()
                .map(|c| c["channel_slug"].as_str().expect("channel_slug present"))
                .collect();
            assert_eq!(
                slugs,
                std::collections::BTreeSet::from([
                    "mailbox-a@example.com",
                    "mailbox-b@example.com"
                ])
            );
        }

        /// Backoff state independence: the production poll loop keys its
        /// `HashMap<(String, String), ImapBackoff>` by the same composite
        /// identity, so a failure on one mailbox must never throttle the other.
        #[test]
        fn backoff_state_is_independent_per_kind_slug_pair() {
            use khive_channel_email::ImapBackoff;
            use std::collections::HashMap;

            let mut backoffs: HashMap<(String, String), ImapBackoff> = HashMap::new();
            let key_a = ("email".to_string(), "mailbox-a@example.com".to_string());
            let key_b = ("email".to_string(), "mailbox-b@example.com".to_string());

            let tick_a = backoffs.entry(key_a.clone()).or_default().record_failure();
            assert!(
                !backoffs.contains_key(&key_b),
                "mailbox-b must have no backoff state after only mailbox-a fails"
            );
            assert!(tick_a.delay.as_secs() >= 1, "mailbox-a backoff engaged");

            // mailbox-b independently starts fresh and succeeds immediately.
            let backoff_b = backoffs.entry(key_b).or_default();
            backoff_b.record_success();
            assert_eq!(
                backoffs.get(&key_a).unwrap().attempt(),
                1,
                "mailbox-a's backoff attempt count must be unaffected by mailbox-b's success"
            );
        }
    }

    // --- note_already_delivered: outbox defensive-guard regression (round-2 finding 1) ---

    #[cfg(feature = "channel-email")]
    mod outbox_delivered_guard_tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn missing_delivered_at_is_undelivered() {
            let props = json!({}).as_object().unwrap().clone();
            assert!(!note_already_delivered(&props));
        }

        #[test]
        fn explicit_null_delivered_at_is_undelivered() {
            // Regression: a note with delivered_at explicitly set to null (e.g. via a
            // curation `update`) must be treated as undelivered, matching the query
            // predicate in list.rs — not skipped forever by `.is_some()`.
            let props = json!({ "delivered_at": null }).as_object().unwrap().clone();
            assert!(!note_already_delivered(&props));
        }

        #[test]
        fn present_non_null_delivered_at_is_delivered() {
            let props = json!({ "delivered_at": "2026-06-30T12:00:00Z" })
                .as_object()
                .unwrap()
                .clone();
            assert!(note_already_delivered(&props));
        }
    }

    // --- spawn_email_channel_loops: shared helper regression (multi-backend gap fix) ---
    //
    // Both `run` and `serve_server` call this same extracted fn (source-verified —
    // see serve.rs's `run` and `serve_server` bodies); a Rust unit test cannot assert
    // "both call sites exist" directly, so this test instead locks in that the
    // extracted helper itself is safe to call in isolation with no `KHIVE_EMAIL_*`
    // env present: it must hit the `Err` arm and return without panicking. No
    // network I/O is exercised (the missing `KHIVE_EMAIL_SMTP_HOST` fails closed
    // before any socket is opened).

    #[cfg(feature = "channel-email")]
    mod spawn_email_channel_loops_tests {
        use super::*;

        const EMAIL_ENV_VARS: [&str; 9] = [
            "KHIVE_EMAIL_SMTP_HOST",
            "KHIVE_EMAIL_IMAP_HOST",
            "KHIVE_EMAIL_USERNAME",
            "KHIVE_EMAIL_MAINTAINER_ADDRESS",
            "KHIVE_EMAIL_AUTHSERV_ID",
            "KHIVE_EMAIL_PASSWORD",
            "KHIVE_EMAIL_OAUTH_TENANT_ID",
            "KHIVE_EMAIL_OAUTH_CLIENT_ID",
            "KHIVE_EMAIL_OAUTH_CLIENT_SECRET",
        ];

        /// RAII guard: snapshots each `KHIVE_EMAIL_*` var's current value, clears it,
        /// and restores the original value (or leaves it removed) on drop — including
        /// on panic, so a failing assertion never leaks env taint to later tests.
        struct EmailEnvGuard {
            snapshot: Vec<(&'static str, Option<String>)>,
        }

        impl EmailEnvGuard {
            fn clear() -> Self {
                let snapshot = EMAIL_ENV_VARS
                    .iter()
                    .map(|&var| (var, std::env::var(var).ok()))
                    .collect();
                for var in EMAIL_ENV_VARS {
                    std::env::remove_var(var);
                }
                Self { snapshot }
            }
        }

        impl Drop for EmailEnvGuard {
            fn drop(&mut self) {
                for (var, prev) in &self.snapshot {
                    match prev {
                        Some(v) => std::env::set_var(var, v),
                        None => std::env::remove_var(var),
                    }
                }
            }
        }

        #[tokio::test]
        #[serial]
        async fn missing_env_hits_err_arm_without_panic() {
            let _env_guard = EmailEnvGuard::clear();

            // Prove the branch the helper depends on is actually taken: with every
            // KHIVE_EMAIL_* var cleared, EmailChannel::from_env() must fail closed.
            // Without this, the test below would pass even if from_env() wrongly hit
            // the Ok arm (it only checks "no panic").
            assert!(
                khive_channel_email::EmailChannel::from_env().is_err(),
                "with KHIVE_EMAIL_* cleared, from_env must fail closed (the Err arm the helper depends on)"
            );

            let config = RuntimeConfig {
                db_path: None,
                default_namespace: Namespace::parse("test").unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
            let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

            // Must not panic: EmailChannel::from_env() fails closed on the missing
            // KHIVE_EMAIL_SMTP_HOST and the fn logs a warning and returns.
            spawn_email_channel_loops(&server);
        }

        /// Regression for #602: `spawn_email_channel_loops_if_daemon` is the
        /// SAME wrapper `run` and `serve_server` call (source-verified — see
        /// those fns' bodies above) — no reimplementation of the role check
        /// here. Actual tokio task spawning cannot be observed from a unit
        /// test, so this pair instead exercises `is_daemon_role` (the pure
        /// predicate) directly against real `Args` values, and drives the
        /// production wrapper through both roles to prove neither branch
        /// panics — the same "no-panic" scope the sibling test above uses.
        #[test]
        fn is_daemon_role_true_for_daemon_args() {
            use clap::Parser;
            let args = Args::parse_from(["mcp", "--daemon"]);
            assert!(
                is_daemon_role(&args),
                "--daemon must resolve to daemon role"
            );
        }

        #[test]
        fn is_daemon_role_false_for_client_args() {
            use clap::Parser;
            let args = Args::parse_from(["mcp"]);
            assert!(
                !is_daemon_role(&args),
                "a plain stdio client (no --daemon) must not resolve to daemon role"
            );
        }

        #[tokio::test]
        #[serial]
        async fn daemon_role_gate_spawns_without_panic() {
            use clap::Parser;
            let _env_guard = EmailEnvGuard::clear();
            let args = Args::parse_from(["mcp", "--daemon"]);

            let config = RuntimeConfig {
                db_path: None,
                default_namespace: Namespace::parse("test").unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
            let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

            // Daemon role: the wrapper must take the spawn branch (still fails
            // closed on missing KHIVE_EMAIL_* — no network I/O — but must not
            // panic reaching it).
            spawn_email_channel_loops_if_daemon(&server, &args);
        }

        #[tokio::test]
        #[serial]
        async fn client_role_gate_skips_without_panic() {
            use clap::Parser;
            let _env_guard = EmailEnvGuard::clear();
            let args = Args::parse_from(["mcp"]);

            let config = RuntimeConfig {
                db_path: None,
                default_namespace: Namespace::parse("test").unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
            let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

            // Client role: the wrapper must take the skip branch and never
            // attempt to construct an EmailChannel at all.
            spawn_email_channel_loops_if_daemon(&server, &args);
        }
    }
}
