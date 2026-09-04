//! khived daemon client — forwarding + auto-spawn.
//!
//! The daemon server lives in `khive-runtime::daemon`. This module provides the
//! client side: [`forward_or_spawn`] connects to the daemon, auto-spawns it on
//! first use, and maps responses to MCP error types. Ordinary fallback paths
//! return `None` so the caller can dispatch locally. `KHIVE_DAEMON_STRICT=1`
//! (#947) is the exception: it turns a recordable fallback into a
//! caller-visible per-op error instead, via `fallback_or_reject`.
//! `KHIVE_NO_DAEMON` and `crate::server`'s `save_to` bypass remain
//! intentional, unconditional local paths — neither is affected by strict
//! mode, since nothing is ever recorded or falls back for them.
//!
//! Also provides the [`khive_runtime::daemon::DaemonDispatch`] impl for [`crate::server::KhiveMcpServer`].

use std::process::Stdio;

use async_trait::async_trait;
use khive_runtime::daemon::{
    self, acquire_recovery_lock, env_truthy, pid_path, read_frame, socket_path, write_frame,
    DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
use rmcp::ErrorData as McpError;
use sha2::{Digest, Sha256};
use tokio::net::UnixStream;

use crate::tools::request::RequestParams;

#[cfg(test)]
mod test_harness;

#[cfg(test)]
use test_harness::{
    reset_counters, DAEMON_DISPATCH, FORCED_CONNECT_ERROR, FORCE_PID_IS_DAEMON,
    FORCE_PID_IS_FOREIGN, KILL_COUNT, RECOVERY_RACE_BARRIER, SPAWN_COUNT,
};

// ── local-dispatch fallback telemetry ─────────────────────────────────────────
//
// Each recordable fallback path below (the paths through `fallback_or_reject`)
// means the caller silently dispatches locally instead of via the warm daemon.
// Intentional local bypasses such as the `KHIVE_NO_DAEMON` opt-out are outside
// this telemetry set by design.
// A silent fallback is the bug this instrumentation exists to surface: it must
// always be loud (a structured fallback event — WARN, or ERROR for
// strict-mode illegitimate reasons) and counted. These are process-global
// production counters (not `#[cfg(test)]`-gated like the instrumentation seams
// above) — they realize the metric `khive_daemon_fallback_total{reason}`; a
// future metrics-export slice can read them without touching call sites.

/// Reason a request fell back to local dispatch instead of the warm daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackReason {
    ConfigMismatch,
    NamespaceMismatch,
    NoSocket,
    // REASON: #644 made "the real frame was already written" a hard error
    // (never a local-dispatch fallback) in `forward_or_spawn`, since retrying
    // or falling back after a completed write risks a duplicate mutation. That
    // removed the only production call sites for these two reasons. They stay
    // in the closed 5-value metrics set for forward-compatibility (a future
    // reason may reuse the same severity tier) and are exercised directly by
    // the counter tests below, matching the existing no-production-call-site
    // pattern already used for `fallback_total`/`fallback_count` in this file.
    #[allow(dead_code)]
    ParseFailure,
    #[allow(dead_code)]
    ProtocolMismatch,
}

impl FallbackReason {
    fn as_str(self) -> &'static str {
        match self {
            FallbackReason::ConfigMismatch => "config_mismatch",
            FallbackReason::NamespaceMismatch => "namespace_mismatch",
            FallbackReason::NoSocket => "no_socket",
            FallbackReason::ParseFailure => "parse_failure",
            FallbackReason::ProtocolMismatch => "protocol_mismatch",
        }
    }

    fn counter(self) -> &'static std::sync::atomic::AtomicUsize {
        match self {
            FallbackReason::ConfigMismatch => &FALLBACK_CONFIG_MISMATCH,
            FallbackReason::NamespaceMismatch => &FALLBACK_NAMESPACE_MISMATCH,
            FallbackReason::NoSocket => &FALLBACK_NO_SOCKET,
            FallbackReason::ParseFailure => &FALLBACK_PARSE_FAILURE,
            FallbackReason::ProtocolMismatch => &FALLBACK_PROTOCOL_MISMATCH,
        }
    }

    /// Legitimacy tier used only by the `daemon_fallback` event-level policy
    /// (see `crates/khive-mcp/docs/api/daemon-lifecycle.md` §"Strict-mode
    /// fallback accounting"; the graduated tiers themselves come from ADR-049
    /// Amendment 2). Every tier increments its per-reason counter and emits
    /// exactly one `daemon_fallback` event: WARN normally, escalated to ERROR
    /// plus `FALLBACK_STRICT_VIOLATIONS` only for `Illegitimate` reasons in
    /// strict mode.
    fn severity(self) -> FallbackSeverity {
        match self {
            // A real misconfiguration: the client and daemon should have
            // agreed on `config_id`/namespace visibility and didn't. Never
            // expected on a correctly-configured fleet post-D1.
            FallbackReason::ConfigMismatch | FallbackReason::NamespaceMismatch => {
                FallbackSeverity::Illegitimate
            }
            // `ParseFailure` and `ProtocolMismatch` retain their historical
            // rollout-transient telemetry tier, but neither is a production
            // fallback anymore. Once the real frame is fully written, both
            // outcomes are terminal exactly-once errors: no retry, local
            // dispatch, kill, or respawn (#644/#539). The variants stay in the
            // closed metrics vocabulary for wire compatibility.
            FallbackReason::ProtocolMismatch | FallbackReason::ParseFailure => {
                FallbackSeverity::RolloutTransient
            }
            // The socket is definitively absent/refused — the ADR-049-mandated
            // fallback path. Access-denied/indeterminate connect failures are
            // terminal and never enter this metrics tier (#1242).
            FallbackReason::NoSocket => FallbackSeverity::NoDaemon,
        }
    }
}

/// Legitimacy tier for a [`FallbackReason`], keyed by the graduated fail-loud
/// policy in ADR-049 Amendment 2. See [`FallbackReason::severity`] for the
/// per-variant mapping and its rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackSeverity {
    /// A real misconfiguration. Elevated to an error-level event (plus a
    /// dedicated violation counter) when `KHIVE_DAEMON_STRICT=1`.
    Illegitimate,
    /// Historical rollout-transient telemetry tier. These outcomes are now
    /// terminal after a real write, but remain in the closed metrics vocabulary.
    /// Never elevated past the WARN-level event, in strict mode or otherwise.
    RolloutTransient,
    /// No daemon to forward to at all. Never elevated past the WARN-level
    /// event — this is the ADR-049-mandated safety-net telemetry tier, not a
    /// signal that strict mode permits local fallback.
    NoDaemon,
}

/// `KHIVE_DAEMON_STRICT=1` has two independent effects:
///
/// - Behavior: [`fallback_or_reject`] rejects every [`FallbackReason`] instead
///   of completing the request locally, regardless of severity (#947).
/// - Telemetry: [`record_fallback`] elevates only `Illegitimate` reasons to an
///   error-level event with the violation counter (D2-R1); the other severity
///   tiers remain WARN-level `daemon_fallback` events and do not increment
///   `FALLBACK_STRICT_VIOLATIONS`.
///
/// Plain opt-in, default OFF — no hosted-vs-local auto-detection exists. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
fn is_daemon_strict_mode() -> bool {
    env_truthy("KHIVE_DAEMON_STRICT")
}

/// `khive_daemon_fallback_total{reason="config_mismatch"}`
static FALLBACK_CONFIG_MISMATCH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// `khive_daemon_fallback_total{reason="namespace_mismatch"}`
static FALLBACK_NAMESPACE_MISMATCH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// `khive_daemon_fallback_total{reason="no_socket"}`
static FALLBACK_NO_SOCKET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// `khive_daemon_fallback_total{reason="parse_failure"}`
static FALLBACK_PARSE_FAILURE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// `khive_daemon_fallback_total{reason="protocol_mismatch"}`
static FALLBACK_PROTOCOL_MISMATCH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// `khive_daemon_fallback_strict_violations_total` — count of `Illegitimate`
/// fallbacks (`config_mismatch`/`namespace_mismatch`) observed while
/// `KHIVE_DAEMON_STRICT=1` was set (D2-R1). Distinct from the five
/// per-reason counters above: this one is scoped to exactly the elevated
/// (error-level) events, so a load-harness (D2-R2) can hard-fail on
/// "any nonzero" without having to know which reasons are illegitimate.
static FALLBACK_STRICT_VIOLATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// REASON: these accessors have no production call site yet — this slice adds the
// counters and their read path; a future metrics-export slice wires them to a real
// exporter (see the `khive_daemon_fallback_total{reason}` doc comments above) without
// needing to touch `record_fallback`'s call sites. Exercised directly by the counter
// tests below in the meantime.
#[allow(dead_code)]
/// `khive_daemon_fallback_total{reason="<all>"}` — sums the five reason
/// counters on read rather than tracking a separate atomic, so
/// total == sum-of-reasons is a structural invariant. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
pub(crate) fn fallback_total() -> usize {
    use std::sync::atomic::Ordering::SeqCst;
    FALLBACK_CONFIG_MISMATCH.load(SeqCst)
        + FALLBACK_NAMESPACE_MISMATCH.load(SeqCst)
        + FALLBACK_NO_SOCKET.load(SeqCst)
        + FALLBACK_PARSE_FAILURE.load(SeqCst)
        + FALLBACK_PROTOCOL_MISMATCH.load(SeqCst)
}

#[allow(dead_code)]
/// Fallback count for a single `reason`.
pub(crate) fn fallback_count(reason: FallbackReason) -> usize {
    reason.counter().load(std::sync::atomic::Ordering::SeqCst)
}

#[allow(dead_code)]
/// `khive_daemon_fallback_strict_violations_total` — see
/// [`FALLBACK_STRICT_VIOLATIONS`]. No production call site yet, same as
/// `fallback_total`/`fallback_count` above; exercised directly by tests.
pub(crate) fn fallback_strict_violations() -> usize {
    FALLBACK_STRICT_VIOLATIONS.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn reset_fallback_counters() {
    use std::sync::atomic::Ordering::SeqCst;
    FALLBACK_CONFIG_MISMATCH.store(0, SeqCst);
    FALLBACK_NAMESPACE_MISMATCH.store(0, SeqCst);
    FALLBACK_NO_SOCKET.store(0, SeqCst);
    FALLBACK_PARSE_FAILURE.store(0, SeqCst);
    FALLBACK_PROTOCOL_MISMATCH.store(0, SeqCst);
    FALLBACK_STRICT_VIOLATIONS.store(0, SeqCst);
}

struct ConfigIdFields<'a> {
    packs: &'a str,
    db: &'a str,
    embed: &'a str,
    extra: &'a str,
    fresh_tail: &'a str,
    blob_hydration_bytes: &'a str,
    backend: &'a str,
    outbound: &'a str,
    gate: &'a str,
    git_write: &'a str,
    display_timezone: &'a str,
    backends: Option<&'a str>,
    pack_backends: Option<&'a str>,
}

fn parse_config_id(config_id: &str) -> Option<ConfigIdFields<'_>> {
    let (base, backends, pack_backends) =
        if let Some((before_routing, routing)) = config_id.rsplit_once("];pack_backends=[") {
            let pack_backends = routing.strip_suffix(']')?;
            let (base, backends) = before_routing.rsplit_once(";backends=[")?;
            (base, Some(backends), Some(pack_backends))
        } else {
            (config_id, None, None)
        };

    let base = base.strip_prefix("packs=[")?;
    let (packs, rest) = base.split_once("];db=")?;
    let (rest, display_timezone) = rest
        .rsplit_once(";display_tz=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, git_write) = rest.rsplit_once(";git_write=")?;
    let (rest, gate) = rest
        .rsplit_once(";gate=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, outbound) = rest.rsplit_once(";outbound=[")?;
    let outbound = outbound.strip_suffix(']')?;
    let (rest, backend) = rest.rsplit_once(";backend=")?;
    // Parse pre-ADR-160 fingerprints too so an incumbent from the preceding
    // release reports the newly construction-baked budget as the mismatch,
    // rather than degrading an otherwise recognizable identity to `unknown`.
    let (rest, blob_hydration_bytes) = rest
        .rsplit_once(";blob_hydration_bytes=")
        .unwrap_or((rest, "<legacy-absent>"));
    let (rest, fresh_tail) = rest.rsplit_once(";fresh_tail=")?;
    let (rest, extra) = rest.rsplit_once(";extra=[")?;
    let extra = extra.strip_suffix(']')?;
    let (db, embed) = rest.rsplit_once(";embed=")?;

    Some(ConfigIdFields {
        packs,
        db,
        embed,
        extra,
        fresh_tail,
        blob_hydration_bytes,
        backend,
        outbound,
        gate,
        git_write,
        display_timezone,
        backends,
        pack_backends,
    })
}

fn first_config_mismatch_field(client: &str, daemon: Option<&str>) -> &'static str {
    let Some(daemon) = daemon else {
        return "unknown";
    };
    let (Some(client), Some(daemon)) = (parse_config_id(client), parse_config_id(daemon)) else {
        return "unknown";
    };

    if client.packs != daemon.packs {
        "packs"
    } else if client.db != daemon.db {
        "db"
    } else if client.embed != daemon.embed {
        "embed"
    } else if client.extra != daemon.extra {
        "extra"
    } else if client.fresh_tail != daemon.fresh_tail {
        "fresh_tail"
    } else if client.blob_hydration_bytes != daemon.blob_hydration_bytes {
        "blob_hydration_bytes"
    } else if client.backend != daemon.backend {
        "backend"
    } else if client.outbound != daemon.outbound {
        "outbound"
    } else if client.gate != daemon.gate {
        "gate"
    } else if client.git_write != daemon.git_write {
        "git_write"
    } else if client.display_timezone != daemon.display_timezone {
        "display_tz"
    } else if client.backends != daemon.backends {
        "backends"
    } else if client.pack_backends != daemon.pack_backends {
        "pack_backends"
    } else {
        "unknown"
    }
}

fn opaque_config_id(config_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"khive.daemon-config-diagnostic.v1\0");
    hasher.update(config_id.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Emit the standardized `daemon_fallback` event and increment the matching
/// counters. Call exactly once per fallback event, at the point where the
/// caller is about to dispatch locally instead of via the warm daemon.
/// Log level/counter are graduated by [`FallbackReason::severity`] and
/// [`is_daemon_strict_mode`] (D2-R1/D2-R3). This function only records; it
/// never decides whether the caller proceeds locally — every call site
/// pairs it with `fallback_or_reject` (#947). See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
fn record_fallback(
    reason: FallbackReason,
    config_id_client: &str,
    config_id_daemon: Option<&str>,
    namespace_client: &str,
) {
    reason
        .counter()
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let strict_violation =
        reason.severity() == FallbackSeverity::Illegitimate && is_daemon_strict_mode();
    let diagnostic_config_id_client = opaque_config_id(config_id_client);
    let diagnostic_config_id_daemon = config_id_daemon
        .map(opaque_config_id)
        .unwrap_or_else(|| "none".to_string());
    let config_mismatch_field = if reason == FallbackReason::ConfigMismatch {
        first_config_mismatch_field(config_id_client, config_id_daemon)
    } else {
        "not_applicable"
    };

    if strict_violation {
        FALLBACK_STRICT_VIOLATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tracing::error!(
            reason = reason.as_str(),
            config_id_client = %diagnostic_config_id_client,
            config_id_daemon = %diagnostic_config_id_daemon,
            config_mismatch_field,
            namespace_client,
            pid = std::process::id(),
            strict = true,
            "daemon_fallback"
        );
    } else {
        tracing::warn!(
            reason = reason.as_str(),
            config_id_client = %diagnostic_config_id_client,
            config_id_daemon = %diagnostic_config_id_daemon,
            config_mismatch_field,
            namespace_client,
            pid = std::process::id(),
            "daemon_fallback"
        );
    }
}

/// #947: the single decision point for what a caller sees when a request
/// would fall back to local dispatch. Always records `reason` via
/// [`record_fallback`] first, then under `KHIVE_DAEMON_STRICT=1` rejects the
/// request instead of letting it complete locally — every `FallbackReason`
/// is rejected, not just the `Illegitimate` tier. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
///
/// `STRICT_FALLBACK_MARKER` tags a strict-fallback rejection's [`McpError`]
/// so `request()` in `server.rs` can tell it apart from every other
/// daemon-forward `McpError`, which stay RPC-level errors.
pub(crate) const STRICT_FALLBACK_MARKER: &str = "khive_strict_daemon_fallback";

/// Call exactly where the caller was about to `return None` (local dispatch)
/// after a fallback; every production call site returns this directly.
fn fallback_or_reject(
    reason: FallbackReason,
    config_id_client: &str,
    config_id_daemon: Option<&str>,
    namespace_client: &str,
) -> Option<Result<String, McpError>> {
    record_fallback(reason, config_id_client, config_id_daemon, namespace_client);
    if is_daemon_strict_mode() {
        return Some(Err(McpError::internal_error(
            format!(
                "daemon fallback rejected under KHIVE_DAEMON_STRICT=1: reason={}; \
                 refusing to complete the request via local dispatch",
                reason.as_str()
            ),
            Some(serde_json::json!({
                STRICT_FALLBACK_MARKER: true,
                "reason": reason.as_str(),
            })),
        )));
    }
    None
}

// ── DaemonDispatch impl ───────────────────────────────────────────────────────

#[async_trait]
impl daemon::DaemonDispatch for crate::server::KhiveMcpServer {
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, String> {
        let params = RequestParams {
            ops,
            presentation,
            presentation_per_op,
            save_to: None,
            format,
            format_per_op,
            request_id: None,
        };
        // Honor the frame's origin: a wire-origin request enforces verb
        // visibility even when served by the daemon; an operator request does not.
        // `identity` (ADR-096 Fork 1) is the caller's per-request identity
        // context, built by `handle_conn` from the frame — threaded straight
        // through so this call serves under the CALLER's namespace/actor
        // rather than this server's own construction-baked identity.
        self.dispatch_request_inner(
            params,
            from_wire,
            identity,
            crate::server::DispatchOrigin::Daemon,
        )
        .await
        .map_err(|e| e.message.to_string())
    }

    async fn warm_all(&self) {
        crate::server::KhiveMcpServer::warm_all(self).await;
    }

    fn namespace(&self) -> &str {
        self.default_namespace()
    }

    fn config_id(&self) -> &str {
        crate::server::KhiveMcpServer::config_id(self)
    }

    fn pool_for_checkpoint(&self) -> Option<std::sync::Arc<khive_db::ConnectionPool>> {
        self.pool()
    }

    fn secondary_pools_for_checkpoint(&self) -> Vec<std::sync::Arc<khive_db::ConnectionPool>> {
        self.secondary_pools()
    }

    fn event_store_for_checkpoint(&self) -> Option<std::sync::Arc<dyn khive_storage::EventStore>> {
        self.event_store()
    }
}

// ── client ────────────────────────────────────────────────────────────────────

/// Result of a single forward attempt to the daemon socket.
enum ForwardOutcome {
    /// Successfully received and decoded a response frame.
    Response(Box<DaemonResponseFrame>),
    /// The socket is absent/refused, or the frame failed before it could reach
    /// dispatch. These outcomes are safe to route through recovery.
    NoSocket,
    /// This process could not establish whether a daemon is listening. An OS
    /// access/policy failure is not proof that the daemon is absent, so it must
    /// never enter lifecycle recovery or local fallback (#1242).
    Unreachable {
        kind: std::io::ErrorKind,
        os_error_code: Option<i32>,
    },
    /// Connected but the response could not be decoded — most likely a stale
    /// daemon speaking a different wire format.
    ParseFailure,
    /// Connected and decoded a response, but the daemon's `daemon_protocol_version`
    /// does not match [`PROTOCOL_VERSION`] even though `version_mismatch` is false.
    /// This is the new-client + old-daemon (pre-versioning) scenario: the old daemon
    /// ignores the unknown request field and returns a decodable response whose
    /// protocol fields default to `false`/`0`. Since the real request was already
    /// written, the client must treat this exactly like `ParseFailure`: return a
    /// hard error without retrying, locally dispatching, killing, or respawning.
    ProtocolMismatch,
}

fn classify_socket_connect_error(error: std::io::Error) -> ForwardOutcome {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ) {
        ForwardOutcome::NoSocket
    } else {
        ForwardOutcome::Unreachable {
            kind: error.kind(),
            os_error_code: error.raw_os_error(),
        }
    }
}

async fn try_forward_inner(frame: &DaemonRequestFrame) -> ForwardOutcome {
    let sock = socket_path();
    #[cfg(test)]
    {
        let forced_error = FORCED_CONNECT_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        if forced_error != 0 {
            return classify_socket_connect_error(std::io::Error::from_raw_os_error(forced_error));
        }
    }
    let mut stream = match UnixStream::connect(&sock).await {
        Ok(s) => s,
        Err(error) => return classify_socket_connect_error(error),
    };
    let payload = match serde_json::to_vec(frame) {
        Ok(p) => p,
        Err(_) => return ForwardOutcome::NoSocket,
    };
    if write_frame(&mut stream, &payload).await.is_err() {
        return ForwardOutcome::NoSocket;
    }
    // The request is now written and may already be executing (or committed)
    // on the daemon side, so a stalled or unresponsive daemon must not leave
    // this read pending forever — that would keep the calling task (and the
    // MCP handler awaiting it) alive indefinitely. Reuse the same read
    // ceiling the bridge already applies to its own request-owned work
    // (`KHIVE_REQUEST_READ_TIMEOUT_SECS`, default 30s) rather than adding a
    // second, independently-tuned timeout knob.
    let resp = match tokio::time::timeout(
        khive_storage::request_read_timeout_from_env(),
        read_frame(&mut stream),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            // The request was sent but the daemon closed the connection before
            // sending a response frame — likely a daemon crash or panic during
            // dispatch. Treat as ParseFailure (not NoSocket): the request may
            // already have committed, so the caller must return a terminal
            // ambiguity error without lifecycle actions or local dispatch.
            tracing::warn!(
                error = %e,
                "daemon closed connection without sending a response \
                 (crash during dispatch?) — returning terminal ambiguity"
            );
            return ForwardOutcome::ParseFailure;
        }
        Err(_elapsed) => {
            // The daemon never answered within the read ceiling. The write
            // already completed, so — exactly like the connection-closed case
            // above — the request may already have committed; there is no
            // safe retry or local fallback, only a terminal ambiguity error.
            tracing::warn!(
                "daemon response read timed out after the request was fully \
                 written — returning terminal ambiguity"
            );
            return ForwardOutcome::ParseFailure;
        }
    };
    match serde_json::from_slice::<DaemonResponseFrame>(&resp) {
        Ok(frame) => {
            // A pre-versioning daemon (old-daemon scenario) returns a decodable
            // response whose `daemon_protocol_version` defaults to 0. The field
            // `version_mismatch` is also false because the old daemon never set
            // it — making `map_response` accept the stale response when
            // `served_config_id` happens to match. Detect this here so the
            // caller can route it through the same terminal no-retry path.
            //
            // Also catch the explicit-mismatch / auto-upgrade case (#156): when a
            // warm OLD daemon receives a request from a NEWER client it responds
            // with `version_mismatch=true` and its own (lower) version number.
            // `daemon_protocol_version < PROTOCOL_VERSION` means the daemon is
            // stale — route through the same terminal error as the implicit case
            // above. If `daemon_protocol_version > PROTOCOL_VERSION` the client
            // binary is behind; let `map_response` return its hard error too.
            let is_stale_daemon = frame.daemon_protocol_version != PROTOCOL_VERSION
                && (!frame.version_mismatch || frame.daemon_protocol_version < PROTOCOL_VERSION);
            if is_stale_daemon {
                tracing::warn!(
                    daemon_version = frame.daemon_protocol_version,
                    expected = PROTOCOL_VERSION,
                    explicit_mismatch = frame.version_mismatch,
                    "daemon protocol version mismatch after request write — rejecting without retry",
                );
                return ForwardOutcome::ProtocolMismatch;
            }
            ForwardOutcome::Response(Box::new(frame))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                bytes = resp.len(),
                "daemon response could not be decoded after request write on {}",
                sock.display()
            );
            ForwardOutcome::ParseFailure
        }
    }
}

fn map_response(
    resp: DaemonResponseFrame,
    expected_config_id: &str,
    namespace_client: &str,
) -> Option<Result<String, McpError>> {
    // Protocol version mismatch is a hard error — do NOT fall back to local
    // dispatch, which would hide the skew. Surface the daemon's own message.
    if resp.version_mismatch {
        let msg = resp.error.unwrap_or_else(|| {
            format!(
                "daemon protocol mismatch: client={} daemon={} — \
                 rebuild/update the client binary (make local)",
                PROTOCOL_VERSION, resp.daemon_protocol_version,
            )
        });
        return Some(Err(McpError::internal_error(msg, None)));
    }

    if resp.namespace_mismatch {
        return fallback_or_reject(
            FallbackReason::NamespaceMismatch,
            expected_config_id,
            resp.served_config_id.as_deref(),
            namespace_client,
        );
    }
    if resp.config_mismatch {
        return fallback_or_reject(
            FallbackReason::ConfigMismatch,
            expected_config_id,
            resp.served_config_id.as_deref(),
            namespace_client,
        );
    }
    // Fail closed: only trust a result the daemon positively confirms it served
    // under our exact config. A legacy daemon omits `served_config_id` (→ None)
    // and a config-drifted daemon echoes a different id — both fall back local.
    if resp.served_config_id.as_deref() != Some(expected_config_id) {
        return fallback_or_reject(
            FallbackReason::ConfigMismatch,
            expected_config_id,
            resp.served_config_id.as_deref(),
            namespace_client,
        );
    }
    if resp.ok {
        Some(Ok(resp.result.unwrap_or_default()))
    } else {
        let msg = resp.error.unwrap_or_else(|| {
            format!(
                "daemon returned an error without a message \
                 (code: internal_error; daemon config: {})",
                resp.served_config_id.as_deref().unwrap_or("unknown"),
            )
        });
        Some(Err(McpError::internal_error(msg, None)))
    }
}

/// Cap on `khived.log` size (bytes) before a spawn rotates it to `khived.log.1`.
///
/// Rotation only happens at spawn time (never mid-session): the daemon is
/// respawned often enough (rebuilds, reconnects, stale-daemon recovery) that
/// this alone bounds disk use, without pulling in `tracing-appender` or
/// touching `init_tracing`'s writer.
const DAEMON_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Resolve `<home>/.khive/logs/khived.log` given an explicit `HOME` value.
///
/// Takes the HOME value as a parameter (rather than reading the environment
/// directly) so the resolution logic is unit-testable without mutating
/// process-global state. Mirrors the `Path::new(&home).join(...)` idiom used
/// for `~/.khive/.env` resolution in `kkernel`'s `load_khive_dotenv`.
fn daemon_log_path_from_home(home: Option<&std::ffi::OsStr>) -> Option<std::path::PathBuf> {
    let home = home?;
    Some(
        std::path::Path::new(home)
            .join(".khive")
            .join("logs")
            .join("khived.log"),
    )
}

/// Resolve the daemon log path from the real process environment. Returns
/// `None` when `HOME` is unset — the caller falls back to discarding the
/// daemon's stderr rather than failing the spawn.
fn daemon_log_path() -> Option<std::path::PathBuf> {
    daemon_log_path_from_home(std::env::var_os("HOME").as_deref())
}

/// Decide whether the log at `current_size` bytes must rotate before this
/// spawn, given a `cap` in bytes. Pulled out as a pure function so the
/// spawn-time rotation policy is unit-testable independent of the filesystem.
fn daemon_log_should_rotate(current_size: u64, cap: u64) -> bool {
    current_size >= cap
}

/// Prepare `log_path` for the daemon's stderr: create its parent directory,
/// rotate the existing file to `<name>.1` (replacing any prior backup) if it
/// is at or over `cap` bytes, then open (or create) it for append.
///
/// Returns `None` on directory-creation or open failure so the caller can
/// fall back to `Stdio::null()`. A rotation (`rename`) failure is deliberately
/// swallowed and degrades to appending to the existing over-cap file — keeping
/// the daemon's stderr flowing to a slightly-too-large log beats losing it.
/// Logging is best-effort; daemon spawn correctness is not, and the daemon is
/// on the hot path for every MCP request.
fn prepare_daemon_log_file_with_cap(log_path: &std::path::Path, cap: u64) -> Option<std::fs::File> {
    let dir = log_path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    if let Ok(meta) = std::fs::metadata(log_path) {
        if daemon_log_should_rotate(meta.len(), cap) {
            let backup = dir.join("khived.log.1");
            // `rename` replaces an existing destination atomically on Unix —
            // exactly the "replace any prior .1" behavior we want.
            let _ = std::fs::rename(log_path, &backup);
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .ok()
}

/// [`prepare_daemon_log_file_with_cap`] using the standing [`DAEMON_LOG_MAX_BYTES`] cap.
fn prepare_daemon_log_file(log_path: &std::path::Path) -> Option<std::fs::File> {
    prepare_daemon_log_file_with_cap(log_path, DAEMON_LOG_MAX_BYTES)
}

fn spawn_daemon() -> std::io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    spawn_daemon_with_exe(&exe)
}

fn spawn_daemon_with_exe(exe: &std::path::Path) -> std::io::Result<std::process::Child> {
    spawn_daemon_with_exe_and_config(exe, None, None, None)
}

fn spawn_daemon_with_exe_and_config(
    exe: &std::path::Path,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
) -> std::io::Result<std::process::Child> {
    #[cfg(test)]
    SPAWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    // The binary is `kkernel`; the MCP server (and its daemon mode) live under
    // the `mcp` subcommand.
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("mcp").arg("--daemon");
    if let Some(path) = config {
        cmd.arg("--config").arg(path);
    }
    // Forward whatever override the caller hands over — the caller owns the
    // decision of WHICH override a spawned daemon must be constructed with
    // (`run_exec_inline_with_forward` in `crates/kkernel/src/exec.rs`):
    //
    // - `:memory:` always forwards: it is the one override a newly spawned
    //   daemon can honor byte-identically to the client (ephemeral by
    //   definition), and without it the fresh daemon would bind the config's
    //   declared persistent backend files instead — the opposite of what the
    //   operator requested.
    // - A CONCRETE path forwards in the single-backend case (no
    //   `[[backends]]` declared): the spawned daemon has no config-declared
    //   database path to default to, so without the override it would bind
    //   `$HOME/.khive/khive.db` and its `config_id` would never match the
    //   client's override-anchored frame.
    // - A redundant concrete override (multi-backend, proven to name the
    //   declared `main` backend) is deliberately NOT passed here by the
    //   caller: the spawned daemon's config-declared path IS that override's
    //   target, and the client's `config_id` has already been normalized to
    //   the no-override anchor — forwarding it would desync the child's
    //   fingerprint from the normalized frame.
    if let Some(db) = db {
        cmd.arg("--db").arg(db);
    }
    // Forward the spawning client's already-resolved pack set explicitly
    // rather than relying on ambient `KHIVE_PACKS` env inheritance: the
    // client may have resolved packs from a CLI `--pack` flag or a
    // discovered `[runtime].packs` config entry, neither of which travels
    // through `Command::new`'s default env inheritance. Without this, a
    // freshly spawned daemon falls back to the built-in default pack set,
    // its `config_id` fingerprint disagrees with every caller expecting the
    // wider set, and those callers permanently fall back to in-process
    // dispatch instead of the warm daemon (khive-oss#1941).
    if let Some(packs) = packs {
        for pack in packs {
            cmd.arg("--pack").arg(pack);
        }
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    // The daemon's tracing (including WAL/checkpoint telemetry) goes to
    // stderr honoring KHIVE_LOG (init_tracing in kkernel's main.rs) — wiring
    // it to /dev/null silently discards all of it. Route it to a log file
    // instead; fall back to null on any resolution/creation failure so a
    // logging problem never breaks the daemon spawn itself.
    match daemon_log_path().and_then(|path| prepare_daemon_log_file(&path)) {
        Some(file) => {
            cmd.stderr(file);
        }
        None => {
            cmd.stderr(Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // #898: return the live `Child` (rather than discarding it) so the caller
    // can positively confirm the respawned process is still alive before
    // treating recovery as healthy — see `RecoveryOutcome::Spawned` and its
    // use in `forward_or_spawn`. A binary that predates or otherwise rejects
    // `mcp --daemon` (version skew) exits immediately with a clap parse
    // error; without this handle that failure was invisible to everything
    // except `khived.log`.
    // A just-written executable can transiently fail `execve(2)` with
    // ETXTBSY on instrumented/contended filesystems. This is especially easy
    // to hit in the argv-forwarding tests, but it can also occur while a real
    // installation is atomically replacing `kkernel`. Retry only that precise
    // error, with a short finite budget; every other spawn failure remains
    // immediate and unchanged.
    spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || cmd.spawn())
}

/// Short finite backoff budget for a transient `ExecutableFileBusy` spawn
/// failure — see the call site in [`spawn_daemon_with_exe_and_config`].
const EXECUTABLE_BUSY_BACKOFF_MS: [u64; 3] = [5, 20, 50];

/// Retry `spawn` up to `delays_ms.len()` extra times, sleeping the
/// corresponding delay between attempts, but only when the failure is
/// `ErrorKind::ExecutableFileBusy`. Any other error — or the final attempt
/// after the backoff budget is exhausted — is returned immediately.
fn spawn_retrying_executable_busy<T>(
    delays_ms: &[u64],
    mut spawn: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    for delay_ms in delays_ms {
        match spawn() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
            }
            outcome => return outcome,
        }
    }
    spawn()
}

/// Return `true` if `args` (the full `ps -o args=` output for a process)
/// identifies a khive daemon.
///
/// Both conditions must hold:
/// (a) the first whitespace-delimited token's file-name basename is exactly
///     `kkernel` (an absolute path like `/Users/x/.cargo/bin/kkernel` is
///     accepted; a basename of `not-kkernel` or a wrapper whose argv[0] merely
///     mentions kkernel elsewhere is rejected), AND
/// (b) the remaining tokens contain both `mcp` and `--daemon` as distinct
///     whitespace-separated tokens (matching the daemon spawn shape
///     `kkernel mcp --daemon`; a bare `kkernel exec '...'` has no `--daemon`
///     token and is correctly rejected).
fn argv_is_khive_daemon(args: &str) -> bool {
    let mut tokens = args.split_whitespace();
    let Some(exe_token) = tokens.next() else {
        return false;
    };
    // Compare by file-name basename so absolute paths are handled correctly.
    let basename = std::path::Path::new(exe_token)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if basename != "kkernel" && basename != "kkernel-bench" {
        return false;
    }
    let rest: Vec<&str> = tokens.collect();
    rest.contains(&"mcp") && rest.contains(&"--daemon")
}

enum PidIdentity {
    KhiveDaemon,
    Foreign,
    Indeterminate,
}

/// Classify the process named by the daemon PID file from its command line.
/// A live process whose identity cannot be read remains indeterminate so the
/// caller conservatively waits for its exit instead of treating it as stale.
fn classify_pid_identity(pid: u32) -> PidIdentity {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return PidIdentity::Foreign;
    };
    if pid_i32 <= 0 {
        return PidIdentity::Foreign;
    }
    #[cfg(test)]
    if FORCE_PID_IS_FOREIGN.load(std::sync::atomic::Ordering::SeqCst) {
        return PidIdentity::Foreign;
    }
    // Test seam: when FORCE_PID_IS_DAEMON is set, treat any positive live PID
    // as a daemon so the SIGTERM branch is reachable in tests.
    #[cfg(test)]
    if FORCE_PID_IS_DAEMON.load(std::sync::atomic::Ordering::SeqCst) {
        // SAFETY: signal 0 is an existence/permission probe with no side effects.
        return if unsafe { libc::kill(pid_i32, 0) } == 0 {
            PidIdentity::KhiveDaemon
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            PidIdentity::Indeterminate
        } else {
            PidIdentity::Foreign
        };
    }
    // Quick liveness check before shelling out.
    // SAFETY: signal 0 is an existence/permission probe with no side effects.
    if unsafe { libc::kill(pid_i32, 0) } != 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) => PidIdentity::Indeterminate,
            Some(libc::ESRCH) => PidIdentity::Foreign,
            _ => PidIdentity::Indeterminate,
        };
    }
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
    {
        Ok(out) if out.status.success() => {
            let args = String::from_utf8_lossy(&out.stdout);
            let args = args.trim();
            if args.is_empty() {
                PidIdentity::Indeterminate
            } else if argv_is_khive_daemon(args) {
                PidIdentity::KhiveDaemon
            } else {
                PidIdentity::Foreign
            }
        }
        _ => PidIdentity::Indeterminate,
    }
}

const INCUMBENT_EXIT_TIMEOUT_SECS: u64 = 12;
const INCUMBENT_EXIT_POLL_MS: u64 = 25;

#[derive(Debug)]
enum RecoveryError {
    Spawn(std::io::Error),
    IncumbentStillAlive { pid: u32 },
}

fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 is an existence/permission probe with no side effects.
    let result = unsafe { libc::kill(pid, 0) };
    let exists = result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !exists {
        return false;
    }
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    {
        Ok(output) if output.status.success() => !String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with('Z'),
        _ => true,
    }
}

async fn wait_for_process_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process_is_alive(pid) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(
            std::time::Duration::from_millis(INCUMBENT_EXIT_POLL_MS).min(deadline - now),
        )
        .await;
    }
}

/// Signal the daemon named by the PID file and remove its rendezvous files
/// only after its PID is positively confirmed dead (caller holds the recovery
/// lock). The PID is captured before SIGTERM and ownership is re-checked by
/// [`remove_daemon_paths_if_still_stale`] immediately before unlinking.
async fn kill_stale_daemon_inner(exit_timeout: std::time::Duration) -> Result<(), RecoveryError> {
    #[cfg(test)]
    KILL_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let pid_file = pid_path();
    let expected_pid = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());

    if let Some(pid) = expected_pid {
        let wait_for_exit = match classify_pid_identity(pid) {
            PidIdentity::KhiveDaemon => {
                if let Ok(signed) = i32::try_from(pid) {
                    if signed > 0 {
                        // SAFETY: SIGTERM is a standard termination signal with no
                        // side effects beyond asking the process to exit.
                        unsafe {
                            libc::kill(signed, libc::SIGTERM);
                        }
                    }
                }
                true
            }
            PidIdentity::Foreign => {
                tracing::warn!(
                    pid,
                    "PID in daemon file belongs to a foreign process — treating it as stale"
                );
                false
            }
            PidIdentity::Indeterminate => {
                tracing::warn!(
                    pid,
                    "could not read PID identity — skipping SIGTERM and waiting conservatively"
                );
                true
            }
        };
        if wait_for_exit && !wait_for_process_exit(pid, exit_timeout).await {
            return Err(RecoveryError::IncumbentStillAlive { pid });
        }
    }

    remove_daemon_paths_if_still_stale(&pid_file, expected_pid);
    Ok(())
}

/// Remove `pid_file`/the daemon socket only if ownership has not changed since
/// `expected_pid` was observed: the PID file must still name `expected_pid`,
/// and the socket path must not already have a live listener answering it.
/// Either signal changing means a replacement daemon claimed the rendezvous
/// between the observation and this call, and unlinking would delete its live
/// paths instead of the truly-stale ones (#645).
fn remove_daemon_paths_if_still_stale(pid_file: &std::path::Path, expected_pid: Option<u32>) {
    let current_pid = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    if current_pid != expected_pid {
        tracing::warn!(
            expected_pid = ?expected_pid,
            current_pid = ?current_pid,
            "pid file changed during stale-daemon cleanup — a replacement daemon \
             already claimed it; skipping unlink to avoid deleting its live paths"
        );
        return;
    }

    let sock = socket_path();
    // A plain blocking connect is enough here: any success means *something*
    // is now listening at this path, which can only be a replacement daemon
    // that bound after our probe found the old one dead. Combined with the
    // PID-file recheck above, this closes the window even when the recovery
    // lock alone did not exclude the replacement's boot.
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        tracing::warn!(
            socket = ?sock,
            "a live listener now answers the daemon socket — skipping unlink to \
             avoid deleting a replacement daemon's rendezvous"
        );
        return;
    }

    let _ = std::fs::remove_file(pid_file);
    let _ = std::fs::remove_file(&sock);
}

/// Outcome of the under-lock identity probe.
#[derive(Debug)]
enum ProbeOutcome {
    /// A live, identity-matching daemon responded before the deadline.
    Alive,
    /// Daemon is absent, crashed, or identity-mismatched — safe to kill+spawn.
    Dead,
    /// Probe timed out — daemon may be alive but slow; do NOT kill.
    Timeout,
    /// The boot/recovery lock ([`khive_runtime::daemon::lock_path`]) stayed
    /// contended past its bounded acquisition deadline while
    /// [`quiesce_then_probe_identity`] was trying to confirm no peer boot is
    /// in flight (#838). Distinct from `Timeout` (which
    /// means "the daemon itself answered slowly") — this means "could not
    /// even confirm whether a peer boot is still running" — so a caller must
    /// not conflate it with a healthy-but-slow daemon. NEVER-KILL on this
    /// outcome either: an unconfirmed peer boot is exactly the ambiguity
    /// `confirm_genuinely_dead` exists to resolve safely.
    LockContended,
}

/// Send a `probe_only` frame to the daemon and return whether a live,
/// identity-matching daemon responded within `timeout_ms` milliseconds.
///
/// Uses `DaemonRequestFrame::probe_only = true` so the daemon returns an
/// identity frame immediately after identity validation — without calling any
/// dispatcher verb, touching the DB, or executing any mutation.
///
/// Probe outcomes → kill decision:
///   `Alive`   → do NOT kill (identity-matching daemon is healthy)
///   `Dead`    → kill+spawn (definitively absent/crashed/mismatched)
///   `Timeout` → do NOT kill (daemon may be healthy-but-busy; NEVER-KILL-SLOW)
async fn probe_daemon_identity(config_id: &str, namespace: &str, timeout_ms: u64) -> ProbeOutcome {
    let probe = DaemonRequestFrame {
        ops: String::new(),
        presentation: None,
        presentation_per_op: None,
        namespace: namespace.to_string(),
        // A probe never reaches the identity-context / dispatch arm (it
        // short-circuits on `probe_only` right after the protocol/config_id
        // checks), so no per-request identity is meaningful here.
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: true,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let deadline = std::time::Duration::from_millis(timeout_ms);
    match tokio::time::timeout(deadline, try_forward_inner(&probe)).await {
        Err(_elapsed) => {
            tracing::debug!(
                timeout_ms,
                "under-lock probe timed out — daemon may be busy; skipping kill"
            );
            ProbeOutcome::Timeout
        }
        Ok(ForwardOutcome::Response(resp)) => {
            // Positive probe-ack confirmation: the response must carry the exact
            // probe-ack sentinel shape (ok=true, result=None, error=None) plus
            // pass all identity checks.
            //
            // Why the sentinel is necessary: `probe_only` is `#[serde(default)]`
            // and the schema has no deny_unknown_fields. A daemon built before this
            // field existed (same protocol version, older binary) deserialises the
            // probe frame without error and falls through to normal dispatch on the
            // empty `ops` string. That produces ok=false (parse error) with matching
            // identity fields — it would be misclassified as Alive by identity
            // checks alone, leaving a stale daemon in place. Requiring the probe-ack
            // shape makes the classifier fail-closed: only the explicit probe branch
            // in handle_conn produces ok=true + result=None + error=None.
            //
            // A normal successful dispatch always sets result=Some(_) (never None).
            // A normal error dispatch sets ok=false. So the sentinel is unambiguous.
            let is_probe_ack = resp.ok && resp.result.is_none() && resp.error.is_none();
            if is_probe_ack
                && !resp.version_mismatch
                && !resp.namespace_mismatch
                && !resp.config_mismatch
                && resp.daemon_protocol_version == PROTOCOL_VERSION
                && resp.served_config_id.as_deref() == Some(config_id)
            {
                tracing::debug!("under-lock probe: live matching daemon confirmed; skipping kill");
                ProbeOutcome::Alive
            } else {
                tracing::debug!(
                    is_probe_ack,
                    version_mismatch = resp.version_mismatch,
                    namespace_mismatch = resp.namespace_mismatch,
                    config_mismatch = resp.config_mismatch,
                    "under-lock probe: daemon did not return probe-ack or identity mismatch — will kill+spawn"
                );
                ProbeOutcome::Dead
            }
        }
        Ok(
            ForwardOutcome::NoSocket
            | ForwardOutcome::ParseFailure
            | ForwardOutcome::ProtocolMismatch,
        ) => ProbeOutcome::Dead,
        Ok(ForwardOutcome::Unreachable {
            kind,
            os_error_code,
        }) => {
            tracing::debug!(
                ?kind,
                ?os_error_code,
                "under-lock probe could not reach the daemon socket; treating its state as \
                 uncertain and suppressing lifecycle recovery"
            );
            ProbeOutcome::Timeout
        }
    }
}

/// Launch seam for daemon recovery.
///
/// Production closures return a real [`std::process::Child`]. The test harness
/// supplies an in-process handle around the real [`daemon::run_daemon`] server,
/// allowing parallel recovery to assert server convergence without forking the
/// test binary with synthetic CLI arguments (#539/#544).
trait DaemonLauncher: Sync {
    type Handle: std::fmt::Debug;

    fn launch(&self) -> std::io::Result<Self::Handle>;
}

struct ProcessDaemonLauncher<'a, F>(&'a F);

impl<F> DaemonLauncher for ProcessDaemonLauncher<'_, F>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    type Handle = std::process::Child;

    fn launch(&self) -> std::io::Result<Self::Handle> {
        (self.0)()
    }
}

/// Outcome returned by [`kill_and_respawn`] to the call site.
#[derive(Debug)]
enum RecoveryOutcome<H = std::process::Child> {
    /// A concurrent client already replaced the daemon; forward the real request
    /// via the normal path (no new spawn occurred).
    Skipped,
    /// This client killed the stale daemon and spawned a replacement; caller
    /// must wait for readiness then forward the real request. The production
    /// launcher returns a [`std::process::Child`] (#898), so
    /// `forward_or_spawn` can, once it is otherwise about to give up and fall
    /// back locally, positively confirm whether that specific respawn attempt
    /// already exited instead of ever binding the socket. The test launcher
    /// carries an in-process task handle with the same ownership role.
    Spawned(H),
    /// Could not obtain a positive confirmation either way within the
    /// deadline-bound recovery window (the recoverer lock or the boot/recovery
    /// lock stayed contended past its deadline) — #838. The
    /// caller's behavior is identical to `Skipped` (never kill on an
    /// unconfirmed state), but this is reported as a distinct variant rather
    /// than silently folded into `Skipped`, so logs/metrics do not conflate
    /// "positively confirmed alive" with "gave up without confirming".
    Uncertain,
}

/// Bounded number of quiescence-confirm rounds [`confirm_genuinely_dead`]
/// performs before trusting a `Dead` classification enough to kill+spawn.
const DEAD_CONFIRM_ROUNDS: u32 = 4;

/// Pacing between [`confirm_genuinely_dead`] rounds. Not a synchronization
/// mechanism by itself — the real synchronization is the bounded `flock` in
/// [`quiesce_then_probe_identity`]; this only avoids busy-spinning while
/// waiting for a peer that has not yet reached its own boot-guard call.
const DEAD_CONFIRM_POLL_MS: u64 = 75;

/// Deadline for each round's bounded wait on the boot/recovery lock inside
/// [`quiesce_then_probe_identity`]. #838: the previous
/// unbounded blocking `flock` meant `DEAD_CONFIRM_ROUNDS` bounded probe
/// *count*, not elapsed *time* — a wedged lock holder blocked recovery
/// forever. Bounding each round's lock wait makes the whole
/// `confirm_genuinely_dead` call bounded by
/// `DEAD_CONFIRM_ROUNDS * (BOOT_QUIESCENCE_LOCK_TIMEOUT_MS +
/// BOOT_FENCE_PROBE_TIMEOUT_MS + DEAD_CONFIRM_POLL_MS)` in the worst case.
const BOOT_QUIESCENCE_LOCK_TIMEOUT_MS: u64 = 500;

/// Block until no concurrent boot holds the shared boot/recovery lock (or the
/// bounded wait's deadline elapses), then re-probe daemon identity — reused
/// by [`confirm_genuinely_dead`] (#758). Deadline-bounded (unlike an
/// unbounded `flock`), so a wedged lock holder cannot block confirmation
/// rounds forever; a contended/failed acquisition returns the distinct
/// [`ProbeOutcome::LockContended`], not `Timeout` (#838). See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
async fn quiesce_then_probe_identity(
    config_id: &str,
    namespace: &str,
    timeout_ms: u64,
) -> ProbeOutcome {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(BOOT_QUIESCENCE_LOCK_TIMEOUT_MS);
    match tokio::task::spawn_blocking(move || {
        khive_runtime::daemon::try_acquire_daemon_boot_guard_until(deadline)
    })
    .await
    {
        Ok(Ok(Some(guard))) => drop(guard),
        Ok(Ok(None)) => {
            tracing::debug!(
                BOOT_QUIESCENCE_LOCK_TIMEOUT_MS,
                "boot/recovery lock still contended past its bounded wait; \
                 could not confirm quiescence this round"
            );
            return ProbeOutcome::LockContended;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to probe boot/recovery lock state");
            return ProbeOutcome::LockContended;
        }
        Err(_join_err) => return ProbeOutcome::LockContended,
    }
    probe_daemon_identity(config_id, namespace, timeout_ms).await
}

/// Confirm a `Dead` probe result is not racing a peer's in-flight
/// `kill_and_respawn` or the daemon's own cold boot (#758) — closes the
/// fork-to-flock gap where `spawn_daemon()`'s child exists but has not yet
/// reached its own `acquire_daemon_boot_guard()` call. Retries
/// [`quiesce_then_probe_identity`] up to [`DEAD_CONFIRM_ROUNDS`] times,
/// paced by [`DEAD_CONFIRM_POLL_MS`]; returns as soon as a peer's boot is
/// observed completing (`Alive`) or going slow (`Timeout`, NEVER-KILL-SLOW).
/// Only `Dead` once every round agrees. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
async fn confirm_genuinely_dead(config_id: &str, namespace: &str) -> ProbeOutcome {
    // `LockContended` means "still could not confirm this round" — the same
    // "keep polling" shape as `Dead`, not a terminal state like `Alive`/
    // `Timeout`. A peer's boot can legitimately hold the lock across several
    // rounds; only give up (return `LockContended`) once every round agreed
    // nobody could confirm, exactly mirroring how `Dead` only becomes trusted
    // once every round agreed the daemon was absent.
    //
    // #838: `LockContended` is STICKY across rounds — once
    // any round can't confirm quiescence, the aggregate must never collapse
    // back to `Dead` just because a LATER round happened to observe it. The
    // old code tracked only the last round's outcome, so a
    // LockContended-then-Dead sequence overwrote the earlier contention and
    // returned `Dead`, permitting kill+spawn on a call that never actually
    // established quiescence across every round. `Dead` is only trustworthy
    // when EVERY round agrees; a single contended round makes the whole call
    // `LockContended` regardless of what any other round returned.
    let mut saw_contention = false;
    for round in 0..DEAD_CONFIRM_ROUNDS {
        match quiesce_then_probe_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await {
            ProbeOutcome::Dead => {}
            ProbeOutcome::LockContended => saw_contention = true,
            other => return other,
        }
        if round + 1 < DEAD_CONFIRM_ROUNDS {
            tokio::time::sleep(std::time::Duration::from_millis(DEAD_CONFIRM_POLL_MS)).await;
        }
    }
    if saw_contention {
        ProbeOutcome::LockContended
    } else {
        ProbeOutcome::Dead
    }
}

/// Deadline for acquiring the recoverer-only lock before starting the
/// dead-confirmation → kill → confirmed-exit → spawn critical section.
/// This exceeds the incumbent-exit deadline so a peer can finish a full
/// recovery turn before another recoverer treats the lock as wedged. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
const RECOVERER_LOCK_TIMEOUT_MS: u64 = 16_000;

/// Kill the stale daemon and spawn a fresh one, serialized against concurrent
/// recoverers by a dedicated recoverer-only lock (#838 double-checked
/// recovery). Outcomes: `Alive`/`Timeout` → `Skipped`, no kill
/// (NEVER-KILL-SLOW). `LockContended` (confirm rounds inconclusive, or the
/// recoverer lock itself timed out) → `Uncertain`, no kill — same safe
/// behavior as `Skipped` but reported distinctly. `Dead` (confirmed,
/// recoverer lock held) → signal + bounded exit confirmation + spawn →
/// `Spawned`; a PID still alive at the deadline returns
/// [`RecoveryError::IncumbentStillAlive`] without spawning. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md` for why a second lock
/// file is required and how this avoids deadlocking a booting daemon.
async fn kill_and_respawn<F>(
    config_id: &str,
    namespace: &str,
    spawn: &F,
) -> Result<RecoveryOutcome, RecoveryError>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    let launcher = ProcessDaemonLauncher(spawn);
    kill_and_respawn_with_launcher(config_id, namespace, &launcher).await
}

/// Test-only shim over [`kill_and_respawn_with_launcher_and_exit_timeout`]:
/// same production code path, with only the launcher construction and the
/// exit-timeout parameter (production passes its own constant through
/// [`kill_and_respawn`]) supplied by the test. Keep it a pure forwarder —
/// any logic added here would drift from the non-test path.
#[cfg(test)]
async fn kill_and_respawn_with_exit_timeout<F>(
    config_id: &str,
    namespace: &str,
    spawn: &F,
    exit_timeout: std::time::Duration,
) -> Result<RecoveryOutcome, RecoveryError>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    let launcher = ProcessDaemonLauncher(spawn);
    kill_and_respawn_with_launcher_and_exit_timeout(config_id, namespace, &launcher, exit_timeout)
        .await
}

async fn kill_and_respawn_with_launcher<L>(
    config_id: &str,
    namespace: &str,
    launcher: &L,
) -> Result<RecoveryOutcome<L::Handle>, RecoveryError>
where
    L: DaemonLauncher,
{
    kill_and_respawn_with_launcher_and_exit_timeout(
        config_id,
        namespace,
        launcher,
        std::time::Duration::from_secs(INCUMBENT_EXIT_TIMEOUT_SECS),
    )
    .await
}

async fn kill_and_respawn_with_launcher_and_exit_timeout<L>(
    config_id: &str,
    namespace: &str,
    launcher: &L,
    exit_timeout: std::time::Duration,
) -> Result<RecoveryOutcome<L::Handle>, RecoveryError>
where
    L: DaemonLauncher,
{
    let initial_probe = {
        let _lock = acquire_recovery_lock();
        probe_daemon_identity(config_id, namespace, 500).await
    };
    match initial_probe {
        ProbeOutcome::Alive | ProbeOutcome::Timeout | ProbeOutcome::LockContended => {
            return Ok(RecoveryOutcome::Skipped);
        }
        ProbeOutcome::Dead => {}
    }

    // Test-only rendezvous (see `RECOVERY_RACE_BARRIER`): forces every
    // concurrent recoverer under test to reach "independently classified
    // Dead" at the same instant, so the recoverer lock below is what
    // actually determines mutual exclusion rather than scheduling order.
    #[cfg(test)]
    {
        let barrier = RECOVERY_RACE_BARRIER
            .lock()
            .expect("barrier mutex poisoned")
            .clone();
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
    }

    let recoverer_deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(RECOVERER_LOCK_TIMEOUT_MS);
    let recoverer_guard = match tokio::time::timeout(
        std::time::Duration::from_millis(RECOVERER_LOCK_TIMEOUT_MS),
        tokio::task::spawn_blocking(move || {
            khive_runtime::daemon::try_acquire_recoverer_lock_until(recoverer_deadline)
        }),
    )
    .await
    {
        Ok(Ok(Ok(Some(guard)))) => guard,
        Ok(Ok(Ok(None))) => {
            tracing::warn!(
                RECOVERER_LOCK_TIMEOUT_MS,
                "recoverer lock still contended past its deadline; a peer recoverer \
                 is likely still mid dead-confirmation/kill/spawn — skipping without \
                 a positive confirmation rather than risking a double-spawn"
            );
            return Ok(RecoveryOutcome::Uncertain);
        }
        Ok(Ok(Err(e))) => {
            tracing::warn!(error = %e, "failed to acquire recoverer lock");
            return Ok(RecoveryOutcome::Uncertain);
        }
        Ok(Err(_)) | Err(_) => {
            tracing::warn!("recoverer lock acquisition task failed or exceeded its deadline");
            return Ok(RecoveryOutcome::Uncertain);
        }
    };

    let outcome = match confirm_genuinely_dead(config_id, namespace).await {
        ProbeOutcome::Alive | ProbeOutcome::Timeout => Ok(RecoveryOutcome::Skipped),
        ProbeOutcome::LockContended => {
            tracing::warn!(
                "confirm_genuinely_dead could not establish quiescence within its \
                 bounded rounds; skipping kill+spawn without a positive confirmation"
            );
            Ok(RecoveryOutcome::Uncertain)
        }
        ProbeOutcome::Dead => {
            // Also take the shared boot/recovery lock for the kill+spawn step
            // itself, matching `acquire_recovery_lock`'s existing role of
            // serializing this against the daemon server's own
            // cleanup→bind→pid-write critical section. No deadlock risk: this
            // is a distinct lock file from `recoverer_guard` above, acquired
            // and dropped entirely within this arm.
            let _boot_lock = acquire_recovery_lock();
            kill_stale_daemon_inner(exit_timeout).await?;
            launcher
                .launch()
                .map(RecoveryOutcome::Spawned)
                .map_err(RecoveryError::Spawn)
        }
    };
    drop(recoverer_guard);
    outcome
}

/// Build the hard error returned when the real request frame was fully
/// written to the daemon socket but no trustworthy response came back.
///
/// #644: once `write_frame` has completed for a real (non-probe) frame, the
/// daemon may already be dispatching or have finished dispatching it. There is
/// no way from the client side to tell "never received" apart from "received,
/// executed, response lost" — so this case must never be retried (against the
/// same daemon or a freshly-spawned one) and must never silently fall back to
/// local dispatch, either of which could execute a mutation a second time.
fn ambiguous_forward_error() -> McpError {
    McpError::internal_error(
        "daemon response lost after request was sent; not retrying or locally \
         dispatching to avoid duplicate execution",
        None,
    )
}

fn daemon_unreachable_error(
    frame: &DaemonRequestFrame,
    kind: std::io::ErrorKind,
    os_error_code: Option<i32>,
) -> McpError {
    let config_id = opaque_config_id(&frame.config_id);
    tracing::error!(
        reason = "daemon_unreachable",
        config_id = %config_id,
        namespace = %frame.namespace,
        ?kind,
        ?os_error_code,
        "daemon socket is unreachable from this process; lifecycle recovery suppressed"
    );
    McpError::internal_error(
        "cannot reach daemon socket from this process; refusing daemon lifecycle recovery \
         because the daemon may still be healthy",
        Some(serde_json::json!({
            "reason": "daemon_unreachable",
            "os_error_kind": format!("{kind:?}"),
            "os_error_code": os_error_code,
        })),
    )
}

// ── #898: loud, unambiguous respawn-failure error ───────────────────────────
//
// Root cause (2026-07-12 incident): `spawn_daemon` already resolves the spawn
// target deterministically via `std::env::current_exe()` (never ambient
// `PATH`), so a version-skewed binary reaching `mcp --daemon` means THIS
// process's own on-disk binary predates (or otherwise rejects) that flag —
// respawning via `current_exe()` faithfully relaunches the very same stale
// binary. That relaunch fails immediately with a clap parse error
// (`error: Unrecognized option: 'daemon'`) written only to `khived.log`.
// Because `spawn_daemon` was fire-and-forget (the `Child` was discarded) and
// `forward_or_spawn` cannot distinguish "our own respawn attempt definitely
// already died" from "no daemon is configured to run here at all", every
// request repeated the same failing respawn, burned the full connect
// deadline plus the boot-quiescence wait, and then quietly completed via
// local dispatch (or, in `KHIVE_DAEMON_STRICT=1`, rejected with the generic
// `no_socket` reason) — a silent, forever-repeating failure invisible to the
// caller and to every metric except a `khived.log` grep.
//
// The fix: `spawn_daemon` now returns the live `Child` (see
// `RecoveryOutcome::Spawned`), and `forward_or_spawn` checks — only at the
// point it would otherwise fall back locally, never earlier, so the existing
// connect-retry window and #667's boot-quiescence fence are unchanged —
// whether the respawn attempt IT made has already exited. A confirmed exit is
// unambiguous (this process spawned that exact child); it is never treated as
// the legitimate ADR-049 no-daemon case and never silently swallowed, in
// either strict or non-strict mode.

#[derive(Clone, Copy)]
enum RespawnFailure {
    SpawnError { os_error_code: Option<i32> },
    ExitedBeforeBind { exit_code: Option<i32> },
}

/// Build the caller-visible error for a respawn attempt this process made and
/// can now positively confirm failed. Both the caller error and bridge tracing
/// expose only stable classifications and non-sensitive numeric status codes.
/// The error is returned regardless of
/// `KHIVE_DAEMON_STRICT`: unlike the ordinary "no daemon reachable" fallback
/// (which may be the legitimate ADR-049 no-daemon deployment), a respawn WE
/// attempted and can prove failed is never a case for quietly completing the
/// request via local dispatch. See #898.
fn respawn_failed_error(failure: RespawnFailure) -> McpError {
    match failure {
        RespawnFailure::SpawnError { os_error_code } => tracing::error!(
            reason = "respawn_failed",
            failure_category = "spawn_error",
            ?os_error_code,
            "daemon respawn attempt confirmed failed"
        ),
        RespawnFailure::ExitedBeforeBind { exit_code } => tracing::error!(
            reason = "respawn_failed",
            failure_category = "exited_before_bind",
            ?exit_code,
            "daemon respawn attempt confirmed failed"
        ),
    }
    // Under strict mode this is also a pre-dispatch rejection, so preserve
    // #947's request-envelope contract by tagging it for `server::request`.
    // Non-strict callers still receive the raw, loud MCP error introduced by
    // #898; in neither mode may the request fall through to local dispatch.
    let data = if is_daemon_strict_mode() {
        serde_json::json!({
            STRICT_FALLBACK_MARKER: true,
            "reason": "respawn_failed",
        })
    } else {
        serde_json::json!({"reason": "respawn_failed"})
    };
    McpError::internal_error(
        "daemon respawn failed (respawn_failed); rebuild with `make local` and retry",
        Some(data),
    )
}

fn incumbent_still_alive_error(pid: u32) -> McpError {
    tracing::error!(
        reason = "incumbent_still_alive",
        pid,
        "daemon recovery refused because the incumbent did not exit before the deadline"
    );
    let mut data = serde_json::json!({
        "reason": "incumbent_still_alive",
        "pid": pid,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    McpError::internal_error(
        format!("daemon recovery refused: incumbent PID {pid} is still alive after the deadline"),
        Some(data),
    )
}

// ── bridge self-heal: re-exec in place on ProtocolMismatch (#714) ───────────
//
// A long-lived stdio bridge process keeps running the OLD on-disk binary
// after `make local` rebuilds it: the daemon it spawns/forwards to is fresh,
// but this bridge process itself never picks up the new binary until its MCP
// client reconnects. `ProtocolMismatch` is exactly that scenario — the daemon
// is fine, this bridge is stale — so instead of leaving the connection dead
// forever, the bridge re-execs the freshest on-disk binary in place,
// preserving the PID and the open stdio file descriptors so the client's
// transport never sees EOF or a reset. See issue #714 for the evidence this
// design is based on (a live re-exec-mid-session test against the reference
// MCP Python SDK client): a first attempt that called `execv()` synchronously
// inside the tool handler, before the SDK's send loop had serialized and
// flushed the response, discarded the in-flight response and the client's
// call timed out with nothing ever written back.
//
// The fix is a true happens-after edge, not a fixed delay: [`arm_pending_self_heal`]
// records the chosen action *before* the mismatch response is even
// constructed (`trigger_bridge_self_heal` runs synchronously inside the
// request handler), and [`SelfHealOnFlushTransport`] — wrapped around the
// stdio transport in `server.rs::serve_stdio` — fires that action from
// [`fire_pending_self_heal`] only once a message has actually finished
// flushing to the client. Because arming always happens before the mismatch
// response is handed to the transport, and firing only ever happens after a
// flush completes, the very next successful flush is guaranteed to be at or
// after that response reached the client — never before, and never on a
// clock that can expire while a slow or backpressured stdout is still
// mid-write (a fixed-duration sleep could not make that guarantee: rmcp's
// send pipeline enqueues the response and returns almost immediately, then
// performs the real write+flush on a separately spawned task with no
// duration bound).

/// Pending self-heal action, armed by [`arm_pending_self_heal`] and taken by
/// [`fire_pending_self_heal`]. `None` on a healthy bridge for its entire
/// lifetime — the overwhelmingly common case.
static PENDING_SELF_HEAL: std::sync::Mutex<Option<MismatchRecovery>> = std::sync::Mutex::new(None);

fn arm_pending_self_heal(action: MismatchRecovery) {
    let mut slot = PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some(action);
}

/// Take and perform whatever self-heal action is armed, if any. Called by
/// [`SelfHealOnFlushTransport::send`] after every message it successfully
/// flushes to the client — the load-bearing happens-after edge documented
/// above. A no-op when nothing is armed.
#[cfg(unix)]
pub(crate) fn fire_pending_self_heal() {
    let action = PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    match action {
        Some(MismatchRecovery::ReexecScheduled) => reexec_in_place(),
        Some(MismatchRecovery::DrainAndExit) => exit_process(),
        None => {}
    }
}

/// Re-exec self-heal requires `exec()` (POSIX-only) — [`schedule_reexec_on_mismatch`]'s
/// non-unix variant arms [`MismatchRecovery::DrainAndExit`] instead of
/// [`MismatchRecovery::ReexecScheduled`], so only the drain-and-exit arm is
/// ever actually armed on this target.
#[cfg(not(unix))]
pub(crate) fn fire_pending_self_heal() {
    let action = PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if action.is_some() {
        exit_process();
    }
}

/// argv marker carrying the exec-once loop-breaker generation counter.
///
/// Parsed here and defined on [`crate::args::Args`] as a hidden clap field of
/// the same name — the clap field exists purely so the CLI parser accepts the
/// flag on a resumed process instead of rejecting it as unknown; the actual
/// read path is this raw argv scan, independent of wherever in the call stack
/// `Args` was parsed. The counter travels with the exec by construction: it
/// is appended to argv immediately before `exec()`, so any process running
/// with it present is, by definition, a resumed generation.
const RESUMED_GENERATION_ARG_PREFIX: &str = "--resumed-generation=";

/// Whether this process is a resumed generation of a prior self-heal re-exec,
/// and if so, its generation counter. `None` on a normal (cold-started)
/// bridge — the overwhelmingly common case.
pub(crate) fn resumed_generation() -> Option<u32> {
    resumed_generation_from_args(std::env::args())
}

/// Pure argv-scan behind [`resumed_generation`], factored out so the parsing
/// logic is unit-testable without depending on this process's own real argv
/// (which never carries the marker inside `cargo test`).
fn resumed_generation_from_args(args: impl Iterator<Item = String>) -> Option<u32> {
    args.filter_map(|a| {
        a.strip_prefix(RESUMED_GENERATION_ARG_PREFIX)
            .map(str::to_owned)
    })
    .last()
    .and_then(|s| s.parse::<u32>().ok())
}

/// Recovery action chosen for a `ProtocolMismatch` observed inside
/// `forward_or_spawn`. Pure decision, factored out of [`trigger_bridge_self_heal`]
/// so the loop-breaker guard rail (#714 §2.2: exec at most once per mismatch
/// generation) is unit-testable without touching the process or the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MismatchRecovery {
    /// First generation (no `--resumed-generation` marker) — schedule an
    /// in-place re-exec of the freshest on-disk binary.
    ReexecScheduled,
    /// Already a resumed generation and it hit `ProtocolMismatch` again — the
    /// on-disk binary is itself stale, or a second rebuild race — take the
    /// fallback instead of exec'ing a second time.
    DrainAndExit,
}

fn decide_mismatch_recovery(resumed_generation: Option<u32>) -> MismatchRecovery {
    match resumed_generation {
        None => MismatchRecovery::ReexecScheduled,
        Some(_) => MismatchRecovery::DrainAndExit,
    }
}

/// Trigger the bridge's self-heal recovery for a `ProtocolMismatch` outcome.
/// Called from both `forward_or_spawn` `ProtocolMismatch` arms; both already
/// construct and return the hard mismatch error, this schedules recovery
/// alongside that return, never in place of it. Accepted concurrency risk
/// (#714, not fixed by this change): a genuinely concurrent second in-flight
/// request could observe the wrong flush event. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
fn trigger_bridge_self_heal() {
    match decide_mismatch_recovery(resumed_generation()) {
        MismatchRecovery::ReexecScheduled => schedule_reexec_on_mismatch(),
        MismatchRecovery::DrainAndExit => {
            tracing::warn!(
                "resumed generation observed ProtocolMismatch again — loop-breaker \
                 tripped (#714 §2.2, exec-once guard); draining and exiting instead \
                 of re-exec'ing a second time"
            );
            schedule_drain_and_exit();
        }
    }
}

/// Arm an in-place re-exec of the freshest on-disk binary, to fire from
/// [`fire_pending_self_heal`] once the mismatch response has actually
/// flushed (see the module-level doc above for why that happens-after edge
/// — not a fixed delay — is load-bearing). Never execs synchronously and
/// never execs from this function directly. Only ever called for a
/// first-generation process — the loop-breaker guard rail lives in
/// [`trigger_bridge_self_heal`].
#[cfg(unix)]
pub(crate) fn schedule_reexec_on_mismatch() {
    tracing::warn!(
        client_version = PROTOCOL_VERSION,
        "protocol mismatch: arming in-place re-exec of the freshest on-disk binary, \
         to fire once the mismatch response has flushed to the client"
    );
    arm_pending_self_heal(MismatchRecovery::ReexecScheduled);
}

/// Re-exec self-heal requires `exec()` (POSIX-only); on any other target,
/// take the same drain-and-exit fallback a loop-breaker trip would.
#[cfg(not(unix))]
pub(crate) fn schedule_reexec_on_mismatch() {
    schedule_drain_and_exit();
}

/// Perform the actual re-exec: resolve the on-disk binary at *exec time* via
/// [`std::env::current_exe`] (the same primitive `spawn_daemon` already uses
/// for "pick up whatever `make local` just replaced" — see `spawn_daemon`
/// above), preserve the original argv, append the `--resumed-generation=1`
/// marker, and replace the process image via
/// [`std::os::unix::process::CommandExt::exec`] — which, unlike `spawn`,
/// keeps the same PID and the same open stdin/stdout/stderr file descriptors,
/// so the client's stdio transport never sees EOF or a reset.
///
/// `exec` only returns on failure; on failure this logs and returns, leaving
/// the process running under its stale binary (the hard mismatch error was
/// already sent to the client for this request; there is nothing safe to
/// retry from here).
#[cfg(all(unix, not(test)))]
fn reexec_in_place() {
    use std::os::unix::process::CommandExt;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "bridge self-heal re-exec failed: could not resolve current_exe");
            return;
        }
    };
    // Drop any pre-existing marker defensively (should never be present here —
    // `trigger_bridge_self_heal` only reaches this path for a first-generation
    // process — but argv should never accumulate duplicates if that invariant
    // is ever violated).
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with(RESUMED_GENERATION_ARG_PREFIX))
        .chain(std::iter::once(format!("{RESUMED_GENERATION_ARG_PREFIX}1")))
        .collect();
    let err = std::process::Command::new(exe).args(&args).exec();
    tracing::error!(
        error = %err,
        "bridge self-heal re-exec failed; continuing under the stale binary"
    );
}

/// Arm this process to stop serving and exit, to fire from
/// [`fire_pending_self_heal`] for the same happens-after reason
/// [`schedule_reexec_on_mismatch`] arms its exec instead of performing it
/// directly. This is the fallback path (issue #714 §4): the MCP connection
/// dies and the client's own process-lifecycle management must restart it —
/// no worse than the pre-#714 hard-error-forever behavior.
pub(crate) fn schedule_drain_and_exit() {
    arm_pending_self_heal(MismatchRecovery::DrainAndExit);
}

#[cfg(not(test))]
fn exit_process() {
    std::process::exit(1);
}

#[cfg(all(test, unix))]
pub(crate) static REEXEC_INVOKED_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static DRAIN_EXIT_INVOKED_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(test, unix))]
pub(crate) fn reset_self_heal_counters() {
    REEXEC_INVOKED_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    DRAIN_EXIT_INVOKED_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    clear_pending_self_heal();
}

#[cfg(all(test, not(unix)))]
pub(crate) fn reset_self_heal_counters() {
    DRAIN_EXIT_INVOKED_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    clear_pending_self_heal();
}

/// Clear whatever `PENDING_SELF_HEAL` currently holds (#843). A test that
/// exercises `forward_or_spawn`'s protocol-mismatch path end to end (e.g.
/// `forward_or_spawn_rejects_old_daemon_and_returns_protocol_mismatch_error`)
/// arms it via `trigger_bridge_self_heal` but never takes the action back
/// out — that is `fire_pending_self_heal`'s job, and asserting the forwarding
/// behavior alone has no reason to call it. Left armed, that leftover slot
/// is invisible to `reset_self_heal_counters` resetting only the two
/// invocation counters, so a later test in the same binary (e.g.
/// `fire_pending_self_heal_is_a_no_op_when_nothing_is_armed`) can inherit it
/// under multi-threaded test ordering and observe a spurious fire. Every
/// existing test that intentionally arms the slot does so AFTER calling
/// `reset_self_heal_counters`, never before, so clearing it here changes no
/// test's semantics.
#[cfg(test)]
fn clear_pending_self_heal() {
    *PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Test double for [`reexec_in_place`]: a real `exec()` would replace the test
/// binary's own process image, killing the entire test run. Counts instead.
/// Gated `unix` like the production version above — it is the only thing that
/// calls it (`schedule_reexec_on_mismatch`'s `not(unix)` arm never reaches
/// `reexec_in_place` at all).
#[cfg(all(test, unix))]
fn reexec_in_place() {
    REEXEC_INVOKED_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Wraps a [`rmcp::transport::Transport`] so every message it successfully
/// flushes to the client fires [`fire_pending_self_heal`] afterward — the
/// actual happens-after edge #714's self-heal design requires, not a
/// fixed-duration sleep. Wraps the transport (not the handler) because the
/// handler itself has no way to await the real write+flush, which `rmcp`
/// performs on a separately spawned task. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
pub(crate) struct SelfHealOnFlushTransport<T> {
    inner: T,
}

impl<T> SelfHealOnFlushTransport<T> {
    pub(crate) fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T> rmcp::transport::Transport<rmcp::RoleServer> for SelfHealOnFlushTransport<T>
where
    T: rmcp::transport::Transport<rmcp::RoleServer>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let send = self.inner.send(item);
        async move {
            let result = send.await;
            if result.is_ok() {
                fire_pending_self_heal();
            }
            result
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>>
           + Send {
        self.inner.receive()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

#[cfg(test)]
fn exit_process() {
    DRAIN_EXIT_INVOKED_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Bounded probe timeout used by [`wait_for_boot_quiescence_then_reprobe`],
/// matching the 500ms bound already used by the identity probe inside
/// [`kill_and_respawn`].
const BOOT_FENCE_PROBE_TIMEOUT_MS: u64 = 500;

/// Outcome of waiting for a concurrent cold-boot (migrations + pack schema
/// plans / FTS DDL) to finish, then re-checking daemon liveness once quiescent.
enum BootFenceOutcome {
    /// Boot quiesced and a live, identity-matching daemon answered — keep
    /// sending the real frame to it.
    DaemonReady,
    /// Boot quiesced and the probe definitively found no daemon — safe to
    /// fall back to local dispatch.
    SafeLocalFallback,
    /// Daemon state is unknown (lock/join failure, or the post-quiescence
    /// probe itself timed out) — must not local-dispatch.
    HardError(McpError),
}

/// #667: the readiness-timeout branch of `forward_or_spawn`'s send loop used
/// to return `None` (silent local fallback) purely because a freshly spawned
/// daemon had not answered within the fixed deadline — including while that
/// daemon was still inside its boot guard running migrations/pack schema
/// plans (FTS DDL). A local writer/searcher racing in at exactly that moment
/// could observe or create a partially-initialized `notes`/`fts_notes` schema.
///
/// This blocks on the SAME boot guard the daemon holds across cold-boot
/// schema init (ADR-D3): acquiring it here can only succeed once no boot is
/// in progress, so acquiring-then-immediately-dropping it is a pure
/// quiescence wait. Only after that wait does it re-probe daemon identity —
/// distinguishing "was still booting, now ready" from "genuinely no daemon"
/// so the caller never has to guess which one caused the original timeout.
async fn wait_for_boot_quiescence_then_reprobe(frame: &DaemonRequestFrame) -> BootFenceOutcome {
    // `acquire_daemon_boot_guard` performs a blocking `flock`; run it on the
    // blocking pool rather than the async executor.
    let quiesced =
        tokio::task::spawn_blocking(khive_runtime::daemon::acquire_daemon_boot_guard).await;
    match quiesced {
        Ok(Ok(guard)) => {
            // The guard's only purpose here is proving quiescence; drop it
            // immediately so it does not itself block a real boot or the
            // re-probe below.
            drop(guard);
        }
        Ok(Err(e)) => {
            return BootFenceOutcome::HardError(McpError::internal_error(
                format!(
                    "failed to acquire daemon boot/recovery lock while waiting for \
                     cold-boot quiescence: {e}"
                ),
                None,
            ));
        }
        Err(e) => {
            return BootFenceOutcome::HardError(McpError::internal_error(
                format!("boot-quiescence wait task failed: {e}"),
                None,
            ));
        }
    }

    match probe_daemon_identity(
        &frame.config_id,
        &frame.namespace,
        BOOT_FENCE_PROBE_TIMEOUT_MS,
    )
    .await
    {
        ProbeOutcome::Alive => BootFenceOutcome::DaemonReady,
        ProbeOutcome::Dead => BootFenceOutcome::SafeLocalFallback,
        ProbeOutcome::Timeout => BootFenceOutcome::HardError(McpError::internal_error(
            "daemon state uncertain after cold-boot quiescence; not falling back to \
             local dispatch to avoid racing a possibly still-initializing index",
            None,
        )),
        // `probe_daemon_identity` (unlike `quiesce_then_probe_identity`) never
        // constructs `LockContended` — it has no lock-acquisition step of its
        // own. Handled here only for match exhaustiveness over the shared
        // `ProbeOutcome` type; same fail-safe HardError as `Timeout` if it
        // were ever reached.
        ProbeOutcome::LockContended => BootFenceOutcome::HardError(McpError::internal_error(
            "daemon state uncertain after cold-boot quiescence (lock probe unexpectedly \
             contended); not falling back to local dispatch",
            None,
        )),
    }
}

/// Forward a request to the daemon, auto-spawning it if absent.
///
/// Returns `None` only when nothing was ever written to the daemon and local
/// dispatch is therefore safe: `KHIVE_NO_DAEMON` is set, or the socket is
/// definitively absent/refused (`NoSocket`) — never when connection access is
/// denied (`Unreachable`) and never after the real frame has been written.
/// `Some(Ok)` / `Some(Err)` both mean the caller must not dispatch locally.
/// Under `KHIVE_DAEMON_STRICT=1` the `NoSocket` case instead becomes
/// `Some(Err(..))` (`KHIVE_NO_DAEMON` is unaffected — it is an explicit caller
/// opt-out, not a fallback).
///
/// The real (possibly mutating) request frame is written to the daemon
/// socket at most once per call; a `NoSocket` outcome never writes anything,
/// so it is safe to recover the daemon and retry. Once the real frame IS
/// fully written (`ParseFailure`/`ProtocolMismatch`), this returns a hard
/// error immediately instead of killing/respawning/retrying or falling back
/// locally (#644). See `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
pub async fn forward_or_spawn(frame: &DaemonRequestFrame) -> Option<Result<String, McpError>> {
    forward_or_spawn_with(frame, &spawn_daemon).await
}

/// Forward a request while preserving an explicit config selection (and an
/// ephemeral `:memory:` database override) on a daemon this call may need to
/// spawn.
///
/// An already-running daemon is still matched exclusively by `config_id`.
/// `config` and `db` only supply construction inputs for a missing daemon;
/// without them, `kkernel exec --config <path>` would spawn `kkernel mcp
/// --daemon` against automatic discovery and immediately disagree with the
/// request it was spawned to serve, and `kkernel exec --db :memory:` would
/// bind the fresh daemon to the config's declared persistent files.
///
/// `packs`, when given, is the caller's already-resolved pack list (whatever
/// mix of CLI `--pack`, `KHIVE_PACKS`, or config-file `[runtime].packs`
/// produced it) forwarded verbatim as explicit `--pack` args on the spawned
/// daemon — see the doc comment on `spawn_daemon_with_exe_and_config`.
pub async fn forward_or_spawn_with_config_and_packs(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
) -> Option<Result<String, McpError>> {
    #[cfg(any(test, feature = "test-forward-seam"))]
    if let Some(intercepted) = test_forward_seam::intercept(frame, packs) {
        return intercepted;
    }
    let spawn = || {
        let exe = std::env::current_exe()?;
        spawn_daemon_with_exe_and_config(&exe, config, db, packs)
    };
    forward_or_spawn_with(frame, &spawn).await
}

/// Forward a request, spawning the daemon if needed, without an explicit
/// pack list: the spawned daemon falls back to its own pack resolution
/// (config-file `[runtime].packs` or the built-in default set), exactly as
/// it did before pack forwarding existed.
///
/// Thin compatibility wrapper over
/// [`forward_or_spawn_with_config_and_packs`] preserving the pre-existing
/// three-argument public signature, so external callers of the published
/// crate keep compiling unchanged.
pub async fn forward_or_spawn_with_config(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
) -> Option<Result<String, McpError>> {
    forward_or_spawn_with_config_and_packs(frame, config, db, None).await
}

/// Test-only capture hook armed at the real entry of
/// `forward_or_spawn_with_config_and_packs` — the actual adapter-boundary conversion
/// site (`packs.as_deref()` at each production call site) production code
/// runs through, as opposed to a `ForwardFnPtr` spy that stands in for the
/// whole adapter and never executes its conversion. Unarmed, `intercept`
/// costs one thread-local read and returns `None` immediately: the
/// production path is byte-for-byte unaffected.
///
/// Gated by `cfg(any(test, feature = "test-forward-seam"))` so both the
/// in-crate `khive-mcp` test suite (plain `cfg(test)`) and a cross-crate
/// `kkernel` test (which cannot see `khive-mcp`'s `cfg(test)` items, so it
/// enables the `test-forward-seam` feature via a `[dev-dependencies]`
/// re-declaration instead) can reach it.
#[cfg(any(test, feature = "test-forward-seam"))]
pub mod test_forward_seam {
    use super::{DaemonRequestFrame, McpError};
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static CAPTURED: std::cell::RefCell<Option<Option<Vec<String>>>> =
            const { std::cell::RefCell::new(None) };
        static CAPTURED_REQUEST_ID: std::cell::RefCell<Option<Option<u64>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Arm the one-shot hook. The next call into
    /// `forward_or_spawn_with_config_and_packs` on this thread records its `packs`
    /// argument and returns a canned success without touching a socket or
    /// spawning a process; the hook then disarms itself.
    pub fn arm() {
        CAPTURED.with(|c| *c.borrow_mut() = None);
        CAPTURED_REQUEST_ID.with(|c| *c.borrow_mut() = None);
        ARMED.with(|a| a.set(true));
    }

    /// Take the captured `packs` argument from the most recent armed call,
    /// if any, and disarm the hook. Disarming here means a hook armed for a
    /// call that never reached `intercept` cannot survive to intercept a
    /// later, unrelated call on the same thread.
    pub fn take_captured() -> Option<Option<Vec<String>>> {
        ARMED.with(|a| a.set(false));
        CAPTURED.with(|c| c.borrow_mut().take())
    }

    /// Take the captured `frame.request_id` of the most recent armed call,
    /// if any. Companion to `take_captured` for tests proving the bridge
    /// correlation id (khive#948, khive-oss#2337) reaches the real
    /// `forward_or_spawn_with_config_and_packs` adapter boundary unchanged.
    pub fn take_captured_request_id() -> Option<Option<u64>> {
        CAPTURED_REQUEST_ID.with(|c| c.borrow_mut().take())
    }

    pub(super) fn intercept(
        frame: &DaemonRequestFrame,
        packs: Option<&[String]>,
    ) -> Option<Option<Result<String, McpError>>> {
        let was_armed = ARMED.with(|a| a.replace(false));
        if !was_armed {
            return None;
        }
        CAPTURED.with(|c| *c.borrow_mut() = Some(packs.map(<[String]>::to_vec)));
        CAPTURED_REQUEST_ID.with(|c| *c.borrow_mut() = Some(frame.request_id));
        Some(Some(Ok(serde_json::json!({
            "results": [{"ok": true, "tool": "stats", "result": {}}],
            "summary": {"total": 1, "succeeded": 1, "failed": 0},
        })
        .to_string())))
    }
}

#[cfg(test)]
async fn forward_or_spawn_with_exe(
    frame: &DaemonRequestFrame,
    exe: &std::path::Path,
) -> Option<Result<String, McpError>> {
    let spawn = || spawn_daemon_with_exe(exe);
    forward_or_spawn_with(frame, &spawn).await
}

async fn forward_or_spawn_with<F>(
    frame: &DaemonRequestFrame,
    spawn: &F,
) -> Option<Result<String, McpError>>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    if env_truthy("KHIVE_NO_DAEMON") {
        return None;
    }

    match try_forward_inner(frame).await {
        ForwardOutcome::Response(resp) => {
            return map_response(*resp, &frame.config_id, &frame.namespace)
        }
        ForwardOutcome::NoSocket => {
            // Nothing was written; fall through to the spawn/recover-then-send
            // path below.
        }
        ForwardOutcome::Unreachable {
            kind,
            os_error_code,
        } => return Some(Err(daemon_unreachable_error(frame, kind, os_error_code))),
        ForwardOutcome::ParseFailure => {
            let config_id = opaque_config_id(&frame.config_id);
            tracing::warn!(
                config_id = %config_id,
                namespace = %frame.namespace,
                retry_suppressed = true,
                "daemon connection lost after the request was fully written — \
                 not retrying or falling back locally to avoid duplicate dispatch"
            );
            return Some(Err(ambiguous_forward_error()));
        }
        ForwardOutcome::ProtocolMismatch => {
            let config_id = opaque_config_id(&frame.config_id);
            tracing::warn!(
                config_id = %config_id,
                namespace = %frame.namespace,
                retry_suppressed = true,
                "daemon protocol mismatch discovered after the request was fully \
                 written — not retrying or falling back locally to avoid duplicate dispatch"
            );
            // #714: the daemon is fine (it just rejected us) — this bridge
            // process itself is the stale one. Trigger self-heal alongside
            // the hard error below, never in place of it.
            trigger_bridge_self_heal();
            return Some(Err(McpError::internal_error(
                format!(
                    "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                     run `make local` to rebuild the daemon binary"
                ),
                None,
            )));
        }
    }

    // NoSocket: nothing has been written yet. Establish a live daemon — either
    // this is the first-ever spawn or a stale one needs replacing — under the
    // single recovery lock, using only `probe_only` frames for the identity
    // check. `kill_and_respawn` is a no-op kill when there is nothing stale to
    // remove, so first-spawn and recovery share this one path.
    //
    // #898: `spawned_child` holds the live handle from `RecoveryOutcome::Spawned`
    // (if THIS call actually spawned one) purely so the two "about to give up
    // and fall back locally" points below can check whether that specific
    // attempt already exited — never polled eagerly, never used to cut the
    // connect-retry window or the #667 boot-quiescence wait short.
    let mut spawned_child: Option<std::process::Child> = None;
    match kill_and_respawn(&frame.config_id, &frame.namespace, spawn).await {
        Err(RecoveryError::Spawn(e)) => {
            // #898: `Command::spawn` itself failed to start the child at all —
            // an unambiguous, already-fully-diagnosed respawn failure. Loud in
            // both strict and non-strict mode; never a silent local fallback.
            return Some(Err(respawn_failed_error(RespawnFailure::SpawnError {
                os_error_code: e.raw_os_error(),
            })));
        }
        Err(RecoveryError::IncumbentStillAlive { pid }) => {
            return Some(Err(incumbent_still_alive_error(pid)));
        }
        Ok(RecoveryOutcome::Skipped) => {
            // A concurrent client already has a live matching daemon ready.
        }
        Ok(RecoveryOutcome::Spawned(child)) => {
            // Give the kernel a moment to release the socket path and let the
            // spawned daemon process start.
            spawned_child = Some(child);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(RecoveryOutcome::Uncertain) => {
            // Could not positively confirm the daemon's state within the
            // deadline (#838) — behave like `Skipped` (never
            // kill on an unconfirmed state) and let the forward loop below
            // discover the real state: it will either reach a daemon a peer
            // is spawning, or hit the readiness deadline and fall through to
            // `wait_for_boot_quiescence_then_reprobe`.
            tracing::debug!(
                "daemon recovery state uncertain; forwarding without a fresh kill+spawn"
            );
        }
    }

    // Send the real frame exactly once now that a daemon is confirmed ready
    // (or believed ready via Skipped). The connect attempt inside
    // `try_forward_inner` doubles as the readiness check — a `NoSocket`
    // outcome here just means "not listening yet" (nothing written), so keep
    // retrying; any other outcome is terminal and returned immediately.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if tokio::time::Instant::now() >= deadline {
            // #667: a bare timeout here does not mean "no daemon" — it may
            // mean "daemon is still inside cold-boot schema init". Wait for
            // that boot (if any) to quiesce and re-probe before deciding.
            match wait_for_boot_quiescence_then_reprobe(frame).await {
                BootFenceOutcome::DaemonReady => {
                    // The real frame still has not been written; fall through
                    // and send it now that boot has quiesced.
                }
                BootFenceOutcome::SafeLocalFallback => {
                    // #898: only now — after the full connect-retry window AND
                    // the #667 boot-quiescence wait, exactly as before — check
                    // whether the respawn THIS call made has already exited.
                    // A confirmed exit is unambiguous (this process spawned
                    // that exact child) and is never treated as the
                    // legitimate ADR-049 no-daemon case.
                    if let Some(child) = spawned_child.as_mut() {
                        if let Ok(Some(status)) = child.try_wait() {
                            return Some(Err(respawn_failed_error(
                                RespawnFailure::ExitedBeforeBind {
                                    exit_code: status.code(),
                                },
                            )));
                        }
                    }
                    return fallback_or_reject(
                        FallbackReason::NoSocket,
                        &frame.config_id,
                        None,
                        &frame.namespace,
                    );
                }
                BootFenceOutcome::HardError(err) => return Some(Err(err)),
            }
        }
        match try_forward_inner(frame).await {
            ForwardOutcome::Response(resp) => {
                return map_response(*resp, &frame.config_id, &frame.namespace)
            }
            ForwardOutcome::ParseFailure => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    retry_suppressed = true,
                    "freshly-established daemon connection lost after the request \
                     was fully written — not retrying or falling back locally"
                );
                return Some(Err(ambiguous_forward_error()));
            }
            ForwardOutcome::ProtocolMismatch => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    "daemon protocol mismatch discovered on the post-recovery retry \
                     — not retrying again or falling back locally"
                );
                // #714: same self-heal trigger as the first-attempt arm above —
                // either arm can observe the mismatch depending on whether this
                // was the first probe or a retry after a kill/respawn.
                trigger_bridge_self_heal();
                return Some(Err(McpError::internal_error(
                    format!(
                        "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                         run `make local` to rebuild the daemon binary"
                    ),
                    None,
                )));
            }
            ForwardOutcome::Unreachable {
                kind,
                os_error_code,
            } => return Some(Err(daemon_unreachable_error(frame, kind, os_error_code))),
            ForwardOutcome::NoSocket => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::test_harness::{
        clear_daemon_env, connect_when_ready, exchange, HarnessDispatch, InProcessDaemonHandle,
        InProcessDaemonLauncher, RecoveryTestGuard,
    };
    use super::*;
    use khive_runtime::daemon::run_daemon;
    use serial_test::serial;

    use khive_runtime::engine_config::ActorConfig;
    use khive_runtime::{
        runtime_config_from_khive_config, GitWriteEntryConfig, GitWriteSectionConfig, KhiveConfig,
        KhiveRuntime, Namespace, RuntimeConfig,
    };

    fn memory_runtime_config() -> RuntimeConfig {
        KhiveRuntime::memory()
            .expect("memory runtime")
            .config()
            .clone()
    }

    fn make_test_server() -> crate::server::KhiveMcpServer {
        let mut config = memory_runtime_config();
        config.default_namespace = Namespace::parse("test").unwrap();
        config.packs = vec!["kg".to_string(), "gtd".to_string()];
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+gtd")
    }

    /// Server whose pack set includes `brain`, which registers
    /// `Visibility::Subhandler` verbs (`brain.state`, …). Used to exercise the
    /// wire visibility gate through the daemon round-trip.
    fn make_subhandler_test_server() -> crate::server::KhiveMcpServer {
        let mut config = memory_runtime_config();
        config.default_namespace = Namespace::parse("braintest").unwrap();
        config.packs = vec!["kg".to_string(), "brain".to_string()];
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+brain")
    }

    /// Server whose pack set includes `comm`, whose `send` verb echoes the
    /// dispatching actor directly in its JSON response (`"from": from_actor`,
    /// where `from_actor = token.actor().id`). Used to verify per-request
    /// actor stamping (ADR-096 Fork 1) without a readback/inbox round-trip.
    fn make_comm_test_server(actor_id: Option<&str>) -> crate::server::KhiveMcpServer {
        let mut config = memory_runtime_config();
        config.default_namespace = Namespace::parse("test").unwrap();
        config.packs = vec!["kg".to_string(), "comm".to_string()];
        config.actor_id = actor_id.map(str::to_string);
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+comm")
    }

    fn folded_actor_memory_config(actor: &str) -> RuntimeConfig {
        runtime_config_from_khive_config(
            &KhiveConfig {
                actor: ActorConfig {
                    id: Some(actor.to_string()),
                    ..ActorConfig::default()
                },
                ..KhiveConfig::default()
            },
            memory_runtime_config(),
        )
    }

    // ── map_response (pure, MCP-specific) ─────────────────────────────────────

    const CFG: &str = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
    const NS: &str = "test";

    fn frame_ok(result: &str) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: true,
            result: Some(result.to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        }
    }

    fn frame_err(error: Option<&str>) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: error.map(str::to_string),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        }
    }

    // These four tests exercise `map_response` branches that now also increment
    // the process-global fallback counters (via `record_fallback`) — `#[serial]`
    // + a reset at the top keeps their counter assertions deterministic against
    // any other test in this file that touches the same counters.

    #[test]
    #[serial]
    fn map_response_namespace_mismatch_yields_none() {
        reset_fallback_counters();
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: true,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };
        assert!(map_response(resp, CFG, NS).is_none());
        assert_eq!(fallback_count(FallbackReason::NamespaceMismatch), 1);
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 0);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    #[serial]
    fn map_response_config_mismatch_yields_none() {
        reset_fallback_counters();
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: true,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };
        assert!(map_response(resp, CFG, NS).is_none());
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
        assert_eq!(fallback_count(FallbackReason::NamespaceMismatch), 0);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    #[serial]
    fn map_response_legacy_daemon_missing_echo_yields_none() {
        // A pre-config_id daemon omits served_config_id (→ None). Even on an
        // ok=true result the client MUST fall back to local dispatch.
        reset_fallback_counters();
        let resp = DaemonResponseFrame {
            ok: true,
            result: Some("served-by-broad-registry".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: None,
            version_mismatch: false,
            daemon_protocol_version: 0,
            metrics: None,
            request_id: None,
        };
        assert!(map_response(resp, CFG, NS).is_none());
        // The served_config_id-echo path is bucketed under config_mismatch —
        // there is no separate reason for "echo missing/drifted" in the closed
        // 5-value reason set.
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    #[serial]
    fn map_response_echo_drift_yields_none() {
        // A daemon serving under a different config (echo != expected) is not
        // trusted, even without an explicit config_mismatch flag.
        reset_fallback_counters();
        let resp = DaemonResponseFrame {
            ok: true,
            result: Some("served-by-other-config".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(
                "packs=[kg,gtd];db=/x;embed=none;extra=[];backend=main".to_string(),
            ),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };
        assert!(map_response(resp, CFG, NS).is_none());
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    fn map_response_ok_with_result_yields_some_ok() {
        match map_response(frame_ok("the-result"), CFG, NS) {
            Some(Ok(s)) => assert_eq!(s, "the-result"),
            other => panic!("expected Some(Ok(\"the-result\")), got {other:?}"),
        }
    }

    #[test]
    fn map_response_ok_with_no_result_yields_some_ok_empty() {
        let resp = DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };
        match map_response(resp, CFG, NS) {
            Some(Ok(s)) => assert_eq!(s, ""),
            other => panic!("expected Some(Ok(\"\")), got {other:?}"),
        }
    }

    #[test]
    fn map_response_not_ok_yields_some_err_preserving_message() {
        match map_response(frame_err(Some("boom: bad verb")), CFG, NS) {
            Some(Err(McpError { message, .. })) => {
                assert!(message.contains("boom: bad verb"), "got: {message}");
            }
            other => panic!("expected Some(Err(..)), got {other:?}"),
        }
    }

    #[test]
    fn map_response_not_ok_without_message_yields_contextual_err() {
        match map_response(frame_err(None), CFG, NS) {
            Some(Err(McpError { message, .. })) => {
                assert!(!message.is_empty(), "fallback message must not be empty");
                assert!(
                    message.contains("daemon returned an error"),
                    "fallback must say 'daemon returned an error'; got: {message}"
                );
            }
            other => panic!("expected Some(Err(..)), got {other:?}"),
        }
    }

    #[test]
    fn map_response_version_mismatch_yields_explicit_error() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some("daemon protocol mismatch: client=0 daemon=1 — rebuild/update the client binary (make local)".to_string()),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: true,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };
        match map_response(resp, CFG, NS) {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("protocol mismatch"),
                    "version mismatch error must name the mismatch; got: {message}"
                );
                assert!(
                    message.contains("make local"),
                    "version mismatch error must tell the operator what to do; got: {message}"
                );
            }
            other => panic!("expected Some(Err(..)): got {other:?}"),
        }
    }

    #[test]
    fn map_response_version_mismatch_without_error_field_synthesizes_message() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: true,
            daemon_protocol_version: 99,
            metrics: None,
            request_id: None,
        };
        match map_response(resp, CFG, NS) {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("protocol mismatch"),
                    "synthesized message must name the mismatch; got: {message}"
                );
                assert!(
                    message.contains("99"),
                    "synthesized message must include daemon version; got: {message}"
                );
            }
            other => panic!("expected Some(Err(..)): got {other:?}"),
        }
    }

    // ── daemon_fallback telemetry: counters (ADR-091 F1) ──────────────────────
    //
    // `no_socket` remains a production fallback. `parse_failure` and
    // `protocol_mismatch` are reserved closed-vocabulary counters with no
    // production `record_fallback` call site after #644: both are terminal
    // once a real request is written. Exercise all three directly here so
    // their metrics compatibility remains explicit without implying that a
    // post-write ambiguity can still reach local dispatch.

    #[test]
    #[serial]
    fn record_fallback_no_socket_increments_matching_counter_and_total() {
        reset_fallback_counters();
        record_fallback(FallbackReason::NoSocket, CFG, None, NS);
        assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
        assert_eq!(fallback_count(FallbackReason::ParseFailure), 0);
        assert_eq!(fallback_count(FallbackReason::ProtocolMismatch), 0);
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 0);
        assert_eq!(fallback_count(FallbackReason::NamespaceMismatch), 0);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    #[serial]
    fn record_fallback_parse_failure_increments_matching_counter_and_total() {
        reset_fallback_counters();
        record_fallback(FallbackReason::ParseFailure, CFG, None, NS);
        assert_eq!(fallback_count(FallbackReason::ParseFailure), 1);
        assert_eq!(fallback_count(FallbackReason::NoSocket), 0);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    #[serial]
    fn record_fallback_protocol_mismatch_increments_matching_counter_and_total() {
        reset_fallback_counters();
        record_fallback(FallbackReason::ProtocolMismatch, CFG, None, NS);
        assert_eq!(fallback_count(FallbackReason::ProtocolMismatch), 1);
        assert_eq!(fallback_count(FallbackReason::ParseFailure), 0);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    #[serial]
    fn record_fallback_config_id_daemon_defaults_to_none_literal_when_absent() {
        // The counter side effect proves the call completes and increments
        // exactly once when `config_id_daemon` is `None` (the common case when
        // no decodable daemon response supplied a served config id).
        reset_fallback_counters();
        record_fallback(FallbackReason::NoSocket, CFG, None, NS);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    fn first_config_mismatch_field_follows_fingerprint_order() {
        let client = "packs=[kg];db=/private/client.db;embed=none;extra=[];fresh_tail=true;\
                      blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=client-policy";
        let daemon = "packs=[kg,gtd];db=/private/daemon.db;embed=none;extra=[];fresh_tail=true;\
                      blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=daemon-policy";

        assert_eq!(first_config_mismatch_field(client, Some(daemon)), "packs");
    }

    #[test]
    fn first_config_mismatch_field_names_fresh_tail_from_computed_ids() {
        let config = RuntimeConfig::no_embeddings();
        let enabled = crate::server::compute_config_id_with_ann_fresh_tail(&config, None, true);
        let disabled = crate::server::compute_config_id_with_ann_fresh_tail(&config, None, false);

        assert_eq!(
            first_config_mismatch_field(&enabled, Some(&disabled)),
            "fresh_tail"
        );
    }

    #[test]
    fn first_config_mismatch_field_names_blob_hydration_budget_from_computed_ids() {
        let config = RuntimeConfig::no_embeddings();
        let mut changed = config.clone();
        changed.blob_hydration_bytes /= 2;
        let client = crate::server::compute_config_id_with_ann_fresh_tail(&config, None, true);
        let daemon = crate::server::compute_config_id_with_ann_fresh_tail(&changed, None, true);

        assert_eq!(
            first_config_mismatch_field(&client, Some(&daemon)),
            "blob_hydration_bytes"
        );
    }

    #[test]
    fn legacy_config_id_reports_the_new_blob_budget_as_its_first_mismatch() {
        let legacy = "packs=[kg];db=:memory:;embed=none;extra=[];fresh_tail=true;\
                      backend=main;outbound=[];git_write=policy";
        let current = "packs=[kg];db=:memory:;embed=none;extra=[];fresh_tail=true;\
                       blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=policy";

        assert_eq!(
            first_config_mismatch_field(current, Some(legacy)),
            "blob_hydration_bytes"
        );
    }

    #[test]
    fn first_config_mismatch_field_names_later_field_from_computed_ids() {
        let config = RuntimeConfig::no_embeddings();
        let mut changed = config.clone();
        changed.allowed_outbound_namespaces = vec![Namespace::parse("remote").unwrap()];
        let client = crate::server::compute_config_id_with_ann_fresh_tail(&config, None, true);
        let daemon = crate::server::compute_config_id_with_ann_fresh_tail(&changed, None, true);

        assert_eq!(
            first_config_mismatch_field(&client, Some(&daemon)),
            "outbound"
        );
    }

    #[test]
    fn first_config_mismatch_field_names_caller_enrollment_policy() {
        let base = RuntimeConfig::no_embeddings();
        let enrolled = RuntimeConfig {
            gate: Arc::new(khive_runtime::CallerEnrollmentGate::new(
                vec!["lambda:enrolled".to_string()],
                false,
            )),
            ..base.clone()
        };
        let revoked = RuntimeConfig {
            gate: Arc::new(khive_runtime::CallerEnrollmentGate::new(Vec::new(), false)),
            ..base
        };
        let client = crate::server::compute_config_id_with_ann_fresh_tail(&enrolled, None, true);
        let daemon = crate::server::compute_config_id_with_ann_fresh_tail(&revoked, None, true);

        assert_eq!(first_config_mismatch_field(&client, Some(&daemon)), "gate");
    }

    #[test]
    fn first_config_mismatch_field_names_backend_topology_without_values() {
        let base = "packs=[kg];db=:memory:;embed=none;extra=[];fresh_tail=true;\
                    blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=policy";
        let client =
            format!("{base};backends=[main:Sqlite:/private/client.db];pack_backends=[kg=main]");
        let daemon =
            format!("{base};backends=[main:Sqlite:/private/daemon.db];pack_backends=[kg=main]");

        assert_eq!(
            first_config_mismatch_field(&client, Some(&daemon)),
            "backends"
        );
    }

    #[test]
    fn first_config_mismatch_field_recognizes_read_only_runtime_mode() {
        let config = RuntimeConfig::no_embeddings();
        let writable =
            crate::server::compute_config_id_with_runtime_policies(&config, None, true, false);
        let read_only =
            crate::server::compute_config_id_with_runtime_policies(&config, None, true, true);

        assert_eq!(
            first_config_mismatch_field(&read_only, Some(&writable)),
            "backend",
            "storage-mode separation must retain a structured mismatch field"
        );
    }

    #[test]
    #[serial]
    fn map_response_config_mismatch_logs_opaque_ids_and_field_without_values() {
        reset_fallback_counters();
        let client =
            "packs=[kg];db=/private/client-topology/main.db;embed=none;extra=[];fresh_tail=true;\
                      blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=same-policy";
        let daemon =
            "packs=[kg];db=/private/daemon-topology/main.db;embed=none;extra=[];fresh_tail=true;\
                      blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=same-policy";
        let response = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: true,
            served_config_id: Some(daemon.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };

        let events = capture_sync_events(|| {
            assert!(map_response(response, client, NS).is_none());
        });

        assert!(events.contains("daemon_fallback"), "{events}");
        assert!(events.contains("config_mismatch_field=\"db\""), "{events}");
        assert!(events.contains(&opaque_config_id(client)), "{events}");
        assert!(events.contains(&opaque_config_id(daemon)), "{events}");
        assert!(!events.contains(client), "{events}");
        assert!(!events.contains(daemon), "{events}");
        assert!(!events.contains("client-topology"), "{events}");
        assert!(!events.contains("daemon-topology"), "{events}");
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
        assert_eq!(fallback_total(), 1);
    }

    #[test]
    #[serial]
    fn map_response_matching_config_emits_no_fallback_diagnostic() {
        reset_fallback_counters();
        let events = capture_sync_events(|| {
            assert!(matches!(map_response(frame_ok("ok"), CFG, NS), Some(Ok(_))));
        });

        assert!(
            events.is_empty(),
            "matching config logged fallback: {events}"
        );
        assert_eq!(fallback_total(), 0);
    }

    #[test]
    #[serial]
    fn fallback_total_sums_all_reason_counters() {
        reset_fallback_counters();
        record_fallback(FallbackReason::ConfigMismatch, CFG, Some("other-cfg"), NS);
        record_fallback(
            FallbackReason::NamespaceMismatch,
            CFG,
            Some(CFG),
            "other-ns",
        );
        record_fallback(FallbackReason::NoSocket, CFG, None, NS);
        record_fallback(FallbackReason::ParseFailure, CFG, None, NS);
        record_fallback(FallbackReason::ProtocolMismatch, CFG, None, NS);

        let sum = fallback_count(FallbackReason::ConfigMismatch)
            + fallback_count(FallbackReason::NamespaceMismatch)
            + fallback_count(FallbackReason::NoSocket)
            + fallback_count(FallbackReason::ParseFailure)
            + fallback_count(FallbackReason::ProtocolMismatch);
        assert_eq!(sum, 5);
        assert_eq!(
            fallback_total(),
            5,
            "total must equal the sum of all reasons"
        );
    }

    // ── KHIVE_DAEMON_STRICT graduated fail-loud policy (D2) ───────────────────
    //
    // These tests prove the graduated behavior via `FALLBACK_STRICT_VIOLATIONS`:
    // an `Illegitimate` reason bumps it if and only if strict mode is on; every
    // other reason/mode combination must never bump it.

    fn with_daemon_strict<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("KHIVE_DAEMON_STRICT").ok();
        match value {
            Some(v) => std::env::set_var("KHIVE_DAEMON_STRICT", v),
            None => std::env::remove_var("KHIVE_DAEMON_STRICT"),
        }
        let result = f();
        match prev {
            Some(v) => std::env::set_var("KHIVE_DAEMON_STRICT", v),
            None => std::env::remove_var("KHIVE_DAEMON_STRICT"),
        }
        result
    }

    #[test]
    #[serial]
    fn record_fallback_config_mismatch_strict_off_never_bumps_strict_violations() {
        with_daemon_strict(None, || {
            reset_fallback_counters();
            record_fallback(FallbackReason::ConfigMismatch, CFG, Some("other-cfg"), NS);
            assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
            assert_eq!(
                fallback_strict_violations(),
                0,
                "strict mode is OFF (default local-dev behavior) — the illegitimate \
                 reason must still bump its own counter, but never the strict-violations one"
            );
        });
    }

    #[test]
    #[serial]
    fn record_fallback_config_mismatch_strict_on_bumps_strict_violations() {
        with_daemon_strict(Some("1"), || {
            reset_fallback_counters();
            record_fallback(FallbackReason::ConfigMismatch, CFG, Some("other-cfg"), NS);
            assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
            assert_eq!(
                fallback_strict_violations(),
                1,
                "KHIVE_DAEMON_STRICT=1 + an Illegitimate reason (config_mismatch) must \
                 bump the strict-violations counter (D2-R1)"
            );
        });
    }

    #[test]
    #[serial]
    fn record_fallback_namespace_mismatch_strict_on_bumps_strict_violations() {
        with_daemon_strict(Some("1"), || {
            reset_fallback_counters();
            record_fallback(
                FallbackReason::NamespaceMismatch,
                CFG,
                Some(CFG),
                "other-ns",
            );
            assert_eq!(
                fallback_strict_violations(),
                1,
                "KHIVE_DAEMON_STRICT=1 + an Illegitimate reason (namespace_mismatch) must \
                 bump the strict-violations counter (D2-R1)"
            );
        });
    }

    #[test]
    #[serial]
    fn record_fallback_no_socket_strict_on_never_bumps_strict_violations() {
        with_daemon_strict(Some("1"), || {
            reset_fallback_counters();
            record_fallback(FallbackReason::NoSocket, CFG, None, NS);
            assert_eq!(
                fallback_strict_violations(),
                0,
                "NoSocket is the ADR-049-mandated no-daemon path — it must NEVER be \
                 elevated, even in strict mode (D2-R3)"
            );
        });
    }

    #[test]
    #[serial]
    fn record_fallback_protocol_mismatch_strict_on_never_bumps_strict_violations() {
        with_daemon_strict(Some("1"), || {
            reset_fallback_counters();
            record_fallback(FallbackReason::ProtocolMismatch, CFG, None, NS);
            assert_eq!(
                fallback_strict_violations(),
                0,
                "the reserved ProtocolMismatch metric retains its historical \
                 rollout-transient tier and is never a strict violation"
            );
        });
    }

    #[test]
    #[serial]
    fn record_fallback_parse_failure_strict_on_never_bumps_strict_violations() {
        with_daemon_strict(Some("1"), || {
            reset_fallback_counters();
            record_fallback(FallbackReason::ParseFailure, CFG, None, NS);
            assert_eq!(
                fallback_strict_violations(),
                0,
                "the reserved ParseFailure metric retains its historical \
                 rollout-transient tier and is never a strict violation"
            );
        });
    }

    #[test]
    fn fallback_reason_severity_matches_the_d2_legitimacy_table() {
        assert_eq!(
            FallbackReason::ConfigMismatch.severity(),
            FallbackSeverity::Illegitimate
        );
        assert_eq!(
            FallbackReason::NamespaceMismatch.severity(),
            FallbackSeverity::Illegitimate
        );
        assert_eq!(
            FallbackReason::ProtocolMismatch.severity(),
            FallbackSeverity::RolloutTransient
        );
        assert_eq!(
            FallbackReason::ParseFailure.severity(),
            FallbackSeverity::RolloutTransient
        );
        assert_eq!(
            FallbackReason::NoSocket.severity(),
            FallbackSeverity::NoDaemon
        );
    }

    // ── fallback_or_reject: strict mode fails the request (#947) ──────────────
    //
    // #947: `KHIVE_DAEMON_STRICT=1` must turn a would-be fallback into a
    // caller-visible error naming the reason, for EVERY `FallbackReason` —
    // not just the `Illegitimate` tier that `record_fallback`'s WARN/ERROR
    // log-level graduation (D2-R1) cares about. These tests exercise the
    // decision function directly, at the same private-fn level as the
    // `record_fallback_*` tests above, so they run in milliseconds instead of
    // needing a real unreachable-socket round trip.

    #[test]
    #[serial]
    fn fallback_or_reject_non_strict_returns_none_and_still_counts() {
        with_daemon_strict(None, || {
            reset_fallback_counters();
            let out = fallback_or_reject(FallbackReason::NoSocket, CFG, None, NS);
            assert!(
                out.is_none(),
                "non-strict mode must keep completing locally, unchanged by #947"
            );
            assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
        });
    }

    #[test]
    #[serial]
    fn fallback_or_reject_strict_no_socket_errors_naming_the_reason() {
        with_daemon_strict(Some("1"), || {
            reset_fallback_counters();
            match fallback_or_reject(FallbackReason::NoSocket, CFG, None, NS) {
                Some(Err(McpError { message, .. })) => {
                    assert!(
                        message.contains("no_socket"),
                        "error must name the fallback reason: {message}"
                    );
                    assert!(
                        message.contains("KHIVE_DAEMON_STRICT"),
                        "error should point at the mode that caused the rejection: {message}"
                    );
                }
                other => panic!("strict mode must reject the request, got {other:?}"),
            }
            // Counters/telemetry are untouched by this change — still exactly
            // what `record_fallback` alone would have produced.
            assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
            assert_eq!(fallback_total(), 1);
        });
    }

    #[test]
    #[serial]
    fn fallback_or_reject_strict_config_mismatch_errors_naming_the_reason() {
        with_daemon_strict(Some("1"), || {
            reset_fallback_counters();
            match fallback_or_reject(FallbackReason::ConfigMismatch, CFG, Some("other-cfg"), NS) {
                Some(Err(McpError { message, .. })) => {
                    assert!(message.contains("config_mismatch"), "{message}");
                }
                other => panic!("strict mode must reject the request, got {other:?}"),
            }
            // An `Illegitimate`-tier reason still bumps the pre-existing
            // strict-violations counter exactly as it did before #947 — this
            // change only affects the return value, never the telemetry.
            assert_eq!(fallback_strict_violations(), 1);
        });
    }

    // ── forward_or_spawn fallback (env-mutating → serial) ─────────────────────

    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_returns_none_when_no_daemon_set() {
        clear_daemon_env();
        reset_fallback_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: "test".to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let out = forward_or_spawn(&frame).await;
        assert!(out.is_none());
        assert!(!sock.exists());
        // KHIVE_NO_DAEMON is an explicit operator opt-out, not one of the 5
        // silent-fallback reasons this telemetry tracks — it must NOT bump the
        // fallback counters (that would be noisy for legitimate always-local
        // deployments).
        assert_eq!(fallback_total(), 0);

        clear_daemon_env();
    }

    // #898: genuine daemon-unreachable fallback (no `KHIVE_NO_DAEMON` opt-out),
    // where the respawn THIS call attempted can be positively confirmed dead.
    // `spawn_daemon()` really runs here (`SPAWN_COUNT` bumps), spawning this
    // same test binary re-invoked with unrecognized `mcp --daemon` args; it
    // exits immediately without ever binding the socket — mirroring the
    // 2026-07-12 incident's version-skewed binary (`error: Unrecognized
    // option: 'daemon'`). This must surface as a loud, caller-visible error in
    // BOTH strict and non-strict mode: unlike the ordinary "no daemon
    // reachable, cause unknown" fallback, a respawn this process made and can
    // prove failed is never eligible for a silent local-dispatch completion.
    // Each run pays the ~5s forward deadline plus the boot-quiescence reprobe
    // — see `forward_or_spawn_blocks_on_boot_quiescence_before_local_fallback`
    // below, which asserts that wait is unaffected by this change.

    fn unreachable_daemon_frame(config_id: &str) -> DaemonRequestFrame {
        DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        }
    }

    #[test]
    fn socket_connect_error_classification_distinguishes_absence_from_denial() {
        for error_code in [libc::ENOENT, libc::ECONNREFUSED] {
            assert!(matches!(
                classify_socket_connect_error(std::io::Error::from_raw_os_error(error_code)),
                ForwardOutcome::NoSocket
            ));
        }

        for error_code in [libc::EACCES, libc::EPERM] {
            match classify_socket_connect_error(std::io::Error::from_raw_os_error(error_code)) {
                ForwardOutcome::Unreachable {
                    kind,
                    os_error_code,
                } => {
                    assert_eq!(kind, std::io::ErrorKind::PermissionDenied);
                    assert_eq!(os_error_code, Some(error_code));
                }
                _ => panic!("EACCES/EPERM must be unreachable, never safe-to-recover NoSocket"),
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn permission_denied_socket_fails_without_lifecycle_or_local_fallback() {
        clear_daemon_env();
        reset_counters();
        reset_fallback_counters();
        let _cleanup = RecoveryTestGuard::new();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::set_var(
            "KHIVE_RECOVERER_LOCK",
            dir.path().join("khived.recoverer.lock"),
        );
        FORCED_CONNECT_ERROR.store(libc::EPERM, std::sync::atomic::Ordering::SeqCst);

        let config_id = crate::server::compute_config_id(&memory_runtime_config(), None);
        let result = forward_or_spawn(&unreachable_daemon_frame(&config_id)).await;

        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an unreachable socket must never trigger a kill"
        );
        assert_eq!(
            SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an unreachable socket must never trigger a spawn"
        );
        assert_eq!(
            fallback_total(),
            0,
            "an unreachable socket must return an error, not local fallback"
        );
        match result {
            Some(Err(error)) => {
                assert!(
                    error.message.contains("cannot reach daemon socket"),
                    "the caller-visible error must name the unreachable socket: {}",
                    error.message
                );
                let data = error.data.as_ref().expect("unreachable error data");
                assert_eq!(data["reason"], "daemon_unreachable");
                assert_eq!(data["os_error_kind"], "PermissionDenied");
                assert_eq!(data["os_error_code"], libc::EPERM);
            }
            other => panic!(
                "unreachable must be Some(Err), never None/local fallback or success: {other:?}"
            ),
        }
    }

    #[tokio::test]
    #[serial]
    async fn stale_socket_connection_refused_remains_safe_to_recover() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &sock);

        {
            let listener = tokio::net::UnixListener::bind(&sock).expect("bind stale socket");
            drop(listener);
        }
        assert!(sock.exists(), "dropped listener must leave a stale socket");

        let config_id = crate::server::compute_config_id(&memory_runtime_config(), None);
        assert!(matches!(
            try_forward_inner(&unreachable_daemon_frame(&config_id)).await,
            ForwardOutcome::NoSocket
        ));

        clear_daemon_env();
    }

    struct RespawnDisclosureFixture {
        original_home: Option<std::ffi::OsString>,
        _home: tempfile::TempDir,
        sentinel: &'static str,
    }

    fn daemon_script_fixture(
        dir: &tempfile::TempDir,
        name: &str,
        body: &str,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("write daemon executable fixture");
        let mut permissions = std::fs::metadata(&path)
            .expect("read daemon executable fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions)
            .expect("make daemon executable fixture executable");
        path
    }

    /// The config path threaded through `forward_or_spawn_with_config_and_packs` must
    /// actually appear on the spawned daemon's command line; a script
    /// fixture records its argv so the assertion observes the real child
    /// invocation (`crates/kkernel/src/exec.rs`'s spy seam proves the exec
    /// side hands the path over; this side proves it reaches `argv`).
    #[test]
    #[serial]
    fn spawn_daemon_with_exe_and_config_appends_config_flag_to_command_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("argv.txt");
        let exe = daemon_script_fixture(
            &dir,
            "record-argv.sh",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
        );

        let config_path = dir.path().join("selected.toml");
        let mut child = spawn_daemon_with_exe_and_config(&exe, Some(&config_path), None, None)
            .expect("spawn argv-recording fixture");
        let status = child.wait().expect("wait for argv-recording fixture");
        assert!(status.success(), "fixture must exit 0: {status}");

        let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
        assert_eq!(
            recorded.trim_end(),
            format!("mcp --daemon --config {}", config_path.display()),
            "the explicit config selection must reach the daemon command line"
        );
    }

    /// The pack list threaded through `forward_or_spawn_with_config_and_packs` must
    /// reach the spawned daemon's command line as explicit `--pack` flags —
    /// the fix for the auto-spawn dropping a client's resolved `KHIVE_PACKS`
    /// (or `--pack`-flag, or config-file `[runtime].packs`) selection when it
    /// spawns a fresh daemon (khive-oss#1941).
    #[test]
    #[serial]
    fn spawn_daemon_with_exe_and_config_appends_pack_flags_to_command_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("argv.txt");
        let exe = daemon_script_fixture(
            &dir,
            "record-argv.sh",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
        );

        let packs = vec!["kg".to_string(), "gtd".to_string(), "formal".to_string()];
        let mut child = spawn_daemon_with_exe_and_config(&exe, None, None, Some(&packs))
            .expect("spawn argv-recording fixture");
        let status = child.wait().expect("wait for argv-recording fixture");
        assert!(status.success(), "fixture must exit 0: {status}");

        let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
        assert_eq!(
            recorded.trim_end(),
            "mcp --daemon --pack kg --pack gtd --pack formal",
            "the caller's resolved pack set must reach the daemon command line as --pack flags"
        );
    }

    /// A `None` pack list (the default, backward-compatible shape used by
    /// bare `forward_or_spawn`/`spawn_daemon`) must append no `--pack` flags
    /// at all — the spawned daemon falls through to its own env/config
    /// resolution exactly as before this fix.
    #[test]
    #[serial]
    fn spawn_daemon_with_exe_and_config_omits_pack_flags_when_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("argv.txt");
        let exe = daemon_script_fixture(
            &dir,
            "record-argv.sh",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
        );

        let mut child = spawn_daemon_with_exe_and_config(&exe, None, None, None)
            .expect("spawn argv-recording fixture");
        let status = child.wait().expect("wait for argv-recording fixture");
        assert!(status.success(), "fixture must exit 0: {status}");

        let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
        assert_eq!(
            recorded.trim_end(),
            "mcp --daemon",
            "no packs supplied must mean no --pack flags on the daemon command line"
        );
    }

    /// An accepted `--db :memory:` override must follow the spawn: without
    /// `--db :memory:` on the daemon's own command line, a fresh daemon
    /// would bind the config's declared persistent backend files, the exact
    /// inversion of the operator's ephemeral invocation.
    #[test]
    #[serial]
    fn spawn_daemon_with_exe_and_config_appends_memory_db_flag_to_command_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("argv.txt");
        let exe = daemon_script_fixture(
            &dir,
            "record-argv.sh",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
        );

        let config_path = dir.path().join("selected.toml");
        let mut child =
            spawn_daemon_with_exe_and_config(&exe, Some(&config_path), Some(":memory:"), None)
                .expect("spawn argv-recording fixture");
        let status = child.wait().expect("wait for argv-recording fixture");
        assert!(status.success(), "fixture must exit 0: {status}");

        let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
        assert_eq!(
            recorded.trim_end(),
            format!(
                "mcp --daemon --config {} --db :memory:",
                config_path.display()
            ),
            "the ephemeral :memory: override must reach the daemon command line"
        );
    }

    /// A concrete override handed to the spawn seam must reach the spawned
    /// daemon's command line: this is the single-backend case (no
    /// `[[backends]]` declared), where the fresh daemon has no
    /// config-declared database path and would otherwise bind
    /// `$HOME/.khive/khive.db` instead of the operator's override — a
    /// `config_id` mismatch against the client's override-anchored frame.
    /// The redundant-concrete multi-backend case never reaches this
    /// function: the caller (`run_exec_inline_with_forward` in
    /// `crates/kkernel/src/exec.rs`) withholds the override there because
    /// the frame's fingerprint has already been normalized to the
    /// no-override anchor (`normalize_redundant_db_override_with_source`).
    #[test]
    #[serial]
    fn spawn_daemon_with_exe_and_config_forwards_concrete_db_flag_to_command_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("argv.txt");
        let exe = daemon_script_fixture(
            &dir,
            "record-argv.sh",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
        );

        let mut child = spawn_daemon_with_exe_and_config(&exe, None, Some("/tmp/main.db"), None)
            .expect("spawn argv-recording fixture");
        let status = child.wait().expect("wait for argv-recording fixture");
        assert!(status.success(), "fixture must exit 0: {status}");

        let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
        assert_eq!(
            recorded.trim_end(),
            "mcp --daemon --db /tmp/main.db",
            "the concrete override handed to the spawn seam must reach the daemon command line"
        );
    }

    /// A writer that momentarily still owns the fixture inode must not turn
    /// the argv-forwarding family into an ETXTBSY flake. The production spawn
    /// seam retries only ExecutableFileBusy, so holding this file open for
    /// writing forces the exact failure class reported by coverage CI.
    #[test]
    #[serial]
    fn spawn_daemon_retries_a_transient_executable_file_busy_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = daemon_script_fixture(&dir, "temporarily-busy.sh", "#!/bin/sh\nexit 0\n");
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&exe)
            .expect("hold executable fixture open for writing");
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(15));
            drop(writer);
        });

        let mut child = spawn_daemon_with_exe_and_config(&exe, None, None, None)
            .expect("transient ETXTBSY must be retried");
        release.join().expect("fixture writer release thread");
        let status = child.wait().expect("wait for executable fixture");
        assert!(status.success(), "fixture must exit 0: {status}");
    }

    /// Blank out `"..."` string-literal contents (escapes and
    /// backslash-newline continuations included) so a spawn-seam name
    /// mentioned in an error message or doc comment can never read as a
    /// call.
    fn strip_string_literals_for_census(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut in_string = false;
        for line in text.lines() {
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                if in_string {
                    if c == '\\' {
                        out.push(' ');
                        if chars.next().is_some() {
                            out.push(' ');
                        }
                        continue;
                    }
                    if c == '"' {
                        in_string = false;
                        out.push('"');
                    } else {
                        out.push(' ');
                    }
                } else {
                    if c == '"' {
                        in_string = true;
                    }
                    out.push(c);
                }
            }
            out.push('\n');
        }
        out
    }

    /// `true` if `text` contains a call to `name` — `name` immediately
    /// followed by `(` (optional whitespace between, including newlines —
    /// `rustfmt` is free to break a long call onto its own line), a
    /// non-identifier character (or start of text) before it, and not
    /// inside a string literal. Anchoring both boundaries matters here
    /// specifically because `spawn_daemon_with_exe(` is a prefix-shaped
    /// substring of `spawn_daemon_with_exe_and_config(` up to the
    /// `_and_config` suffix — a plain `contains` check would still tell
    /// them apart by luck (the character after `exe` differs), but a
    /// boundary check makes that non-collision load-bearing instead of
    /// incidental.
    fn calls_name_for_census(text: &str, name: &str) -> bool {
        fn is_ident_byte(b: u8) -> bool {
            b.is_ascii_alphanumeric() || b == b'_'
        }
        let text = strip_string_literals_for_census(text);
        let bytes = text.as_bytes();
        let mut search_from = 0usize;
        while let Some(rel) = text[search_from..].find(name) {
            let idx = search_from + rel;
            let before_ok = idx == 0 || !is_ident_byte(bytes[idx - 1]);
            let after = idx + name.len();
            let mut j = after;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let after_ok = j < bytes.len() && bytes[j] == b'(';
            let is_definition = text[..idx].ends_with("fn ");
            if before_ok && after_ok && !is_definition {
                return true;
            }
            search_from = idx + 1;
        }
        false
    }

    /// Regression for a scanner that only tolerated a space/tab between a
    /// seam name and its `(` — see the identical fix and rationale for
    /// `khive-runtime`'s `calls_name`.
    #[test]
    fn calls_name_for_census_matches_across_a_newline_before_the_parenthesis() {
        let text = "fn wraps_it() {\n    spawn_daemon_with_exe\n        (exe)\n}";
        assert!(calls_name_for_census(text, "spawn_daemon_with_exe"));
    }

    /// The name of the function whose signature starts at `sig_line`
    /// (already stripped of leading whitespace), if any.
    fn fn_name_from_signature_for_census(sig_line: &str) -> Option<&str> {
        let mut rest = sig_line;
        for prefix in ["pub(crate) ", "pub(super) ", "pub "] {
            if let Some(stripped) = rest.strip_prefix(prefix) {
                rest = stripped;
            }
        }
        let rest = rest
            .strip_prefix("async fn ")
            .or_else(|| rest.strip_prefix("fn "))?;
        Some(rest.split(['(', '<', ' ']).next().unwrap_or(rest))
    }

    /// Recursively collect every `.rs` file under `dir` into `out`.
    fn collect_rust_files_for_census(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                collect_rust_files_for_census(&path, out);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    /// `spawn_daemon_with_exe_and_config` and its thin wrapper
    /// `spawn_daemon_with_exe` are both module-private — visible only
    /// inside `daemon.rs` and its descendant modules — so unlike the
    /// config-ledger seam (`with_event_store`, `pub` and reachable from any
    /// crate in the workspace), the real population for this census is
    /// bounded to this file plus the `daemon/` submodule directory, not the
    /// whole workspace. This walks that directory explicitly instead of
    /// hard-coding "just this file" so a future submodule under `daemon/`
    /// is covered without anyone remembering to widen the scan.
    fn daemon_module_sources() -> Vec<(std::path::PathBuf, String)> {
        let this_file = std::path::PathBuf::from(file!());
        let daemon_rs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon.rs");
        let daemon_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon");
        let mut files = vec![daemon_rs];
        if daemon_dir.is_dir() {
            collect_rust_files_for_census(&daemon_dir, &mut files);
        }
        let _ = this_file; // `file!()` is relative; CARGO_MANIFEST_DIR-joined paths are the source of truth.
        files
            .into_iter()
            .filter_map(|path| {
                let text = std::fs::read_to_string(&path).ok()?;
                Some((path, text))
            })
            .collect()
    }

    /// Every direct test caller of the spawn seam — `daemon.rs` and its
    /// `daemon/` submodules, the only places `spawn_daemon_with_exe[_and_
    /// config]` are visible from — mutates the process-wide `SPAWN_COUNT`,
    /// so it must share the default serial group with tests that reset and
    /// assert that counter.
    #[test]
    fn direct_spawn_seam_test_callers_are_serialized() {
        let sources = daemon_module_sources();
        assert!(
            !sources.is_empty(),
            "the daemon-module source scan found no .rs files under \
             crates/khive-mcp/src/daemon.rs or src/daemon/; the census's own file walk \
             is broken, not the population it walks"
        );

        let spawn_calls = ["spawn_daemon_with_exe_and_config", "spawn_daemon_with_exe"];
        let mut candidate_count = 0usize;
        let mut offenders = Vec::new();

        for (path, text) in &sources {
            let lines: Vec<&str> = text.lines().collect();
            let test_starts: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| {
                    let trimmed = line.trim();
                    trimmed == "#[test]" || trimmed.starts_with("#[tokio::test")
                })
                .map(|(index, _)| index)
                .collect();

            for (index, start) in test_starts.iter().copied().enumerate() {
                let end = test_starts.get(index + 1).copied().unwrap_or(lines.len());
                let span = &lines[start..end];
                let span_text = span.join("\n");
                let matched = spawn_calls
                    .iter()
                    .find(|seam| calls_name_for_census(&span_text, seam));
                let Some(matched) = matched else {
                    continue;
                };
                candidate_count += 1;

                if !span.iter().any(|line| line.trim() == "#[serial]") {
                    let signature_offset = span
                        .iter()
                        .position(|line| {
                            fn_name_from_signature_for_census(line.trim_start()).is_some()
                        })
                        .expect("test span has a function signature");
                    let name =
                        fn_name_from_signature_for_census(span[signature_offset].trim_start())
                            .unwrap_or("<unknown>");
                    offenders.push(format!(
                        "{}:{name} (reaches the spawn seam via `{matched}`)",
                        path.display()
                    ));
                }
            }
        }

        assert!(
            candidate_count > 0,
            "census found zero direct spawn-seam test candidates under the daemon \
             module ({} source files) — the scan is broken, not the population it \
             should have found (this file's own argv-forwarding tests are known \
             direct callers)",
            sources.len()
        );
        assert!(
            offenders.is_empty(),
            "tests that directly call the SPAWN_COUNT-mutating spawn seam must use \
             #[serial]; offenders: {offenders:?}"
        );
    }

    /// The helper behind the retry above must actually retry the exact
    /// number of times ETXTBSY is injected, then succeed on the next
    /// attempt — a fixture that merely holds a file open (as the test above
    /// does) never proves an initial ETXTBSY was observed or counts
    /// attempts, so it passes even against a no-retry implementation on a
    /// filesystem that permits executing a writable-open file.
    #[test]
    fn spawn_retrying_executable_busy_retries_exact_count_then_succeeds() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result: std::io::Result<()> =
            spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || {
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
                } else {
                    Ok(())
                }
            });

        assert!(result.is_ok(), "must succeed once ETXTBSY stops recurring");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "must retry exactly until success: 2 injected ETXTBSY failures + 1 success"
        );
    }

    /// A non-ETXTBSY error must never be retried, even though the backoff
    /// budget has room left.
    #[test]
    fn spawn_retrying_executable_busy_does_not_retry_other_errors() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result: std::io::Result<()> =
            spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            });

        assert!(
            result.is_err(),
            "a non-ETXTBSY error must surface, not be swallowed"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a non-ETXTBSY error must not retry"
        );
    }

    #[derive(Clone, Default)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("captured log mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl CapturedLog {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("captured log mutex poisoned").clone())
                .expect("tracing output is UTF-8")
        }
    }

    fn capture_sync_events(run: impl FnOnce()) -> String {
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        captured.contents()
    }

    #[test]
    fn captured_log_flush_is_a_noop() {
        use std::io::Write;

        let mut captured = CapturedLog::default();
        captured
            .write_all(b"respawn-event")
            .expect("write captured event");
        captured.flush().expect("flush captured event");
        assert_eq!(captured.contents(), "respawn-event");
    }

    impl RespawnDisclosureFixture {
        fn new(sentinel: &'static str) -> Self {
            let original_home = std::env::var_os("HOME");
            let home = tempfile::tempdir().expect("isolated HOME tempdir");
            std::env::set_var("HOME", home.path());
            let log_path = daemon_log_path().expect("HOME resolves daemon log path");
            std::fs::create_dir_all(log_path.parent().expect("daemon log has parent"))
                .expect("create daemon log directory");
            std::fs::write(&log_path, format!("{sentinel}\n")).expect("seed daemon log sentinel");
            Self {
                original_home,
                _home: home,
                sentinel,
            }
        }

        fn assert_output_is_sanitized(&self, output_name: &str, output: &str) {
            let executable = std::env::current_exe()
                .expect("resolve current test executable")
                .display()
                .to_string();
            assert!(
                !output.contains(self.sentinel),
                "shared daemon log content must not reach {output_name}: {output}"
            );
            assert!(
                !output.contains(&executable),
                "absolute daemon executable path must not reach {output_name}: {output}"
            );
        }

        fn assert_caller_output_is_sanitized(&self, error: &McpError) {
            let caller_output = serde_json::to_string(error).expect("serialize caller MCP error");
            self.assert_output_is_sanitized("the caller", &caller_output);
            assert!(
                caller_output.contains("respawn_failed"),
                "caller must receive the stable respawn_failed code: {caller_output}"
            );
            assert!(
                caller_output.contains("make local"),
                "caller must receive safe remediation text: {caller_output}"
            );
        }
    }

    async fn forward_with_exe_and_captured_events(
        frame: &DaemonRequestFrame,
        exe: &std::path::Path,
    ) -> (Option<Result<String, McpError>>, String) {
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let subscriber_guard = tracing::subscriber::set_default(subscriber);
        let output = forward_or_spawn_with_exe(frame, exe).await;
        drop(subscriber_guard);
        let events = captured.contents();
        (output, events)
    }

    impl Drop for RespawnDisclosureFixture {
        fn drop(&mut self) {
            match self.original_home.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    #[serial]
    fn respawn_disclosure_fixture_restores_absent_home() {
        let home = std::env::var_os("HOME").expect("test process has HOME");
        std::env::remove_var("HOME");
        {
            let _fixture =
                RespawnDisclosureFixture::new("KHIVE_RESPAWN_LOG_SENTINEL_ABSENT_HOME_62cc1aeb13");
            assert!(std::env::var_os("HOME").is_some());
        }
        assert!(std::env::var_os("HOME").is_none());
        std::env::set_var("HOME", home);
    }

    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_surfaces_loud_error_when_respawn_confirmed_dead_non_strict() {
        clear_daemon_env();
        reset_fallback_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::remove_var("KHIVE_DAEMON_STRICT");
        let disclosure =
            RespawnDisclosureFixture::new("KHIVE_RESPAWN_LOG_SENTINEL_NON_STRICT_4d9813b72e");
        let exe = daemon_script_fixture(&dir, "exits-before-bind", "#!/bin/sh\nexit 23\n");

        let frame = unreachable_daemon_frame(CFG);
        let (out, events) = forward_with_exe_and_captured_events(&frame, &exe).await;
        disclosure.assert_output_is_sanitized("bridge tracing events", &events);
        assert!(
            events.contains("reason=\"respawn_failed\""),
            "trace must retain the stable reason code: {events}"
        );
        assert!(
            events.contains("failure_category=\"exited_before_bind\""),
            "trace must classify the confirmed failure without raw detail: {events}"
        );

        match out {
            Some(Err(error)) => {
                disclosure.assert_caller_output_is_sanitized(&error);
                let data = error.data.as_ref().expect("respawn error data");
                assert_eq!(data["reason"], "respawn_failed");
                assert!(data.get(STRICT_FALLBACK_MARKER).is_none());
                let message = &error.message;
                assert!(
                    message.contains("respawn failed"),
                    "must name the respawn failure specifically, not a generic \
                     fallback: {message}"
                );
                assert!(
                    message.contains("make local"),
                    "must point the operator at the fix: {message}"
                );
            }
            other => panic!(
                "a respawn attempt confirmed dead must surface loudly even in \
                 non-strict mode (#898) instead of completing the request via \
                 silent local dispatch, got {other:?}"
            ),
        }
        // #898's loud respawn-failure path bypasses the ordinary
        // fallback/telemetry machinery entirely — a confirmed respawn failure
        // is never the legitimate ADR-049 no-daemon case that telemetry
        // exists to count.
        assert_eq!(fallback_count(FallbackReason::NoSocket), 0);

        reset_fallback_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_surfaces_loud_error_when_respawn_confirmed_dead_strict() {
        clear_daemon_env();
        reset_fallback_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::set_var("KHIVE_DAEMON_STRICT", "1");
        let disclosure =
            RespawnDisclosureFixture::new("KHIVE_RESPAWN_LOG_SENTINEL_STRICT_7ac60d5391");
        let exe = daemon_script_fixture(&dir, "exits-before-bind", "#!/bin/sh\nexit 23\n");

        let frame = unreachable_daemon_frame(CFG);
        let (out, events) = forward_with_exe_and_captured_events(&frame, &exe).await;
        disclosure.assert_output_is_sanitized("strict-mode bridge tracing events", &events);
        assert!(
            events.contains("reason=\"respawn_failed\""),
            "strict-mode trace must retain the stable reason code: {events}"
        );
        assert!(
            events.contains("failure_category=\"exited_before_bind\""),
            "strict-mode trace must classify the failure without raw detail: {events}"
        );

        match out {
            Some(Err(error)) => {
                disclosure.assert_caller_output_is_sanitized(&error);
                let data = error.data.as_ref().expect("strict respawn error data");
                assert_eq!(data["reason"], "respawn_failed");
                assert_eq!(data[STRICT_FALLBACK_MARKER], true);
                let message = &error.message;
                assert!(
                    message.contains("respawn failed"),
                    "strict mode must still surface the specific respawn-failure \
                     diagnosis, not the generic no_socket reason: {message}"
                );
            }
            other => panic!(
                "KHIVE_DAEMON_STRICT=1 must reject the request when the daemon is \
                 unreachable, got {other:?}"
            ),
        }
        // Strict mode changes nothing here: #898's loud path is unconditional
        // and never reaches record_fallback/fallback_or_reject at all.
        assert_eq!(fallback_count(FallbackReason::NoSocket), 0);

        reset_fallback_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
        std::env::remove_var("KHIVE_DAEMON_STRICT");
    }

    // ── daemon socket round-trip (env-mutating → serial) ─────────────────────

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_with_injected_exe_sanitizes_spawn_error_without_local_fallback() {
        clear_daemon_env();
        reset_fallback_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::remove_var("KHIVE_DAEMON_STRICT");
        let disclosure =
            RespawnDisclosureFixture::new("KHIVE_RESPAWN_LOG_SENTINEL_SPAWN_ERROR_f6a81b23c9");
        let exe = dir.path().join("not-executable");
        std::fs::write(&exe, "not an executable").expect("write non-executable fixture");

        let frame = unreachable_daemon_frame(CFG);
        let (out, events) = forward_with_exe_and_captured_events(&frame, &exe).await;
        disclosure.assert_output_is_sanitized("spawn-error bridge tracing events", &events);
        assert!(
            events.contains("reason=\"respawn_failed\""),
            "trace must retain the stable reason code: {events}"
        );
        assert!(
            events.contains("failure_category=\"spawn_error\""),
            "trace must classify the process-start failure without raw detail: {events}"
        );
        assert!(
            events.contains("os_error_code=Some(13)"),
            "trace diagnostic must be the numeric permission-denied code only: {events}"
        );
        assert!(
            !events.contains("Permission denied") && !events.contains("os error"),
            "trace must not expose the raw OS error text: {events}"
        );

        match out {
            Some(Err(error)) => {
                disclosure.assert_caller_output_is_sanitized(&error);
                let caller_output =
                    serde_json::to_string(&error).expect("serialize caller MCP error");
                assert!(
                    !caller_output.contains("Permission denied")
                        && !caller_output.contains("os error"),
                    "caller must not receive the raw OS error text: {caller_output}"
                );
            }
            other => panic!(
                "a confirmed process-start failure must return respawn_failed instead of \
                 permitting local dispatch, got {other:?}"
            ),
        }
        assert_eq!(fallback_total(), 0);

        reset_fallback_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_with_injected_exe_falls_back_when_child_stays_alive() {
        clear_daemon_env();
        reset_fallback_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::remove_var("KHIVE_DAEMON_STRICT");
        let exe = daemon_script_fixture(&dir, "still-running", "#!/bin/sh\nsleep 10\n");

        let out = forward_or_spawn_with_exe(&unreachable_daemon_frame(CFG), &exe).await;

        assert!(
            out.is_none(),
            "a live spawned child with no socket remains eligible for non-strict local fallback: {out:?}"
        );
        assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    #[tokio::test]
    #[serial]
    #[serial_test::serial(config_ledger)]
    async fn daemon_round_trip_dispatches_and_enforces_config_id() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let reference = make_test_server();
        let config_id = reference.config_id().to_string();
        let daemon_server = reference.clone();

        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });

        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        // (a) valid same-namespace, same-config op
        let req = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let resp = exchange(&sock, &req).await;
        assert!(resp.ok, "valid op must succeed; error={:?}", resp.error);
        assert!(!resp.namespace_mismatch);
        assert!(!resp.config_mismatch);
        assert!(!resp.version_mismatch);
        assert_eq!(resp.daemon_protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            resp.served_config_id.as_deref(),
            Some(config_id.as_str()),
            "daemon must echo the config it served under"
        );

        let reference_result = reference
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: Some("verbose".to_string()),
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("local dispatch of stats() must succeed");
        let mut daemon_result: serde_json::Value = serde_json::from_str(
            resp.result
                .as_deref()
                .expect("daemon dispatch of stats() must return JSON"),
        )
        .expect("daemon stats response must be valid JSON");
        let mut local_result: serde_json::Value = serde_json::from_str(&reference_result)
            .expect("local stats response must be valid JSON");
        // Usage counters are request-local. The daemon dispatch may count its
        // own audit event before serializing while the direct reference call
        // observes a different event boundary, so they are not an equivalence
        // oracle for the transport round trip exercised by this test.
        for result in [&mut daemon_result, &mut local_result] {
            if let Some(entries) = result
                .get_mut("results")
                .and_then(serde_json::Value::as_array_mut)
            {
                for entry in entries {
                    if let Some(object) = entry.as_object_mut() {
                        object.remove("usage");
                    }
                }
            }
        }
        assert_eq!(daemon_result, local_result);
        assert!(reference_result.contains("\"entities\""));

        // (b) ADR-096 Fork 1: a different namespace, same config_id, is no
        // longer rejected — the daemon accepts and serves the request under
        // the frame's OWN namespace ("other") over the same shared warm
        // registry, instead of setting `namespace_mismatch`.
        let other = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "other".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let resp_other = exchange(&sock, &other).await;
        assert!(
            resp_other.ok,
            "a differently-namespaced frame with a matching config_id must be \
             served, not rejected; error={:?}",
            resp_other.error
        );
        assert!(
            !resp_other.namespace_mismatch,
            "ADR-096 Fork 1 removed the namespace_mismatch reject"
        );
        assert!(!resp_other.config_mismatch);
        assert_eq!(
            resp_other.served_config_id.as_deref(),
            Some(config_id.as_str())
        );

        // (c) same namespace but different config (e.g. a `--pack kg` client
        // hitting the broader daemon) → config_mismatch, no dispatch. The
        // config_id reject stays hard under ADR-096 Fork 1 — only the
        // namespace reject was softened.
        let mismatched_config = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: "packs=[kg];db=:memory:;embed=none;extra=[];backend=main".to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let resp_cfg = exchange(&sock, &mismatched_config).await;
        assert!(
            resp_cfg.config_mismatch,
            "differing config must be rejected"
        );
        assert!(!resp_cfg.namespace_mismatch);
        assert!(!resp_cfg.ok);

        // (d) version mismatch → explicit error, NOT namespace/config mismatch
        let wrong_version = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.clone(),
            protocol_version: 0,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let resp_ver = exchange(&sock, &wrong_version).await;
        assert!(
            resp_ver.version_mismatch,
            "wrong protocol version must set version_mismatch"
        );
        assert!(!resp_ver.ok);
        assert!(
            resp_ver
                .error
                .as_deref()
                .unwrap_or("")
                .contains("protocol mismatch"),
            "version mismatch error must include 'protocol mismatch'; got: {:?}",
            resp_ver.error
        );
        assert!(
            resp_ver
                .error
                .as_deref()
                .unwrap_or("")
                .contains("make local"),
            "version mismatch error must tell operator what to do; got: {:?}",
            resp_ver.error
        );
        assert_eq!(
            resp_ver.daemon_protocol_version, PROTOCOL_VERSION,
            "daemon must echo its own protocol version in the mismatch response"
        );

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
    }

    #[tokio::test]
    #[serial]
    async fn daemon_rejects_client_after_git_write_policy_is_revoked() {
        clear_daemon_env();
        reset_fallback_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let mut allowed_config = memory_runtime_config();
        allowed_config.default_namespace = Namespace::parse("test").unwrap();
        allowed_config.packs = vec!["kg".to_string()];
        allowed_config.git_write = GitWriteSectionConfig {
            allowed: vec![GitWriteEntryConfig {
                repo: "/srv/repos/alpha".to_string(),
                branches: vec!["feat/*".to_string()],
            }],
        };
        let revoked_config = RuntimeConfig {
            git_write: GitWriteSectionConfig::default(),
            ..allowed_config.clone()
        };

        let daemon_server = crate::server::KhiveMcpServer::new(
            KhiveRuntime::new(allowed_config).expect("allowed-policy runtime"),
        )
        .expect("allowed-policy server");
        let daemon_config_id = daemon_server.config_id().to_string();
        let revoked_config_id = crate::server::compute_config_id(&revoked_config, None);
        assert_ne!(
            daemon_config_id, revoked_config_id,
            "revoking the allowlist must change the daemon identity"
        );

        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });
        let ready = connect_when_ready(&sock).await;
        drop(ready);

        let request = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: revoked_config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let response = exchange(&sock, &request).await;

        assert!(response.config_mismatch, "revoked policy must be rejected");
        assert!(!response.ok, "the old-policy daemon must not dispatch");
        assert_eq!(
            response.served_config_id.as_deref(),
            Some(daemon_config_id.as_str())
        );
        assert!(
            map_response(response, &revoked_config_id, "test").is_none(),
            "non-strict clients must take the established config-mismatch fallback path"
        );
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);

        handle.abort();
        let _ = handle.await;
        reset_fallback_counters();
        clear_daemon_env();
    }

    // ── ADR-096 Fork 1: per-request identity over one warm registry ──────────
    //
    // Core capability test: requests carrying DIFFERENT frame namespaces and
    // actors, dispatched against ONE already-running warm daemon, must all be
    // served over the shared backend — and each write must be stamped with its
    // OWN frame's actor and process provenance, never the other request's and
    // never a daemon-baked default. `comm.send` is the
    // vehicle; a chained `comm.thread` reads its persisted properties back.
    #[tokio::test]
    #[serial]
    async fn daemon_serves_per_request_identity_over_one_warm_registry() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::set_var("KHIVE_PROCESS_REF", "daemon/stale-origin");

        // No baked actor_id on the daemon-side server: every actor must come
        // from the per-request frame, never a construction-time default.
        let reference = make_comm_test_server(None);
        let config_id = reference.config_id().to_string();
        let daemon_server = reference.clone();

        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });
        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        let alice_frame = DaemonRequestFrame {
            ops: "comm.send(to=\"bob\", content=\"hello from alice\") | comm.thread(id=$prev.full_id)"
                .to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "alpha".to_string(),
            actor_id: Some("alice".to_string()),
            process_ref: Some("worker/alice:17".to_string()),
            visible_namespaces: Vec::new(),
            config_id: config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let bob_frame = DaemonRequestFrame {
            ops: "comm.send(to=\"alice\", content=\"hello from bob\") | comm.thread(id=$prev.full_id)"
                .to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "beta".to_string(),
            actor_id: Some("bob".to_string()),
            process_ref: Some("worker/bob:29".to_string()),
            visible_namespaces: Vec::new(),
            config_id: config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let charlie_frame = DaemonRequestFrame {
            ops: "comm.send(to=\"alice\", content=\"hello from charlie\") | comm.thread(id=$prev.full_id)"
                .to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "gamma".to_string(),
            actor_id: Some("charlie".to_string()),
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let resp_alice = exchange(&sock, &alice_frame).await;
        assert!(
            resp_alice.ok,
            "alice's request must be served over the shared warm registry; error={:?}",
            resp_alice.error
        );
        assert!(!resp_alice.namespace_mismatch);
        assert!(!resp_alice.config_mismatch);
        let body_alice: serde_json::Value =
            serde_json::from_str(resp_alice.result.as_deref().expect("alice result body"))
                .expect("decode alice result json");
        assert_eq!(
            body_alice["results"][0]["result"]["from"], "alice",
            "write dispatched under alice's frame must stamp actor=alice, got: {body_alice}"
        );
        assert_eq!(
            body_alice["results"][1]["result"]["messages"][0]["properties"]["sent_by_process"],
            "worker/alice:17",
            "the warm daemon must persist alice's frame provenance, not its own environment: \
             {body_alice}"
        );

        let resp_bob = exchange(&sock, &bob_frame).await;
        assert!(
            resp_bob.ok,
            "bob's request must be served over the SAME shared warm registry; error={:?}",
            resp_bob.error
        );
        assert!(!resp_bob.namespace_mismatch);
        assert!(!resp_bob.config_mismatch);
        let body_bob: serde_json::Value =
            serde_json::from_str(resp_bob.result.as_deref().expect("bob result body"))
                .expect("decode bob result json");
        assert_eq!(
            body_bob["results"][0]["result"]["from"], "bob",
            "write dispatched under bob's frame must stamp actor=bob, NOT cross-\
             contaminated with alice's actor; got: {body_bob}"
        );
        assert_eq!(
            body_bob["results"][1]["result"]["messages"][0]["properties"]["sent_by_process"],
            "worker/bob:29",
            "the warm daemon must persist bob's frame provenance independently: {body_bob}"
        );

        let resp_charlie = exchange(&sock, &charlie_frame).await;
        assert!(
            resp_charlie.ok,
            "charlie's request must be served over the SAME shared warm registry; error={:?}",
            resp_charlie.error
        );
        let body_charlie: serde_json::Value =
            serde_json::from_str(resp_charlie.result.as_deref().expect("charlie result body"))
                .expect("decode charlie result json");
        assert_eq!(body_charlie["results"][0]["result"]["from"], "charlie");
        let charlie_properties = body_charlie["results"][1]["result"]["messages"][0]["properties"]
            .as_object()
            .expect("charlie message properties");
        assert!(
            !charlie_properties.contains_key("sent_by_process"),
            "an explicitly absent frame provenance must stay absent rather than falling back to \
             the daemon environment; got: {body_charlie}"
        );

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
    }

    // ADR-096 Fork 1 completion: actor-derived visible namespaces are request
    // identity, not daemon engine identity. Two clients with different
    // configured actors therefore compute the same config_id, but each daemon
    // frame must still read only through its own visible set.
    #[tokio::test]
    #[serial]
    async fn daemon_config_id_ignores_actor_folded_visibility_but_frame_visibility_isolated() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let actor_a = "lambda:actor-a";
        let actor_b = "lambda:actor-b";
        let cfg_a = folded_actor_memory_config(actor_a);
        let cfg_b = folded_actor_memory_config(actor_b);
        let ns_a = Namespace::parse(actor_a).expect("actor a namespace");
        let ns_b = Namespace::parse(actor_b).expect("actor b namespace");

        assert_eq!(cfg_a.actor_id.as_deref(), Some(actor_a));
        assert_eq!(cfg_b.actor_id.as_deref(), Some(actor_b));
        assert!(
            cfg_a.visible_namespaces.contains(&ns_a),
            "actor.id must fold into client A visible_namespaces"
        );
        assert!(
            cfg_b.visible_namespaces.contains(&ns_b),
            "actor.id must fold into client B visible_namespaces"
        );
        assert_ne!(
            cfg_a.visible_namespaces, cfg_b.visible_namespaces,
            "precondition: clients must carry different folded visible sets"
        );

        let id_a = crate::server::compute_config_id(&cfg_a, None);
        let id_b = crate::server::compute_config_id(&cfg_b, None);
        assert_eq!(
            id_a, id_b,
            "actor-derived visible_namespaces must not affect daemon config_id"
        );

        let daemon_server = {
            let runtime = KhiveRuntime::new(memory_runtime_config()).expect("in-memory runtime");
            crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg")
        };
        assert_eq!(
            daemon_server.config_id(),
            id_a,
            "daemon and both clients must share the same engine-coherence key"
        );
        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });
        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        let frame = |ops: &str, actor: &str, visible: &[Namespace]| DaemonRequestFrame {
            ops: ops.to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            namespace: "local".to_string(),
            actor_id: Some(actor.to_string()),
            process_ref: None,
            visible_namespaces: visible.iter().map(|ns| ns.as_str().to_string()).collect(),
            config_id: id_a.clone(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let seed_a = exchange(
            &sock,
            &frame(
                r#"create(kind="concept", name="ActorAVisibleOnly", namespace="lambda:actor-a")"#,
                actor_a,
                &cfg_a.visible_namespaces,
            ),
        )
        .await;
        assert!(seed_a.ok, "seed A must succeed: {:?}", seed_a.error);
        let seed_b = exchange(
            &sock,
            &frame(
                r#"create(kind="concept", name="ActorBVisibleOnly", namespace="lambda:actor-b")"#,
                actor_b,
                &cfg_b.visible_namespaces,
            ),
        )
        .await;
        assert!(seed_b.ok, "seed B must succeed: {:?}", seed_b.error);

        fn names_from_list_response(resp: &DaemonResponseFrame) -> Vec<String> {
            assert!(resp.ok, "list response must be ok: {:?}", resp.error);
            assert!(
                !resp.config_mismatch,
                "list response must not reject on config_id"
            );
            let body: serde_json::Value =
                serde_json::from_str(resp.result.as_deref().expect("list result body"))
                    .expect("decode list result json");
            let first = &body["results"][0];
            assert_eq!(
                first["ok"], true,
                "list op must succeed inside daemon result: {first}"
            );
            let rows = first["result"]["items"]
                .as_array()
                .expect("list result must contain the stable items array");
            rows.iter()
                .filter_map(|row| row.get("name").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        }

        let list_a = exchange(
            &sock,
            &frame(r#"list(kind="entity")"#, actor_a, &cfg_a.visible_namespaces),
        )
        .await;
        let names_a = names_from_list_response(&list_a);
        assert!(
            names_a.iter().any(|name| name == "ActorAVisibleOnly"),
            "actor A frame must see actor A namespace rows; got {names_a:?}"
        );
        assert!(
            !names_a.iter().any(|name| name == "ActorBVisibleOnly"),
            "actor A frame must not see actor B namespace rows; got {names_a:?}"
        );

        let list_b = exchange(
            &sock,
            &frame(r#"list(kind="entity")"#, actor_b, &cfg_b.visible_namespaces),
        )
        .await;
        let names_b = names_from_list_response(&list_b);
        assert!(
            names_b.iter().any(|name| name == "ActorBVisibleOnly"),
            "actor B frame must see actor B namespace rows; got {names_b:?}"
        );
        assert!(
            !names_b.iter().any(|name| name == "ActorAVisibleOnly"),
            "actor B frame must not see actor A namespace rows; got {names_b:?}"
        );

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
    }

    // Back-compat: a caller with NO per-request identity context (pure local
    // dispatch, never touching the daemon socket) must keep using the
    // server's own construction-baked actor_id, unaffected by the identity-
    // override machinery introduced for daemon-forwarded requests.
    #[tokio::test]
    #[serial]
    #[serial_test::serial(config_ledger)]
    async fn local_dispatch_without_identity_context_uses_baked_actor() {
        clear_daemon_env();

        let server = make_comm_test_server(Some("baked-actor"));
        let result = server
            .dispatch_request_local(RequestParams {
                ops: "comm.send(to=\"someone\", content=\"hello\")".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("local dispatch of comm.send must succeed");

        let body: serde_json::Value =
            serde_json::from_str(&result).expect("decode local dispatch result json");
        assert_eq!(
            body["results"][0]["result"]["from"], "baked-actor",
            "local dispatch (no daemon, no identity context) must use the server's \
             own baked actor_id, got: {body}"
        );
    }

    // ── daemon-forward wire-origin gate (security regression) ────────────────
    //
    // The agent-facing MCP `request` tool sets `from_wire=true` on its daemon
    // frame; the daemon must HONOR that bit so the subhandler visibility gate
    // fires after the socket round-trip, not only on the local-fallback path.
    // The local-fallback tests (in tests/integration.rs) run with the daemon
    // disabled and would stay green even if the MCP frame were flipped to
    // `from_wire=false` — which would open an agent-reachable subhandler bypass
    // on every daemon-backed deployment. This pins the round-trip seam.
    #[tokio::test]
    #[serial]
    async fn daemon_round_trip_honors_from_wire_for_subhandlers() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let reference = make_subhandler_test_server();
        let config_id = reference.config_id().to_string();
        let daemon_server = reference.clone();
        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });
        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        let frame = |from_wire: bool| DaemonRequestFrame {
            ops: "brain.state()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "braintest".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire,
            request_id: None,
        };

        // (a) from_wire=true → daemon applies the wire visibility gate:
        // `brain.state` is a Subhandler and must be blocked after the round-trip.
        let resp_wire = exchange(&sock, &frame(true)).await;
        assert!(
            resp_wire.ok,
            "dispatch itself must succeed (the op carries the gate error); error={:?}",
            resp_wire.error
        );
        let body_wire: serde_json::Value =
            serde_json::from_str(resp_wire.result.as_deref().expect("wire result body"))
                .expect("decode wire result json");
        let first_wire = &body_wire["results"][0];
        assert_eq!(
            first_wire["ok"], false,
            "from_wire=true subhandler must be blocked through the daemon: {first_wire}"
        );
        let err_wire = first_wire["error"].as_str().unwrap_or("");
        assert!(
            err_wire.contains("permission denied") || err_wire.contains("subhandler"),
            "daemon-forward wire path must surface the subhandler gate error; got: {err_wire}"
        );

        // (b) from_wire=false (operator frame, e.g. `kkernel exec`) → the same
        // Subhandler verb must be REACHED, not gated.
        let resp_op = exchange(&sock, &frame(false)).await;
        let body_op: serde_json::Value =
            serde_json::from_str(resp_op.result.as_deref().expect("operator result body"))
                .expect("decode operator result json");
        let first_op = &body_op["results"][0];
        let err_op = first_op["error"].as_str().unwrap_or("");
        assert!(
            !err_op.contains("permission denied") && !err_op.contains("subhandler"),
            "operator frame must NOT gate the subhandler through the daemon: {first_op}"
        );

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
    }

    // The agent-facing `request` tool must stamp `from_wire=true` on its daemon
    // forward-frame. Without this, a daemon-backed MCP request would dispatch
    // with `from_wire=false` and silently reopen the agent subhandler bypass
    // (review #369 Medium). Pinned at the frame-builder seam so the daemon
    // round-trip test above (which proves the daemon HONORS the bit) is paired
    // with proof that the tool SETS it.
    #[test]
    fn wire_request_frame_sets_from_wire_true() {
        let server = make_subhandler_test_server();
        let params = RequestParams {
            ops: "brain.state()".to_string(),
            ..Default::default()
        };
        let frame = server.wire_daemon_frame(&params);
        assert!(
            frame.from_wire,
            "request tool must set from_wire=true on the daemon forward-frame"
        );
        assert_eq!(frame.ops, "brain.state()");
        assert_eq!(frame.namespace, "braintest");
    }

    // ── new-client + old-daemon regression (fix for #98 BLOCKER) ─────────────
    //
    // Simulates a pre-versioning daemon that:
    //   • Ignores the unknown `protocol_version` request field (it deserializes
    //     as missing and is silently dropped).
    //   • Returns a decodable response with a matching `served_config_id` but
    //     WITHOUT `daemon_protocol_version` or `version_mismatch` (they default
    //     to `0` / `false` on the client).
    //
    // Before the fix, `map_response` accepted this response because `version_mismatch`
    // was false and `served_config_id` matched — the stale daemon was trusted.
    //
    // After the fix, `try_forward_inner` detects `daemon_protocol_version == 0 != 1`
    // and returns `ForwardOutcome::ProtocolMismatch`, which `forward_or_spawn`
    // rejects without retry, local dispatch, kill, or respawn.
    //
    // The test verifies at the `forward_or_spawn` level (not just `map_response`)
    // that:
    //   1. The stale response is NOT accepted.
    //   2. No second connection or recovery attempt is made.
    //   3. A clear "protocol mismatch" error is returned.

    /// Minimal "old daemon" frame: has `served_config_id` but omits the
    /// protocol version fields (they default to `false`/`0` on the client side).
    fn old_daemon_response(config_id: &str) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: true,
            result: Some("stale-result".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(config_id.to_string()),
            // Pre-versioning daemon would never set these:
            version_mismatch: false,
            daemon_protocol_version: 0,
            metrics: None,
            request_id: None,
        }
    }

    /// Serve exactly one connection with `response`, then stop accepting.
    async fn serve_one_response(listener: tokio::net::UnixListener, response: DaemonResponseFrame) {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Read the inbound request frame (and discard it — old daemon ignores
            // unknown fields, which is the scenario we're simulating).
            if read_frame(&mut stream).await.is_ok() {
                if let Ok(payload) = serde_json::to_vec(&response) {
                    let _ = write_frame(&mut stream, &payload).await;
                }
            }
        }
        // Listener drops here; subsequent connection attempts see "connection refused".
    }

    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_rejects_old_daemon_and_returns_protocol_mismatch_error() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        // Bind the fake old-daemon socket BEFORE starting the client.
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake old-daemon socket");
        // A PID file makes the fake daemon's rendezvous realistic. The terminal
        // mismatch path must leave it untouched.
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

        let old_resp = old_daemon_response(config_id);
        // Serve exactly one exchange, then let the listener drop (no second connection
        // will be served — any second connection would therefore expose an
        // incorrect retry as `NoSocket` rather than hiding it in the fixture.
        let fake_handle = tokio::spawn(serve_one_response(listener, old_resp));

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let result = forward_or_spawn(&frame).await;

        // The fake socket served exactly one old-protocol response.  The fake handle
        // should have completed by now; join it to catch any panics.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

        // Must NOT accept the stale daemon result.
        match result {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("protocol mismatch"),
                    "error must name 'protocol mismatch'; got: {message}"
                );
                assert!(
                    message.contains("make local") || message.contains("rebuild"),
                    "error must tell the operator what to do; got: {message}"
                );
            }
            Some(Ok(v)) => {
                panic!("forward_or_spawn must NOT accept old-daemon response; got Ok({v:?})")
            }
            None => panic!(
                "forward_or_spawn must return Some(Err(..)) for protocol mismatch, \
                 not None (which would cause silent fallback to local dispatch)"
            ),
        }

        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── daemon crash mid-dispatch regression (#91) ────────────────────────────
    //
    // When the daemon crashes (or panics) during dispatch it closes the
    // connection without writing a response frame. Before the fix, `try_forward_inner`
    // returned `ForwardOutcome::NoSocket` on the `read_frame` error, causing
    // `forward_or_spawn` to return `None` — a silent fallback to local dispatch.
    //
    // The fix promotes the `read_frame` error to `ForwardOutcome::ParseFailure`.
    // Since the real frame was fully written, `forward_or_spawn` turns that
    // classification into a terminal ambiguity error and performs no recovery.
    //
    // This test binds a fake socket that reads the request but immediately drops
    // the connection without writing a response (simulating a crash during
    // dispatch). This focused test validates the exact `ForwardOutcome`
    // discriminant; the parallel test validates the terminal call-site behavior
    // and zero lifecycle actions.

    /// Serve one connection: read the request frame, then drop the stream
    /// without writing any response (simulating a daemon crash mid-dispatch).
    async fn serve_crash_on_dispatch(listener: tokio::net::UnixListener) {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Read the inbound request (and discard it — the "crash" happens here).
            let _ = read_frame(&mut stream).await;
            // Drop stream without writing a response — connection resets.
        }
        // Listener drops; subsequent connection attempts see "connection refused".
    }

    #[tokio::test]
    #[serial]
    #[serial_test::serial(config_ledger)]
    async fn try_forward_inner_returns_parse_failure_when_daemon_closes_without_response() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        let listener =
            tokio::net::UnixListener::bind(&sock).expect("bind fake crash-daemon socket");
        // A PID file makes the fake daemon's rendezvous realistic; this focused
        // classifier test must not touch it.
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

        // Serve one connection that crashes without replying.
        let fake_handle = tokio::spawn(serve_crash_on_dispatch(listener));

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        // Call try_forward_inner directly to assert the discriminant.
        let outcome = try_forward_inner(&frame).await;

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

        assert!(
            matches!(outcome, ForwardOutcome::ParseFailure),
            "daemon crash (connection closed without response) must yield \
             ParseFailure, not NoSocket — got a different variant"
        );

        clear_daemon_env();
    }

    // ── argv_is_khive_daemon unit tests ───────────────────────────────────────

    #[test]
    fn argv_daemon_true_bare() {
        // Exact daemon argv as spawned by spawn_daemon().
        assert!(argv_is_khive_daemon("kkernel mcp --daemon"));
    }

    #[test]
    fn argv_daemon_true_absolute_path() {
        // Absolute path to kkernel binary — basename must match.
        assert!(argv_is_khive_daemon(
            "/Users/x/.cargo/bin/kkernel mcp --daemon"
        ));
    }

    #[test]
    fn argv_daemon_false_editor_with_kkernel_in_filename() {
        // Editor opened on a file whose name contains kkernel — not a daemon.
        assert!(!argv_is_khive_daemon("vim kkernel-notes.md"));
    }

    #[test]
    fn argv_daemon_false_less_with_kkernel_path() {
        // less paging a kkernel source file — argv[0] is "less", not "kkernel".
        assert!(!argv_is_khive_daemon(
            "less /Users/x/projects/kkernel/daemon.rs"
        ));
    }

    #[test]
    fn argv_daemon_false_kkernel_no_daemon_flag() {
        // kkernel exec subcommand — has kkernel basename but no --daemon token.
        assert!(!argv_is_khive_daemon("kkernel exec 'something'"));
    }

    #[test]
    fn argv_daemon_false_wrapper_argv0_not_kkernel() {
        // A wrapper script passes kkernel mcp --daemon as args but its own
        // argv[0] is "some-wrapper" — basename check must reject it.
        assert!(!argv_is_khive_daemon("some-wrapper kkernel mcp --daemon"));
    }

    #[test]
    fn argv_daemon_false_empty_string() {
        assert!(!argv_is_khive_daemon(""));
    }

    #[test]
    fn argv_daemon_true_with_surrounding_and_inner_whitespace() {
        assert!(argv_is_khive_daemon(
            "  /Users/x/.cargo/bin/kkernel   mcp    --daemon  "
        ));
    }

    #[test]
    fn argv_daemon_true_kkernel_bench_basename() {
        // bench binary copy is named "kkernel-bench"; pid management must treat it
        // as a valid daemon so stale bench daemons are SIGTERM'd on respawn.
        assert!(argv_is_khive_daemon("kkernel-bench mcp --daemon"));
        assert!(argv_is_khive_daemon(
            "/Users/x/.cargo/bin/kkernel-bench mcp --daemon"
        ));
    }

    #[tokio::test]
    #[serial]
    async fn recovery_requires_incumbent_exit_before_spawning() {
        let mut cleanup = RecoveryTestGuard::new();
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("khived.pid");
        let ready_file = dir.path().join("incumbent.ready");

        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::set_var(
            "KHIVE_RECOVERER_LOCK",
            dir.path().join("khived.recoverer.lock"),
        );
        std::env::remove_var("KHIVE_NO_DAEMON");

        let incumbent = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; : > \"$1\"; while :; do sleep 1; done")
            .arg("stubborn-incumbent")
            .arg(&ready_file)
            .spawn()
            .expect("spawn signal-resistant incumbent");
        let incumbent_pid = cleanup.track_child(incumbent);
        let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !ready_file.exists() {
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "incumbent did not install its signal handler"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        std::fs::write(&pid_file, incumbent_pid.to_string()).expect("write incumbent pid file");
        FORCE_PID_IS_DAEMON.store(true, std::sync::atomic::Ordering::SeqCst);

        let spawn_calls = std::sync::atomic::AtomicUsize::new(0);
        let spawn = || {
            spawn_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
        };
        let outcome = kill_and_respawn_with_exit_timeout(
            CFG,
            NS,
            &spawn,
            std::time::Duration::from_millis(100),
        )
        .await;
        let refused_pid = match outcome {
            Err(RecoveryError::IncumbentStillAlive { pid }) => Some(pid),
            Ok(RecoveryOutcome::Spawned(mut child)) => {
                let _ = child.wait();
                None
            }
            Ok(RecoveryOutcome::Skipped | RecoveryOutcome::Uncertain)
            | Err(RecoveryError::Spawn(_)) => None,
        };
        let live_pid_file_preserved = pid_file.exists();
        cleanup.kill_and_reap_child();

        assert_eq!(refused_pid, Some(incumbent_pid));
        assert_eq!(
            spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a replacement must not spawn while the signalled incumbent PID is still alive"
        );
        assert!(
            live_pid_file_preserved,
            "the live incumbent's PID file must remain in place after refusal"
        );
        let refusal = incumbent_still_alive_error(incumbent_pid);
        assert!(refusal.message.contains(&format!("PID {incumbent_pid}")));
        let data = refusal.data.expect("live-incumbent refusal data");
        assert_eq!(data["reason"], "incumbent_still_alive");
        assert_eq!(data["pid"], incumbent_pid);

        let recovered = kill_and_respawn_with_exit_timeout(
            CFG,
            NS,
            &spawn,
            std::time::Duration::from_millis(100),
        )
        .await;
        let spawned = match recovered {
            Ok(RecoveryOutcome::Spawned(mut child)) => {
                let _ = child.wait();
                true
            }
            _ => false,
        };

        FORCE_PID_IS_DAEMON.store(false, std::sync::atomic::Ordering::SeqCst);
        clear_daemon_env();
        assert!(spawned, "a confirmed-dead incumbent must still be replaced");
        assert_eq!(
            spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "confirmed-dead recovery must spawn exactly one replacement"
        );
        assert!(
            !pid_file.exists(),
            "the confirmed-dead incumbent PID file must be removed before spawning"
        );
    }

    #[tokio::test]
    #[serial]
    async fn recovery_replaces_stale_pid_without_waiting_on_live_foreign_process() {
        let mut cleanup = RecoveryTestGuard::new();
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("khived.pid");

        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::set_var(
            "KHIVE_RECOVERER_LOCK",
            dir.path().join("khived.recoverer.lock"),
        );
        std::env::remove_var("KHIVE_NO_DAEMON");

        let foreign = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn live foreign process");
        let foreign_pid = cleanup.track_child(foreign);
        std::fs::write(&pid_file, foreign_pid.to_string()).expect("write foreign pid file");
        // Process inspection may be restricted in test environments, so force
        // the classification while retaining a live child to prove it is not killed.
        FORCE_PID_IS_FOREIGN.store(true, std::sync::atomic::Ordering::SeqCst);

        let spawn_calls = std::sync::atomic::AtomicUsize::new(0);
        let spawn = || {
            spawn_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
        };

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            kill_and_respawn(CFG, NS, &spawn),
        )
        .await
        .expect("recovery stalled on a PID positively identified as foreign")
        .expect("foreign-PID recovery failed");
        match outcome {
            RecoveryOutcome::Spawned(mut child) => {
                child.wait().expect("reap replacement fixture");
            }
            RecoveryOutcome::Skipped | RecoveryOutcome::Uncertain => {
                panic!("foreign-PID recovery did not spawn a replacement")
            }
        }

        assert_eq!(
            spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "foreign-PID recovery must spawn exactly one replacement"
        );
        assert!(
            !pid_file.exists(),
            "the stale foreign PID file must be removed before spawning"
        );
        assert!(
            cleanup
                .child_mut()
                .try_wait()
                .expect("query foreign child state")
                .is_none(),
            "recovery must not signal the live foreign process"
        );
    }

    // ── concurrent recovery — second client skips kill+spawn when daemon alive ──
    //
    // Exercises the recheck-under-lock (double-checked locking) in
    // kill_and_respawn.  Scenario:
    //
    //   1. A real daemon is running (via run_daemon).
    //   2. A recovering client calls kill_and_respawn directly — simulating a
    //      client that observed pre-write `NoSocket`, but a concurrent first
    //      recoverer has ALREADY
    //      spawned a healthy daemon before this client reached the lock.
    //   3. Under the lock, kill_and_respawn sends a probe_only frame and finds a
    //      responsive, identity-matching daemon → returns RecoveryOutcome::Skipped.
    //   4. KILL_COUNT must be 0 and SPAWN_COUNT must be 0.
    //   5. The fresh daemon's PID file and socket must survive intact.
    //
    // FORCE_PID_IS_DAEMON=true makes every live PID SIGTERM-eligible so that
    // if the bounded-probe is removed (reverted) kill_stale_daemon_inner would
    // attempt SIGTERM against the real daemon PID, KILL_COUNT would be 1, and
    // the assertion below would catch the regression.
    //
    // Fail-if-reverted assertion: the `assert_eq!(KILL_COUNT, 0)` below catches
    // a reverted probe because without it, kill_stale_daemon_inner is called
    // unconditionally and KILL_COUNT increments to 1.

    #[tokio::test]
    #[serial]
    async fn concurrent_recovery_second_client_skips_kill_when_daemon_alive() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        // Run a real daemon so probe_daemon_identity in kill_and_respawn finds a
        // live, responsive, identity-matching daemon under the lock.
        let server = make_test_server();
        let config_id = server.config_id().to_string();
        let daemon_server = server.clone();
        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });

        // Wait for the daemon to bind the socket and write its PID file.
        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        // Record the daemon PID as written by run_daemon.
        let daemon_pid_str =
            std::fs::read_to_string(&pid_file).expect("daemon must have written a pid file");
        let daemon_pid: u32 = daemon_pid_str
            .trim()
            .parse()
            .expect("daemon pid file must contain a u32");

        // Arm the SIGTERM-eligible hook: classify_pid_identity() will now identify
        // the live daemon PID as khive. Without the bounded-probe, a reverted
        // kill_and_respawn would send SIGTERM to that PID and unlink the socket —
        // KILL_COUNT catches both paths.
        FORCE_PID_IS_DAEMON.store(true, std::sync::atomic::Ordering::SeqCst);
        reset_counters();

        // Call kill_and_respawn directly — simulates a second recovering client
        // whose turn arrives after the first recoverer already replaced the stale
        // daemon.  The bounded probe confirms the live daemon; Skipped is returned
        // without killing.
        let outcome = kill_and_respawn(&config_id, "test", &spawn_daemon).await;

        assert!(
            matches!(outcome, Ok(RecoveryOutcome::Skipped)),
            "kill_and_respawn must return Ok(RecoveryOutcome::Skipped) when a live \
             matching daemon exists under the lock"
        );
        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "KILL_COUNT must be 0: the probe found the daemon alive so \
             kill_stale_daemon_inner must NOT be called \
             (this assertion fails if the probe-under-lock is removed)"
        );
        assert_eq!(
            SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "SPAWN_COUNT must be 0: no respawn needed when the daemon is alive \
             (this assertion fails if the probe-under-lock is removed)"
        );

        // The daemon's PID file and socket must be intact.
        assert!(
            pid_file.exists(),
            "PID file must survive: kill_and_respawn must NOT unlink it"
        );
        assert!(
            sock.exists(),
            "socket must survive: kill_and_respawn must NOT unlink it"
        );
        let surviving_pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("pid file readable")
            .trim()
            .parse()
            .expect("pid file is a u32");
        assert_eq!(
            surviving_pid, daemon_pid,
            "PID in file must be the original daemon PID — no new daemon was spawned"
        );

        handle.abort();
        let _ = handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── oversized daemon response does not trigger kill/respawn ───────────────
    //
    // When the daemon's serialized response exceeds MAX_FRAME_BYTES the server
    // sends a small explicit error frame (ok=false, "response too large") instead
    // of closing the connection without a response.  The client receives this as
    // ForwardOutcome::Response (decodable frame) → map_response → Some(Err(..));
    // NOT the less-actionable ambiguous-forward ParseFailure error.
    //
    // This test drives the REAL handle_conn server path via run_daemon with a
    // BigDispatch that returns a string larger than MAX_FRAME_BYTES.  The client
    // calls forward_or_spawn and must receive Some(Err(..)) containing "too large".
    // The daemon's PID file and socket must survive the call.
    //
    // Fail-if-reverted: if the handle_conn oversized gate (the `if payload.len()
    // > MAX_FRAME_BYTES` branch that sends the small error frame) is removed,
    // handle_conn falls through to write_frame with the oversized payload, which
    // write_frame REJECTS (its own guard returns Err), causing the connection to
    // close without a response → the client sees the generic terminal
    // ambiguous-forward error instead of the precise "response too large"
    // refusal asserted below.

    /// A minimal DaemonDispatch that returns a payload larger than MAX_FRAME_BYTES
    /// so handle_conn's oversized guard fires and emits the "response too large"
    /// error frame.
    #[derive(Clone)]
    struct BigDispatch {
        namespace: String,
        config_id: String,
    }

    #[async_trait]
    impl daemon::DaemonDispatch for BigDispatch {
        async fn dispatch(
            &self,
            _ops: String,
            _presentation: Option<String>,
            _presentation_per_op: Option<Vec<Option<String>>>,
            _format: Option<String>,
            _format_per_op: Option<Vec<Option<String>>>,
            _from_wire: bool,
            _identity: Option<khive_runtime::RequestIdentity>,
        ) -> Result<String, String> {
            // Return a string whose serialized DaemonResponseFrame JSON length
            // exceeds MAX_FRAME_BYTES.  The frame JSON overhead is ~200 bytes so
            // a result of this size is comfortably over the cap.
            Ok("X".repeat(khive_runtime::daemon::MAX_FRAME_BYTES + 1))
        }

        async fn warm_all(&self) {}

        fn namespace(&self) -> &str {
            &self.namespace
        }

        fn config_id(&self) -> &str {
            &self.config_id
        }
    }

    #[tokio::test]
    #[serial]
    async fn oversized_daemon_response_sends_error_frame_not_kills_daemon() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "test-oversized-config";
        let dispatcher = BigDispatch {
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
        };

        // Run the real daemon server (with handle_conn's oversized gate live).
        let handle = tokio::spawn(async move {
            let _ = run_daemon(dispatcher).await;
        });

        // Wait for the daemon to bind and write its PID.
        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        let daemon_pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("daemon must have written a pid file")
            .trim()
            .parse()
            .expect("daemon pid must be a u32");

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let result = forward_or_spawn(&frame).await;

        // Primary assertion: the daemon's PID file and socket must be intact
        // (no kill/respawn triggered).
        assert!(
            pid_file.exists(),
            "PID file must survive: oversized response is NOT a daemon crash"
        );
        assert!(
            sock.exists(),
            "socket must survive: oversized response is NOT a daemon crash"
        );
        let surviving_pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("pid file readable")
            .trim()
            .parse()
            .expect("pid file is a u32");
        assert_eq!(
            surviving_pid, daemon_pid,
            "daemon PID must not change — no kill+respawn occurred"
        );
        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "KILL_COUNT must be 0 — oversized response must NOT trigger \
             kill_stale_daemon_inner (fails if handle_conn oversized gate is removed)"
        );

        // The result must be Some(Err(..)) containing "too large" — the explicit
        // error frame the server sends when the real response is oversized.
        match result {
            Some(Err(e)) => {
                assert!(
                    e.message.contains("too large"),
                    "error must describe the oversized response; got: {}",
                    e.message
                );
            }
            Some(Ok(_)) => panic!("oversized response must not produce Ok result"),
            None => panic!(
                "oversized response must produce Some(Err(..)) from the explicit \
                 error frame, not None (None would mean map_response fell back to \
                 local dispatch, hiding the server-side error)"
            ),
        }

        handle.abort();
        let _ = handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── probe-only recovery primitive never dispatches a real request ─────────
    //
    // Scenario:
    //   1. Recovery is invoked directly after an independently established
    //      pre-write liveness failure (`NoSocket` is the only production caller).
    //   2. Under the lock, probe_daemon_identity sends a probe_only frame to a
    //      REAL CountingDispatch daemon (already running).  The daemon's
    //      handle_conn returns an identity frame without calling dispatch() —
    //      DAEMON_DISPATCH stays 0.
    //   3. kill_and_respawn returns RecoveryOutcome::Skipped (live daemon found).
    //   4. The call site forwards the REAL request once via try_forward_inner.
    //      CountingDispatch.dispatch() is called → DAEMON_DISPATCH == 1.
    //
    // Fail-if-reverted: if kill_and_respawn is reverted to use the real frame as
    // the probe (try_forward_inner(frame) under the lock), CountingDispatch.dispatch()
    // is called for the probe (DAEMON_DISPATCH == 1), and again at the call site
    // (DAEMON_DISPATCH == 2).  The assert_eq!(DAEMON_DISPATCH, 1) then fails.
    //
    // This intentionally exercises the recovery primitive directly. A
    // post-write ParseFailure cannot reach it after #644; the parallel terminal
    // test below guards that call-site boundary separately.

    /// A minimal DaemonDispatch that increments DAEMON_DISPATCH on every real
    /// (non-probe) dispatch.  probe_only frames never reach dispatch() — they
    /// are short-circuited by handle_conn before calling the dispatcher.
    #[derive(Clone)]
    struct CountingDispatch {
        namespace: String,
        config_id: String,
    }

    #[async_trait]
    impl daemon::DaemonDispatch for CountingDispatch {
        async fn dispatch(
            &self,
            _ops: String,
            _presentation: Option<String>,
            _presentation_per_op: Option<Vec<Option<String>>>,
            _format: Option<String>,
            _format_per_op: Option<Vec<Option<String>>>,
            _from_wire: bool,
            _identity: Option<khive_runtime::RequestIdentity>,
        ) -> Result<String, String> {
            DAEMON_DISPATCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("{\"ok\":true,\"counted\":true}".to_string())
        }

        async fn warm_all(&self) {}

        fn namespace(&self) -> &str {
            &self.namespace
        }

        fn config_id(&self) -> &str {
            &self.config_id
        }
    }

    #[tokio::test]
    #[serial]
    #[serial_test::serial(config_ledger)]
    async fn recovery_path_dispatches_real_request_exactly_once() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let real_sock = dir.path().join("khived.sock");
        let stale_sock = dir.path().join("stale.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        // Start a real CountingDispatch daemon on `real_sock`.
        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
        let counting_dispatcher = CountingDispatch {
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
        };
        // Temporarily point KHIVE_SOCKET at real_sock to let run_daemon bind there.
        std::env::set_var("KHIVE_SOCKET", &real_sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let counting_handle = tokio::spawn(async move {
            let _ = run_daemon(counting_dispatcher).await;
        });
        let _ready = connect_when_ready(&real_sock).await;
        drop(_ready);

        // Bind the stale fake socket on `stale_sock` BEFORE redirecting the client
        // env so the client sees it first.  The fake stale socket reads one frame
        // then drops without responding — simulating a crashed old daemon.
        let stale_listener =
            tokio::net::UnixListener::bind(&stale_sock).expect("bind stale socket");
        let stale_handle = tokio::spawn(serve_crash_on_dispatch(stale_listener));

        // The stale fixture documents the historical failure shape, while the
        // actual recovery assertion below starts from the live daemon path. A
        // real post-write ParseFailure no longer enters recovery.
        let stale_pid_file = dir.path().join("stale.pid");
        std::fs::write(&stale_pid_file, std::process::id().to_string()).expect("write stale pid");
        std::env::set_var("KHIVE_SOCKET", &stale_sock);
        std::env::set_var("KHIVE_PID", &stale_pid_file);

        // After kill_and_respawn's probe, the probe needs to reach the REAL daemon.
        // We achieve this by having the probe use `real_sock` (via KHIVE_SOCKET).
        // Redirect KHIVE_SOCKET back to the real daemon socket AFTER the stale
        // socket has been read (but we need the probe to already know where to look).
        //
        // Simpler approach: use a single socket for both — the stale response is
        // from a `serve_crash_on_dispatch` (closes after one read), the NEXT
        // connection attempt (the probe) goes to the same path, but the stale
        // listener is now gone.  Instead we use real_sock for the probe by
        // redirecting KHIVE_SOCKET before the probe fires.
        //
        // To get the "Skipped" (live daemon under lock) scenario without a real
        // spawn: call kill_and_respawn directly with KHIVE_SOCKET pointing at the
        // live CountingDispatch daemon, and separately assert DAEMON_DISPATCH from
        // a direct try_forward_inner call.

        // Point back at the real daemon for the probe and the real forward.
        std::env::set_var("KHIVE_SOCKET", &real_sock);
        std::env::set_var("KHIVE_PID", &pid_file);

        // Simulate the exactly-once scenario:
        //   (a) kill_and_respawn sees a live daemon → returns Skipped (0 dispatches)
        //   (b) call site forwards the real request once → 1 dispatch
        let recovery = kill_and_respawn(config_id, "test", &spawn_daemon).await;
        assert!(
            matches!(recovery, Ok(RecoveryOutcome::Skipped)),
            "probe must find the live CountingDispatch daemon and return Skipped"
        );
        // DAEMON_DISPATCH must still be 0: the probe_only frame does not reach dispatch().
        assert_eq!(
            DAEMON_DISPATCH.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "probe_only frame must NOT increment DAEMON_DISPATCH \
             (fails if the real request is used as the probe)"
        );

        // Now forward the real request exactly once — the call site's single forward.
        let real_frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let fwd = try_forward_inner(&real_frame).await;
        assert!(
            matches!(fwd, ForwardOutcome::Response(_)),
            "real forward after Skipped must succeed; got non-Response outcome"
        );

        // DAEMON_DISPATCH must now be exactly 1.
        assert_eq!(
            DAEMON_DISPATCH.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "real request must be dispatched EXACTLY ONCE across the recovery path \
             (assert fails with count==2 if the real frame is used as the probe \
              AND re-forwarded at the call site — the double-dispatch bug)"
        );

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stale_handle).await;
        counting_handle.abort();
        let _ = counting_handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    /// Serve every connection with a valid probe-ack response identity-matching
    /// `config_id`, forever (until the listener is dropped/aborted). Simulates
    /// an already-bound, healthy daemon that answers `probe_only` frames
    /// immediately.
    async fn serve_probe_ack_forever(listener: tokio::net::UnixListener, config_id: String) {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            if read_frame(&mut stream).await.is_err() {
                continue;
            }
            let response = DaemonResponseFrame {
                ok: true,
                result: None,
                error: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some(config_id.clone()),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: None,
            };
            if let Ok(payload) = serde_json::to_vec(&response) {
                let _ = write_frame(&mut stream, &payload).await;
            }
        }
    }

    // ── #758: confirm_genuinely_dead must not trust a bare Dead reading while
    // a peer's boot is in flight ───────────────────────────────────────────
    //
    // Regression for the daemon-recovery double-spawn window: `spawn_daemon()`
    // is fire-and-forget, so a concurrent recoverer's identity probe can
    // observe `Dead` in the gap between a peer's `cmd.spawn()` returning and
    // that child reaching its own `acquire_daemon_boot_guard()` call. This
    // test drives that gap directly: a background OS thread holds the real
    // boot/recovery lock for `GUARD_HOLD` (simulating "a peer's child is
    // mid cold-boot, holding the same lock this process would need to
    // classify it"), while a fake, already-bound, identity-matching listener
    // answers probe_only frames the instant it is asked (simulating "the
    // peer's child has already bound its socket and would answer this
    // instant, if only the classifier would wait for the lock instead of
    // trusting an immediate Dead reading").
    //
    // Fail-if-reverted: without the fix, `confirm_genuinely_dead` would not
    // exist and `kill_and_respawn` would trust the bare `Dead` result
    // immediately — this test exercises the new function directly, so
    // reverting it is a compile error. The regression oracle is the `timeout`
    // window below: it asserts `confirm_genuinely_dead` does NOT resolve
    // while the peer explicitly still holds the lock, then asserts it DOES
    // resolve (as `Alive`) once the peer explicitly releases it — real
    // two-way synchronization via channels, not a fixed sleep + elapsed-time
    // assertion (#838: the previous version held the guard
    // via `std::thread::sleep` and asserted `elapsed >= GUARD_HOLD`, which is
    // timing-dependent under load).
    #[tokio::test]
    #[serial]
    async fn confirm_genuinely_dead_waits_for_peer_to_release_boot_guard() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let server = make_test_server();
        let config_id = server.config_id().to_string();

        // A fake daemon is already reachable from T=0 — proving the wait
        // below is caused by the contended lock, not by the daemon being
        // slow to bind.
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake daemon socket");
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write fake pid file");
        let serve_handle = tokio::spawn(serve_probe_ack_forever(listener, config_id.clone()));

        // Real two-way synchronization: the boot-holder thread signals once it
        // has genuinely acquired the lock (so the test never proceeds before
        // contention is real), then blocks on an explicit release channel
        // instead of a fixed sleep — the test controls exactly when the lock
        // becomes available, with no timing guess involved.
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let boot_thread = std::thread::spawn(move || {
            let guard = khive_runtime::daemon::acquire_daemon_boot_guard()
                .expect("test boot holder must acquire the recovery lock");
            acquired_tx.send(()).expect("signal lock acquired");
            let _ = release_rx.recv();
            drop(guard);
        });
        acquired_rx
            .recv()
            .expect("boot-holder thread must signal after acquiring the lock");

        let confirm_fut = confirm_genuinely_dead(&config_id, "test");
        tokio::pin!(confirm_fut);

        // Bounded assertion window (NOT the release mechanism — the lock is
        // released explicitly below via `release_tx`): proves
        // confirm_genuinely_dead does not resolve while the peer still holds
        // the lock.
        let too_early =
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut confirm_fut).await;
        assert!(
            too_early.is_err(),
            "confirm_genuinely_dead must not resolve while a peer holds the \
             boot/recovery lock"
        );

        release_tx
            .send(())
            .expect("boot-holder thread still awaiting release");
        boot_thread
            .join()
            .expect("boot-holder thread must not panic");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), confirm_fut)
            .await
            .expect("confirm_genuinely_dead must resolve promptly once the peer releases the lock");

        assert!(
            matches!(outcome, ProbeOutcome::Alive),
            "confirm_genuinely_dead must observe the already-reachable daemon \
             once the contended lock clears, not conclude Dead early"
        );

        serve_handle.abort();
        let _ = serve_handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── #838: an earlier LockContended round must not be
    // erased by a later Dead round ──────────────────────────────────────────
    //
    // `confirm_genuinely_dead` only trusts `Dead` once EVERY round agrees the
    // daemon is absent. Before this fix, the aggregation tracked only the
    // LAST round's outcome: a LockContended round followed by a later Dead
    // round overwrote the earlier contention and the whole call returned
    // `Dead`, which `kill_and_respawn` trusts enough to kill+spawn — even
    // though quiescence was never actually established across every round.
    //
    // This test drives that exact sequence directly: a background thread
    // holds the real boot/recovery lock for longer than a single round's
    // bounded wait (`BOOT_QUIESCENCE_LOCK_TIMEOUT_MS` = 500ms), guaranteeing
    // round 1 cannot acquire it and returns `LockContended`, then releases
    // the lock before `confirm_genuinely_dead` returns — so the remaining
    // rounds observe the (genuinely absent) daemon and return `Dead`.
    //
    // Fail-if-reverted: with the old last-round-wins aggregation, this
    // LockContended-then-Dead sequence resolves to `ProbeOutcome::Dead`, and
    // the assertion below fails.
    #[tokio::test]
    #[serial]
    async fn confirm_genuinely_dead_is_sticky_uncertain_after_earlier_contention() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        // Genuinely no daemon at all: no socket, no pid file. Once the lock
        // is free, every round's identity probe observes Dead.
        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main".to_string();

        // Hold the real boot/recovery lock from before `confirm_genuinely_dead`
        // starts, for strictly longer than one round's bounded wait, so round
        // 1 is deterministically unable to acquire it (LockContended) — not a
        // probabilistic race, since the hold spans the round's entire
        // deadline window.
        let guard = khive_runtime::daemon::acquire_daemon_boot_guard()
            .expect("test lock holder must acquire the recovery lock");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let boot_thread = std::thread::spawn(move || {
            let _ = release_rx.recv();
            drop(guard);
        });

        let confirm_config_id = config_id.clone();
        let confirm_handle =
            tokio::spawn(async move { confirm_genuinely_dead(&confirm_config_id, "test").await });

        // Round 1's bounded wait is 500ms; sleeping past it while still
        // holding the lock guarantees round 1 observed LockContended before
        // release. `confirm_handle` runs concurrently on the runtime during
        // this sleep (unlike a merely-pinned, never-polled future), so round
        // 1 genuinely contends against the held lock here.
        tokio::time::sleep(std::time::Duration::from_millis(650)).await;
        release_tx
            .send(())
            .expect("boot-holder thread still awaiting release");
        boot_thread
            .join()
            .expect("boot-holder thread must not panic");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), confirm_handle)
            .await
            .expect("confirm_genuinely_dead must resolve once the lock is released")
            .expect("confirm_genuinely_dead task must not panic");

        assert!(
            matches!(outcome, ProbeOutcome::LockContended),
            "an earlier LockContended round must make the whole call \
             LockContended (sticky), never overwritten by a later round's \
             Dead reading; got {outcome:?}"
        );
        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "confirm_genuinely_dead must never kill on its own"
        );
        assert_eq!(
            SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "confirm_genuinely_dead must never spawn on its own"
        );

        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── #539/#544: real parallel recovery and terminal post-write failure ───

    /// Eight clients independently observe `NoSocket`, rendezvous before the
    /// recoverer lock, and launch the real in-process daemon server. The oracle
    /// is one live, responsive owner after quiescence — not an exact spawn-call
    /// count, because a launch can legitimately lose the server-side ownership
    /// fence before it binds.
    ///
    /// Deliberately NOT `launched_count() == 1`: racing launches are tolerated
    /// by design. The recoverer lock is best-effort serialization (it shrinks
    /// the raced-launch window; it is not the correctness mechanism), and the
    /// server-side boot fence alone guarantees convergence — so removing the
    /// lock would degrade this test to more raced launches without violating
    /// its oracle, and that is the intended contract, not a coverage gap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 12)]
    #[serial]
    async fn parallel_no_socket_recovery_converges_to_one_usable_daemon() {
        const CLIENTS: usize = 8;

        let _cleanup = RecoveryTestGuard::new();
        clear_daemon_env();
        reset_counters();
        *RECOVERY_RACE_BARRIER
            .lock()
            .expect("barrier mutex poisoned") =
            Some(std::sync::Arc::new(tokio::sync::Barrier::new(CLIENTS)));

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::set_var(
            "KHIVE_RECOVERER_LOCK",
            dir.path().join("khived.recoverer.lock"),
        );
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
        let dispatcher = HarnessDispatch::new("test", config_id);
        let launcher = InProcessDaemonLauncher::new(dispatcher.clone());
        let mut recoverers = tokio::task::JoinSet::new();
        for _ in 0..CLIENTS {
            let launcher = launcher.clone();
            recoverers.spawn(async move {
                kill_and_respawn_with_launcher(config_id, "test", &launcher).await
            });
        }

        let (spawned, skipped, uncertain) =
            tokio::time::timeout(std::time::Duration::from_secs(30), async move {
                let mut spawned: Vec<InProcessDaemonHandle> = Vec::new();
                let mut skipped = 0usize;
                let mut uncertain = 0usize;
                while let Some(result) = recoverers.join_next().await {
                    match result.expect("recoverer task must not panic") {
                        Ok(RecoveryOutcome::Spawned(handle)) => spawned.push(handle),
                        Ok(RecoveryOutcome::Skipped) => skipped += 1,
                        Ok(RecoveryOutcome::Uncertain) => uncertain += 1,
                        Err(error) => panic!("parallel NoSocket recovery failed: {error:?}"),
                    }
                }
                (spawned, skipped, uncertain)
            })
            .await
            .expect("parallel recoverers must quiesce within 30s");
        assert!(
            !spawned.is_empty(),
            "at least one recoverer must launch a daemon"
        );
        assert_eq!(
            spawned.len() + skipped + uncertain,
            CLIENTS,
            "every parallel recoverer must reach a classified outcome"
        );
        // Uncertain is a deadline-bound degraded outcome per
        // docs/api/daemon-lifecycle.md. The healthy parallel path resolves
        // losers as Skipped well inside the 16s deadline, so any Uncertain
        // here is a regression signal, not noise.
        assert_eq!(uncertain, 0, "healthy parallel recovery must not time out");
        assert_eq!(
            launcher.launched_count(),
            spawned.len(),
            "every launch attempt must return its owned test handle"
        );

        drop(connect_when_ready(&sock).await);
        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };
        let response = exchange(&sock, &frame).await;
        assert!(
            response.ok,
            "the surviving daemon must answer stats(): {response:?}"
        );
        assert_eq!(
            dispatcher.dispatch_count(),
            1,
            "probe traffic must not dispatch; the one stats exchange must dispatch once"
        );

        launcher.wait_for_running_count(1).await;
        let handle_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let active_handles = loop {
            let active = spawned
                .iter()
                .filter(|handle| !handle.is_finished())
                .count();
            if active == 1 {
                break active;
            }
            assert!(
                tokio::time::Instant::now() < handle_deadline,
                "launched daemon handles did not converge to one active task; active={active}"
            );
            tokio::task::yield_now().await;
        };
        assert_eq!(
            active_handles,
            1,
            "server-side ownership must converge to one live daemon handle; launched={} running={}",
            launcher.launched_count(),
            launcher.running_count()
        );
        assert!(
            sock.exists(),
            "the surviving daemon must own one socket path"
        );
        assert_eq!(
            std::fs::read_to_string(&pid_file)
                .expect("surviving pid file")
                .trim(),
            std::process::id().to_string(),
            "the surviving in-process daemon must own the sole PID rendezvous"
        );

        for handle in spawned {
            handle.stop().await;
        }
        launcher.wait_for_running_count(0).await;
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&pid_file);
        assert!(
            !sock.exists(),
            "test teardown must remove the daemon socket"
        );
        assert!(
            !pid_file.exists(),
            "test teardown must remove the daemon pid file"
        );
    }

    /// A post-write response loss is ambiguous and therefore terminal. All
    /// clients return the exactly-once refusal, with no kill, spawn, retry, or
    /// follow-up socket connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 12)]
    #[serial]
    async fn parallel_parse_failure_is_terminal_and_never_recovers() {
        const CLIENTS: usize = 8;

        let _cleanup = RecoveryTestGuard::new();
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::set_var(
            "KHIVE_RECOVERER_LOCK",
            dir.path().join("khived.recoverer.lock"),
        );
        std::env::remove_var("KHIVE_NO_DAEMON");

        let listener = tokio::net::UnixListener::bind(&sock).expect("bind crash fixture socket");
        let release = std::sync::Arc::new(tokio::sync::Barrier::new(CLIENTS));
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let mut accepted = 0usize;
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut done_rx => break,
                    incoming = listener.accept() => {
                        let (mut stream, _) = incoming.expect("accept client frame");
                        accepted += 1;
                        if accepted > CLIENTS {
                            // A retry is itself the regression oracle. Do not put
                            // it into the first-wave barrier or teardown would
                            // deadlock waiting for another full cohort.
                            drop(stream);
                            continue;
                        }
                        let release = std::sync::Arc::clone(&release);
                        connections.spawn(async move {
                            read_frame(&mut stream).await.expect("read complete client frame");
                            release.wait().await;
                            // Drop without a response: the request may have dispatched,
                            // so the client must classify this as ParseFailure.
                        });
                    }
                }
            }
            // Teardown race: done_tx fires the instant the last client has its
            // terminal error, but a regression-produced follow-up connect may
            // already sit queued in the listener backlog. Dropping the listener
            // immediately would let it escape uncounted. Quiesce-drain instead:
            // every first-wave connection is already accepted (done only fires
            // after all CLIENTS completed), so anything still arriving here is
            // a retry, and counting it makes `accepted == CLIENTS` below fail
            // the test rather than pass silently.
            while let Ok(incoming) =
                tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept()).await
            {
                let (stream, _) = incoming.expect("accept client frame");
                accepted += 1;
                drop(stream);
            }
            while let Some(connection) = connections.join_next().await {
                connection.expect("crash fixture connection task must not panic");
            }
            accepted
        });

        let frame = std::sync::Arc::new(DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: CFG.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        });
        let mut clients = tokio::task::JoinSet::new();
        for _ in 0..CLIENTS {
            let frame = std::sync::Arc::clone(&frame);
            clients.spawn(async move { forward_or_spawn(&frame).await });
        }

        let terminal_errors = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            async move {
                let mut terminal_errors = 0usize;
                while let Some(result) = clients.join_next().await {
                    match result.expect("client task must not panic") {
                        Some(Err(error)) => {
                            assert!(
                                error
                                    .message
                                    .contains("response lost after request was sent"),
                                "ParseFailure must return the stable ambiguous-forward refusal: {error:?}"
                            );
                            terminal_errors += 1;
                        }
                        other => {
                            panic!("ParseFailure must be a terminal hard error, got {other:?}")
                        }
                    }
                }
                terminal_errors
            },
        )
        .await
        .expect("parallel ParseFailure clients must terminate within 10s");
        done_tx.send(()).expect("crash fixture still running");
        let accepted = server.await.expect("crash fixture must not panic");

        assert_eq!(terminal_errors, CLIENTS);
        assert_eq!(
            accepted, CLIENTS,
            "terminal ParseFailure must not make any follow-up connection"
        );
        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "post-write ambiguity must never trigger daemon lifecycle actions"
        );
        assert_eq!(
            SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "post-write ambiguity must never spawn a replacement"
        );
    }

    // ── probe classifier is fail-CLOSED for same-protocol pre-probe daemons ──
    //
    // Regression test for the version-skew gap: a daemon built BEFORE probe_only
    // was introduced but carrying PROTOCOL_VERSION (same numeric version, older
    // binary) deserialises the probe frame via serde default and falls through to
    // dispatch on the empty `ops` string.  It returns ok=false (parse error on
    // empty ops) WITH matching identity fields (namespace / config / protocol all
    // match).  Before this fix, probe_daemon_identity classified
    // ANY response with matching identity as Alive, leaving the stale daemon in
    // place.
    //
    // After the fix, the classifier requires the probe-ack sentinel shape:
    //   resp.ok && resp.result.is_none() && resp.error.is_none()
    // An ok=false response fails this predicate → Dead → kill+spawn.
    //
    // Fail-if-reverted: removing the `is_probe_ack` check from the classifier
    // causes the ok=false identity-matching response to be classified Alive →
    // kill_and_respawn returns Skipped → KILL_COUNT stays 0 → assertion fails.

    /// Build a response that matches all identity fields but has ok=false (the
    /// shape a pre-probe daemon produces when dispatching the empty-ops probe).
    fn pre_probe_daemon_response(config_id: &str) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some("parse error: empty ops string".to_string()),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(config_id.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        }
    }

    #[tokio::test]
    #[serial]
    async fn probe_classifier_dead_when_same_protocol_daemon_lacks_probe_support() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        // Bind a fake socket that serves one pre-probe response, then stops
        // accepting (simulates a same-protocol daemon without probe_only support).
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake pre-probe socket");
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
        let pre_probe_resp = pre_probe_daemon_response(config_id);
        let fake_handle = tokio::spawn(serve_one_response(listener, pre_probe_resp));

        // Arm FORCE_PID_IS_DAEMON so kill_stale_daemon_inner would attempt SIGTERM
        // IF it were called.  This makes KILL_COUNT the reliable regression signal:
        // if the classifier incorrectly returns Alive (Skipped), kill is not called
        // and KILL_COUNT stays 0.
        FORCE_PID_IS_DAEMON.store(true, std::sync::atomic::Ordering::SeqCst);
        reset_counters();

        let outcome = kill_and_respawn(config_id, "test", &spawn_daemon).await;

        // The fake socket served exactly one response; join it before asserting.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

        // The ok=false response must NOT be classified as Alive.  kill_and_respawn
        // must attempt kill+spawn (Spawned outcome — spawn itself fails because there
        // is no real kkernel binary in test, but KILL_COUNT is checked BEFORE spawn).
        assert!(
            matches!(outcome, Ok(RecoveryOutcome::Spawned(_)) | Err(_)),
            "pre-probe same-protocol daemon must NOT be classified Alive; \
             expected Spawned or spawn-error, got Skipped"
        );
        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "KILL_COUNT must be 1 — the pre-probe response must classify as Dead \
             so kill_stale_daemon_inner is called \
             (this fails if is_probe_ack check is removed and ok=false response \
              is incorrectly classified as Alive)"
        );

        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── dispatch error propagates to client as non-empty message (#91) ─────────
    //
    // Regression for #91: when the daemon's dispatcher returns Err(msg), the
    // client-side `map_response` must surface that message through
    // `forward_or_spawn` as `Some(Err(McpError { message, .. }))` with a
    // non-empty `message`.  Before the #91 fix, some failure paths swallowed the
    // message and the client saw only "daemon returned an error without a message".
    //
    // This test uses a real run_daemon + FailDispatch (always returns Err("forced
    // dispatch error: <detail>")) and drives the full forward_or_spawn path so we
    // exercise both the daemon's response serialization AND the client's
    // map_response deserialization in one round trip.

    #[derive(Clone)]
    struct FailDispatch {
        namespace: String,
        config_id: String,
    }

    #[async_trait]
    impl daemon::DaemonDispatch for FailDispatch {
        async fn dispatch(
            &self,
            _ops: String,
            _presentation: Option<String>,
            _presentation_per_op: Option<Vec<Option<String>>>,
            _format: Option<String>,
            _format_per_op: Option<Vec<Option<String>>>,
            _from_wire: bool,
            _identity: Option<khive_runtime::RequestIdentity>,
        ) -> Result<String, String> {
            Err("forced dispatch error: verb returned an error for testing".to_string())
        }

        async fn warm_all(&self) {}

        fn namespace(&self) -> &str {
            &self.namespace
        }

        fn config_id(&self) -> &str {
            &self.config_id
        }
    }

    #[tokio::test]
    #[serial]
    async fn dispatch_error_propagates_as_non_empty_client_message() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
        let dispatcher = FailDispatch {
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
        };

        let handle = tokio::spawn(async move {
            let _ = run_daemon(dispatcher).await;
        });

        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let result = forward_or_spawn(&frame).await;

        match result {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    !message.is_empty(),
                    "error message forwarded to client must not be empty"
                );
                assert!(
                    message.contains("forced dispatch error"),
                    "client must receive the daemon's error message verbatim; got: {message}"
                );
            }
            Some(Ok(v)) => panic!("FailDispatch always errs; got Ok({v:?})"),
            None => panic!(
                "forward_or_spawn returned None (local fallback) instead of \
                 propagating the daemon's error — the error message was swallowed"
            ),
        }

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── explicit version_mismatch is terminal after the real write (#156) ─────
    //
    // Regression for #156: when a NEWER client connects to an OLD warm daemon,
    // the old daemon responds with `version_mismatch=true` and its own (lower)
    // `daemon_protocol_version`. Before the fix, this went to the generic
    // `Response` arm → `map_response` → hard MCP error without a stable
    // stale-daemon classification.
    //
    // After the fix, `try_forward_inner` detects `version_mismatch=true` &&
    // `daemon_protocol_version < PROTOCOL_VERSION` and returns
    // `ForwardOutcome::ProtocolMismatch`, routing it through the terminal
    // no-retry/no-lifecycle path exactly like the implicit old-daemon case.
    //
    // This test serves one connection returning the protocol-v3 shape that a
    // still-warm pre-process_ref daemon reports to a v4 client
    // (version_mismatch=true, daemon_protocol_version=3) and asserts that
    // try_forward_inner classifies it as ProtocolMismatch (not Response).

    fn explicit_version_mismatch_response(config_id: &str) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some(format!(
                "daemon protocol mismatch: client={} daemon=3 — \
                 rebuild/update the client binary (make local)",
                PROTOCOL_VERSION
            )),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(config_id.to_string()),
            version_mismatch: true,
            daemon_protocol_version: 3,
            metrics: None,
            request_id: None,
        }
    }

    #[tokio::test]
    #[serial]
    async fn v4_client_rejects_warm_v3_daemon_before_accepting_result() {
        clear_daemon_env();
        assert_eq!(PROTOCOL_VERSION, 4, "process_ref is the protocol-v4 change");
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        let listener =
            tokio::net::UnixListener::bind(&sock).expect("bind explicit-mismatch socket");
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

        let mismatch_resp = explicit_version_mismatch_response(config_id);
        let fake_handle = tokio::spawn(serve_one_response(listener, mismatch_resp));

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let outcome = try_forward_inner(&frame).await;

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

        assert!(
            matches!(outcome, ForwardOutcome::ProtocolMismatch),
            "explicit version_mismatch=true with daemon_protocol_version < PROTOCOL_VERSION \
             must classify as ProtocolMismatch (terminal no-retry path), not Response \
             (which would lose the stable stale-daemon classification)"
        );

        clear_daemon_env();
    }

    // ── explicit version_mismatch from NEWER daemon is NOT routed to recovery ──
    //
    // Complementary to the test above: when a stale CLIENT talks to a NEWER
    // daemon, the daemon responds with `version_mismatch=true` and a
    // `daemon_protocol_version > PROTOCOL_VERSION`. Kill+respawn cannot fix this
    // (it would just spawn the same newer daemon again); the client must receive
    // a hard error telling the operator to upgrade the client binary.
    //
    // This test asserts try_forward_inner returns ForwardOutcome::Response
    // (not ProtocolMismatch) so map_response produces the hard error.

    fn newer_daemon_version_mismatch_response(config_id: &str) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some(format!(
                "daemon protocol mismatch: client={} daemon={} — \
                 rebuild/update the client binary (make local)",
                PROTOCOL_VERSION,
                PROTOCOL_VERSION + 1
            )),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(config_id.to_string()),
            version_mismatch: true,
            daemon_protocol_version: PROTOCOL_VERSION + 1,
            metrics: None,
            request_id: None,
        }
    }

    #[tokio::test]
    #[serial]
    async fn try_forward_inner_newer_daemon_mismatch_yields_response_not_recovery() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        let listener = tokio::net::UnixListener::bind(&sock).expect("bind newer-daemon socket");
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

        let mismatch_resp = newer_daemon_version_mismatch_response(config_id);
        let fake_handle = tokio::spawn(serve_one_response(listener, mismatch_resp));

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let outcome = try_forward_inner(&frame).await;

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

        assert!(
            matches!(outcome, ForwardOutcome::Response(_)),
            "version_mismatch=true with daemon_protocol_version > PROTOCOL_VERSION \
             must yield Response (hard error via map_response), not ProtocolMismatch \
             (the client binary, not the daemon, is stale)"
        );

        clear_daemon_env();
    }

    // ── #644: ambiguous post-write outcome never retries or falls back ───────
    //
    // Before the #644 fix, a `ParseFailure` on the first `try_forward_inner`
    // attempt (the real frame was already fully written) triggered
    // `kill_and_respawn` followed by resending the SAME real frame to whatever
    // daemon answered next. If the original (stale) daemon had actually
    // dispatched the mutation before the connection dropped, that retry would
    // execute it a second time on the freshly-spawned daemon.
    //
    // This test proves the fixed contract: once the real frame is confirmed
    // written, `forward_or_spawn` returns a hard error and never opens another
    // connection — not even to a daemon that is fully ready and willing to
    // serve the exact same request.

    #[tokio::test]
    #[serial]
    #[serial_test::serial(config_ledger)]
    async fn ambiguous_write_never_retries_against_freshly_spawned_daemon() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        // Stale daemon: accepts one connection, reads the request, then drops
        // without responding — forces the first try_forward_inner to see
        // ParseFailure (frame written, response lost).
        let stale_listener = tokio::net::UnixListener::bind(&sock).expect("bind stale socket");
        let stale_handle = tokio::spawn(serve_crash_on_dispatch(stale_listener));

        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        // A fully ready "freshly respawned" daemon binds shortly after the
        // stale one drops. If the fix regresses and forward_or_spawn retries
        // the real frame, this listener would happily answer it — that's
        // exactly why connect_count must stay 0.
        let connect_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connect_count_srv = connect_count.clone();
        let resp = frame_ok("stats-result");
        let fresh_sock = sock.clone();
        let fresh_handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let listener =
                tokio::net::UnixListener::bind(&fresh_sock).expect("bind fresh daemon socket");
            if let Ok((mut stream, _)) = listener.accept().await {
                connect_count_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if read_frame(&mut stream).await.is_ok() {
                    if let Ok(payload) = serde_json::to_vec(&resp) {
                        let _ = write_frame(&mut stream, &payload).await;
                    }
                }
            }
        });

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let result = forward_or_spawn(&frame).await;

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stale_handle).await;
        // Give the never-contacted fresh listener a moment to prove it stays idle,
        // then drop it so its task doesn't hang the test process.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        fresh_handle.abort();
        let _ = fresh_handle.await;

        match result {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("not retrying") && message.contains("duplicate execution"),
                    "ambiguous post-write outcome must return the #644 hard-error \
                     message; got: {message}"
                );
            }
            other => {
                panic!("expected Some(Err(..)) for an ambiguous post-write outcome, got {other:?}")
            }
        }

        assert_eq!(
            connect_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "forward_or_spawn must NOT contact any daemon (stale or freshly \
             spawned) again once the real frame has been fully written — \
             retrying risks a duplicate dispatch (#644)"
        );

        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── #644: exactly-once dispatch through the full forward_or_spawn path ───
    //
    // End-to-end version of `recovery_path_dispatches_real_request_exactly_once`
    // (which drives `kill_and_respawn` + `try_forward_inner` directly to avoid
    // a two-socket setup problem). This test drives the full public entry point:
    // a single fake daemon counts every real (non-probe) dispatch, answers
    // `probe_only` frames with a valid identity ack, and closes the connection
    // without responding to the real frame — simulating a crash after dispatch.
    //
    // Fail-if-reverted: if `forward_or_spawn` ever resends the real frame after
    // a confirmed write (the #644 bug), this fake daemon would dispatch it
    // again and DAEMON_DISPATCH would read 2, not 1.

    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_dispatches_real_frame_exactly_once_end_to_end() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        // `forward_or_spawn`'s first attempt writes the real (non-probe) frame
        // directly — no probe precedes it — so this fake daemon only needs to
        // simulate "read the real frame, dispatch it, then crash before
        // responding" for the very first connection.
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake daemon socket");
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
        let dispatch_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_count_srv = dispatch_count.clone();
        let fake_handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                if read_frame(&mut stream).await.is_err() {
                    continue;
                }
                // Count the dispatch, then drop the connection without
                // responding — simulating a crash after the mutation already ran.
                dispatch_count_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(stream);
            }
        });

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let result = forward_or_spawn(&frame).await;

        match result {
            Some(Err(_)) => {}
            other => panic!(
                "expected Some(Err(..)) — not None (silent local fallback) — for a \
                 dispatch-then-crash response, got {other:?}"
            ),
        }
        assert_eq!(
            dispatch_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the real request must be dispatched EXACTLY ONCE; a value of 2 means \
             forward_or_spawn resent the frame after the write already completed"
        );

        fake_handle.abort();
        let _ = fake_handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── #644 boundary: a successful daemon round trip dispatches exactly once ──
    //
    // The crash-after-dispatch test above proves the `Err` boundary (a lost
    // response must never trigger a resend). This test proves the opposite
    // boundary on the SAME counter: when the daemon successfully answers, the
    // real frame must still have been dispatched exactly once — not retried
    // after a successful response, and not dispatched a second time by any
    // fallback path once `forward_or_spawn` already returns `Some(Ok(_))`.
    //
    // Fail-if-reverted: if a future edit ever re-sent the real frame after a
    // successful response (e.g. a stray retry-on-timeout wrapped around the
    // whole attempt), this fake daemon would observe DISPATCH_COUNT == 2.
    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_dispatches_real_frame_exactly_once_on_success() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake daemon socket");
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
        let dispatch_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_count_srv = dispatch_count.clone();
        let cfg_for_srv = config_id.to_string();
        let fake_handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                if read_frame(&mut stream).await.is_err() {
                    continue;
                }
                dispatch_count_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let resp = DaemonResponseFrame {
                    ok: true,
                    result: Some("daemon-handled-stats".to_string()),
                    error: None,
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id: Some(cfg_for_srv.clone()),
                    version_mismatch: false,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    metrics: None,
                    request_id: None,
                };
                let payload = serde_json::to_vec(&resp).expect("serialize response frame");
                let _ = write_frame(&mut stream, &payload).await;
            }
        });

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let result = forward_or_spawn(&frame).await;

        match result {
            Some(Ok(ref body)) => {
                assert_eq!(
                    body, "daemon-handled-stats",
                    "request() must surface the daemon's response verbatim"
                );
            }
            other => {
                panic!("expected Some(Ok(_)) for a successful daemon round trip, got {other:?}")
            }
        }
        assert_eq!(
            dispatch_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a successful daemon round trip must dispatch the real request EXACTLY \
             ONCE; a value other than 1 means forward_or_spawn retried or double-sent \
             the frame around a successful response"
        );

        fake_handle.abort();
        let _ = fake_handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── #667: readiness-timeout fallback must wait for boot quiescence ────────
    //
    // Before this fix, `forward_or_spawn`'s post-respawn readiness loop treated
    // a bare deadline elapse as "no daemon" and returned `None` (silent local
    // fallback) unconditionally — even if a concurrent process was still
    // holding the cold-boot guard (ADR-D3) running migrations / pack schema
    // plans (FTS DDL included). A local writer/searcher racing in at exactly
    // that moment could observe or create a partially-initialized
    // `notes`/`fts_notes` schema (#667).
    //
    // Nothing ever binds the socket in this test (no `NoSocket` outcome here
    // ever writes a real frame, so #644's at-most-once invariant is untouched
    // by this scenario) — `kill_and_respawn`'s own probe therefore sees
    // `NoSocket` too, classifies the (nonexistent) daemon as `Dead`, and calls
    // the real `spawn_daemon()` (which forks this test binary with `mcp
    // --daemon`; the child fails to parse those as libtest args and exits
    // almost immediately without ever binding anything — the same tolerated
    // pattern already used by
    // `try_forward_inner_returns_parse_failure_when_daemon_closes_without_response`
    // in this file).
    //
    // `SPAWN_COUNT` (already incremented for real inside `spawn_daemon`) is
    // used purely as a **synchronization signal**: it tells the background
    // "child boot" thread below that `kill_and_respawn`'s own (much shorter)
    // use of the recovery lock is in its final moments, so the boot thread's
    // blocking `acquire_daemon_boot_guard()` call queues immediately behind
    // it via the real `flock`, rather than racing it — the two never expect
    // to hold the lock at the same time, and the ordering between them is
    // never guessed at with a fixed sleep.
    //
    // Once the boot thread holds the guard (for `GUARD_HOLD`, deliberately
    // longer than `forward_or_spawn`'s fixed 5s readiness deadline), the fix
    // must block inside `wait_for_boot_quiescence_then_reprobe` until that
    // guard is released before it is allowed to decide "genuinely no daemon".
    // The elapsed-time assertion below is the fail-if-reverted oracle for
    // #667: reverting that fence makes `forward_or_spawn` return right at the
    // 5s readiness deadline (measured from well before the boot thread even
    // starts holding the guard), strictly before `GUARD_HOLD` has elapsed.
    //
    // #898: by the time that wait finally ends, THIS call's own spawned child
    // (the test binary, rejected `mcp --daemon` and exited within
    // milliseconds) has long since exited — so the final outcome is now the
    // loud, specific respawn-failure error rather than a silent `None`. That
    // change is orthogonal to what this test actually guards: the fence must
    // still be waited out in full regardless of which terminal outcome
    // follows it, which the elapsed-time assertion below continues to prove.
    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_blocks_on_boot_quiescence_before_local_fallback() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        const GUARD_HOLD: std::time::Duration = std::time::Duration::from_secs(6);
        let boot_thread = std::thread::spawn(|| {
            let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                assert!(
                    std::time::Instant::now() < wait_deadline,
                    "kill_and_respawn never reached spawn_daemon() within 5s; \
                     the boot-holder thread has nothing to queue behind"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // `kill_and_respawn` is at most a few milliseconds from dropping
            // its own lock guard at this point (spawn_daemon() is its last
            // action before returning) — this blocks on the real `flock`
            // until that happens, then holds it for GUARD_HOLD.
            let guard = khive_runtime::daemon::acquire_daemon_boot_guard()
                .expect("test boot holder must acquire the recovery lock");
            std::thread::sleep(GUARD_HOLD);
            drop(guard);
        });

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
            request_id: None,
        };

        let started = std::time::Instant::now();
        let result = forward_or_spawn(&frame).await;
        let elapsed = started.elapsed();

        boot_thread
            .join()
            .expect("boot-holder thread must not panic");

        match &result {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("respawn failed"),
                    "#898: this call's own spawned child is confirmed dead by \
                     now, so the outcome must be the specific loud respawn- \
                     failure error, not a generic message: {message}"
                );
            }
            other => panic!(
                "after boot quiescence, a respawn attempt confirmed dead must \
                 surface loudly (#898) rather than falling back silently, got \
                 {other:?}"
            ),
        }
        assert!(
            elapsed >= GUARD_HOLD,
            "forward_or_spawn must block until the cold-boot guard is released \
             before deciding to fall back locally — returned after {elapsed:?}, \
             faster than the {GUARD_HOLD:?} the boot guard was held, meaning it \
             (or a reverted version of this fix) would local-dispatch while \
             cold-boot schema init could still be in progress (#667)"
        );

        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── daemon stderr log-file helpers (no process spawned) ───────────────────

    #[test]
    fn daemon_log_path_from_home_none_when_home_unset() {
        assert!(daemon_log_path_from_home(None).is_none());
    }

    #[test]
    fn daemon_log_path_from_home_joins_dot_khive_logs() {
        let home = std::ffi::OsStr::new("/home/example");
        let path = daemon_log_path_from_home(Some(home)).expect("home present");
        assert_eq!(
            path,
            std::path::PathBuf::from("/home/example/.khive/logs/khived.log")
        );
    }

    #[test]
    fn daemon_log_should_rotate_under_cap_is_false() {
        assert!(!daemon_log_should_rotate(100, 1000));
    }

    #[test]
    fn daemon_log_should_rotate_at_cap_is_true() {
        assert!(daemon_log_should_rotate(1000, 1000));
    }

    #[test]
    fn daemon_log_should_rotate_over_cap_is_true() {
        assert!(daemon_log_should_rotate(1001, 1000));
    }

    #[test]
    fn prepare_daemon_log_file_creates_dir_and_file_on_first_use() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(".khive").join("logs").join("khived.log");

        let file = prepare_daemon_log_file_with_cap(&log_path, DAEMON_LOG_MAX_BYTES);

        assert!(file.is_some(), "must create dir + file on first use");
        assert!(log_path.exists());
    }

    #[test]
    fn prepare_daemon_log_file_leaves_existing_when_under_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).expect("create log dir");
        let log_path = log_dir.join("khived.log");
        std::fs::write(&log_path, b"existing content\n").expect("seed existing log");

        let file = prepare_daemon_log_file_with_cap(&log_path, 1_000_000);

        assert!(file.is_some());
        assert!(
            !log_dir.join("khived.log.1").exists(),
            "under-cap log must not be rotated"
        );
        let content = std::fs::read_to_string(&log_path).expect("read log");
        assert_eq!(
            content, "existing content\n",
            "append-open must preserve existing content"
        );
    }

    #[test]
    fn prepare_daemon_log_file_rotates_when_over_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).expect("create log dir");
        let log_path = log_dir.join("khived.log");
        std::fs::write(&log_path, vec![7u8; 20]).expect("seed oversized log");

        let file = prepare_daemon_log_file_with_cap(&log_path, 10);

        assert!(file.is_some());
        let backup = log_dir.join("khived.log.1");
        assert!(backup.exists(), "oversized log must rotate to .log.1");
        assert_eq!(
            std::fs::metadata(&backup).expect("backup metadata").len(),
            20,
            "backup must retain the original oversized content"
        );
        assert_eq!(
            std::fs::metadata(&log_path).expect("log metadata").len(),
            0,
            "post-rotation log must start fresh"
        );
    }

    #[test]
    fn prepare_daemon_log_file_rotation_replaces_prior_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).expect("create log dir");
        let log_path = log_dir.join("khived.log");
        let backup = log_dir.join("khived.log.1");
        std::fs::write(&backup, b"stale backup").expect("seed stale backup");
        std::fs::write(&log_path, vec![9u8; 20]).expect("seed oversized log");

        let file = prepare_daemon_log_file_with_cap(&log_path, 10);

        assert!(file.is_some());
        let backup_content = std::fs::read(&backup).expect("read backup");
        assert_eq!(
            backup_content,
            vec![9u8; 20],
            "rotation must replace the prior .log.1, not merge with it"
        );
    }

    #[test]
    fn prepare_daemon_log_file_returns_none_when_dir_creation_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Put a regular FILE where the log directory needs to be, so
        // create_dir_all cannot create a directory at that path.
        let blocker = dir.path().join("logs");
        std::fs::write(&blocker, b"not a directory").expect("seed blocker file");
        let log_path = blocker.join("khived.log");

        assert!(prepare_daemon_log_file_with_cap(&log_path, DAEMON_LOG_MAX_BYTES).is_none());
    }

    // ── #645: remove_daemon_paths_if_still_stale ownership recheck ───────────
    //
    // These mirror the existing `shutdown_cleanup_skips_when_*` tests in
    // `khive-runtime/src/daemon.rs` (owner-checked cleanup on the daemon's own
    // shutdown path) but exercise the CLIENT's stale-daemon cleanup instead:
    // between observing a PID as stale and reaching the unlink, a concurrent
    // starter that could not rely on the recovery lock alone (e.g. it failed
    // to acquire it) may have already claimed the rendezvous. Deterministic
    // state fabrication (not a sleep-based race) proves each skip condition.

    #[test]
    #[serial]
    fn remove_daemon_paths_if_still_stale_removes_when_pid_unchanged() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("khived.pid");
        let sock = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::fs::write(&pid_file, "4242").expect("write pid file");
        std::fs::write(&sock, "stale socket placeholder").expect("write stale sock placeholder");

        remove_daemon_paths_if_still_stale(&pid_file, Some(4242));

        assert!(!pid_file.exists(), "unchanged pid file must be removed");
        assert!(!sock.exists(), "stale socket must be removed");
        clear_daemon_env();
    }

    #[test]
    #[serial]
    fn remove_daemon_paths_if_still_stale_skips_when_pid_file_changed() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("khived.pid");
        let sock = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &sock);
        // A replacement daemon already wrote its own (different) pid here.
        std::fs::write(&pid_file, "5555").expect("write replacement pid file");
        std::fs::write(&sock, "replacement socket placeholder").expect("write sock placeholder");

        remove_daemon_paths_if_still_stale(&pid_file, Some(4242));

        assert!(
            pid_file.exists(),
            "replacement daemon's pid file must survive when it no longer \
             matches the expected (pre-SIGTERM) pid"
        );
        assert!(
            sock.exists(),
            "replacement daemon's socket must survive alongside its pid file"
        );
        clear_daemon_env();
    }

    #[test]
    #[serial]
    fn remove_daemon_paths_if_still_stale_skips_when_socket_has_a_live_listener() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("khived.pid");
        let sock = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::fs::write(&pid_file, "4242").expect("write pid file matching expected_pid");
        // A replacement daemon already bound the socket path — even though the
        // pid file on disk has not been overwritten yet (e.g. its bind landed
        // just before its own pid-write).
        let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind live socket");

        remove_daemon_paths_if_still_stale(&pid_file, Some(4242));

        assert!(
            sock.exists(),
            "a socket with a live listener must never be unlinked"
        );
        assert!(
            pid_file.exists(),
            "pid file must be left alone alongside the live socket"
        );
        clear_daemon_env();
    }

    // ── bridge self-heal on ProtocolMismatch (#714) ───────────────────────────

    #[test]
    fn resumed_generation_from_args_absent_is_none() {
        let argv = vec![
            "kkernel".to_string(),
            "mcp".to_string(),
            "--daemon".to_string(),
        ];
        assert_eq!(resumed_generation_from_args(argv.into_iter()), None);
    }

    #[test]
    fn resumed_generation_from_args_present_parses_value() {
        let argv = vec![
            "kkernel".to_string(),
            "mcp".to_string(),
            "--resumed-generation=1".to_string(),
        ];
        assert_eq!(resumed_generation_from_args(argv.into_iter()), Some(1));
    }

    #[test]
    fn resumed_generation_from_args_malformed_value_is_none() {
        let argv = vec![
            "kkernel".to_string(),
            "--resumed-generation=notanumber".to_string(),
        ];
        assert_eq!(resumed_generation_from_args(argv.into_iter()), None);
    }

    #[test]
    fn resumed_generation_from_args_takes_the_last_occurrence() {
        // Defensive: `reexec_in_place` filters any pre-existing marker before
        // appending its own, so duplicates should never occur in practice —
        // but if one ever did, the last one (the freshest exec's own marker)
        // must win, not the first.
        let argv = vec![
            "kkernel".to_string(),
            "--resumed-generation=1".to_string(),
            "--resumed-generation=2".to_string(),
        ];
        assert_eq!(resumed_generation_from_args(argv.into_iter()), Some(2));
    }

    // Cold-start non-regression (issue #714, self-heal test plan item 4): a
    // real `cargo test` process's own argv never carries the marker, so the
    // production entry point must resolve to `None` — the same guarantee
    // `KhiveMcpServer::serve_stdio` relies on to keep using the normal
    // `.serve()` handshake for every ordinary session.
    #[test]
    fn resumed_generation_is_none_for_a_normal_test_process() {
        assert_eq!(resumed_generation(), None);
    }

    // Loop-breaker guard rail (issue #714, self-heal test plan item 2): a
    // resumed generation that observes ProtocolMismatch again must take the
    // fallback, never a second exec. Pure decision function — no live
    // process needed.
    #[test]
    fn decide_mismatch_recovery_first_generation_schedules_reexec() {
        assert_eq!(
            decide_mismatch_recovery(None),
            MismatchRecovery::ReexecScheduled
        );
    }

    #[test]
    fn decide_mismatch_recovery_resumed_generation_drains_and_exits() {
        assert_eq!(
            decide_mismatch_recovery(Some(1)),
            MismatchRecovery::DrainAndExit
        );
    }

    // Ordering regression (issue #714, self-heal test plan item 1): arming a
    // self-heal action must never fire it — only `fire_pending_self_heal`
    // (the stdio transport's post-flush hook, exercised for real in
    // `crates/kkernel/tests/mcp_bridge_reexec_protocol_mismatch.rs`) may take
    // the armed action, and only after a flush has actually completed. This
    // is the exact bug the original toy-server evidence reproduced (an
    // in-flight response lost because `execv()` ran before the response
    // bytes were flushed) — these tests pin the "arm never fires" half of
    // that contract; the "fire only after a real flush" half is what the
    // live integration test proves.

    #[test]
    #[serial]
    fn schedule_reexec_on_mismatch_arms_without_firing() {
        reset_self_heal_counters();
        schedule_reexec_on_mismatch();
        assert_eq!(
            REEXEC_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "arming must never exec synchronously or eagerly"
        );
        fire_pending_self_heal();
        assert_eq!(
            REEXEC_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the armed action must fire exactly once it is taken"
        );
    }

    #[test]
    #[serial]
    fn schedule_drain_and_exit_arms_without_firing() {
        reset_self_heal_counters();
        schedule_drain_and_exit();
        assert_eq!(
            DRAIN_EXIT_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "arming must never exit synchronously or eagerly"
        );
        fire_pending_self_heal();
        assert_eq!(
            DRAIN_EXIT_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the armed action must fire exactly once it is taken"
        );
    }

    #[test]
    #[serial]
    fn fire_pending_self_heal_is_a_no_op_when_nothing_is_armed() {
        reset_self_heal_counters();
        fire_pending_self_heal();
        assert_eq!(
            REEXEC_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            DRAIN_EXIT_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[test]
    #[serial]
    fn fire_pending_self_heal_takes_the_armed_action_exactly_once() {
        reset_self_heal_counters();
        schedule_reexec_on_mismatch();
        fire_pending_self_heal();
        // A second flush completing after the action was already taken must
        // not re-fire it — `PENDING_SELF_HEAL` is `take()`n, not just read.
        fire_pending_self_heal();
        assert_eq!(
            REEXEC_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "must fire exactly once even if fire_pending_self_heal is called again"
        );
    }
}
