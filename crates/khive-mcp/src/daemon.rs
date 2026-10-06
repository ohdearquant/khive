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
    DaemonRequestFrame, DaemonResponseFrame, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use khive_runtime::process_retry::{spawn_retrying_executable_busy, EXECUTABLE_BUSY_BACKOFF_MS};
use rmcp::ErrorData as McpError;
use sha2::{Digest, Sha256};
use tokio::net::UnixStream;

use crate::tools::request::RequestParams;

pub(crate) mod executable;

/// Snapshot the stdio bridge image before config discovery or database boot can
/// wait across an installation. Daemon and one-shot exec entrypoints omit this.
pub fn capture_bridge_executable() {
    let _ = crate::server::bridge_instance_id();
    executable::capture_at_startup();
}

#[cfg(test)]
mod bridge_diagnostics_tests;

#[cfg(test)]
mod memory_namespace_tests;

#[cfg(test)]
mod test_harness;

#[cfg(test)]
use test_harness::{
    reset_counters, DAEMON_DISPATCH, FORCED_CONNECT_ERROR, FORCE_PID_IS_DAEMON,
    FORCE_PID_IS_FOREIGN, KILL_COUNT, RECOVERY_RACE_BARRIER, SIGTERM_COUNT, SPAWN_COUNT,
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

#[derive(serde::Serialize)]
pub(crate) struct BridgeFallbackReasons {
    config_mismatch: usize,
    namespace_mismatch: usize,
    no_socket: usize,
    parse_failure: usize,
    protocol_mismatch: usize,
}

#[derive(serde::Serialize)]
pub(crate) struct BridgeDiagnosticsSnapshot {
    bridge_instance_id: uuid::Uuid,
    pid: u32,
    fallback_reasons: BridgeFallbackReasons,
    fallback_total: usize,
    strict_violations: usize,
}

pub(crate) fn bridge_diagnostics_snapshot() -> Option<BridgeDiagnosticsSnapshot> {
    use std::sync::atomic::Ordering::SeqCst;

    let fallback_reasons = BridgeFallbackReasons {
        config_mismatch: FALLBACK_CONFIG_MISMATCH.load(SeqCst),
        namespace_mismatch: FALLBACK_NAMESPACE_MISMATCH.load(SeqCst),
        no_socket: FALLBACK_NO_SOCKET.load(SeqCst),
        parse_failure: FALLBACK_PARSE_FAILURE.load(SeqCst),
        protocol_mismatch: FALLBACK_PROTOCOL_MISMATCH.load(SeqCst),
    };
    let fallback_total = [
        fallback_reasons.config_mismatch,
        fallback_reasons.namespace_mismatch,
        fallback_reasons.no_socket,
        fallback_reasons.parse_failure,
        fallback_reasons.protocol_mismatch,
    ]
    .into_iter()
    .try_fold(0usize, usize::checked_add)?;

    Some(BridgeDiagnosticsSnapshot {
        bridge_instance_id: crate::server::bridge_instance_id(),
        pid: std::process::id(),
        fallback_reasons,
        fallback_total,
        strict_violations: FALLBACK_STRICT_VIOLATIONS.load(SeqCst),
    })
}

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

fn first_config_mismatch_field(client: &str, daemon: Option<&str>) -> &'static str {
    khive_runtime::daemon::first_config_mismatch_field(client, daemon)
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
        return Some(Err(daemon_mcp_error(
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

#[cfg(test)]
pub(crate) fn test_recordable_fallback_rejected(reason: FallbackReason) -> bool {
    matches!(
        fallback_or_reject(reason, "client", None, "local"),
        Some(Err(_))
    )
}

// ── DaemonDispatch impl ───────────────────────────────────────────────────────

#[async_trait]
impl daemon::DaemonDispatch for crate::server::KhiveMcpServer {
    fn plan(&self, ops: &str) -> String {
        self.plan_ops(ops)
    }

    fn request_read_timeout(&self, ops: &str) -> std::time::Duration {
        crate::request_policy::read_timeout(ops, khive_storage::request_read_timeout_from_env())
    }

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
        self.dispatch_with_error_detail(
            ops,
            presentation,
            presentation_per_op,
            format,
            format_per_op,
            from_wire,
            identity,
        )
        .await
        .map_err(|error| error.message)
    }

    async fn dispatch_with_error_detail(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, daemon::DaemonDispatchError> {
        if khive_request::parse_request(&ops)
            .is_ok_and(|parsed| parsed.ops.iter().any(|op| op.tool == "bridge.diagnostics"))
        {
            return Err(daemon::DaemonDispatchError::new(
                "bridge.diagnostics is available only on the stdio bridge".to_string(),
                Some(serde_json::json!({
                    "kind": "invalid_input",
                    "message": "bridge.diagnostics is available only on the stdio bridge",
                })),
            ));
        }
        let params = RequestParams {
            plan: None,
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
        .map_err(|error| daemon::DaemonDispatchError::new(error.message.to_string(), error.data))
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

    fn idle_retirement_blockers(&self) -> Vec<String> {
        let main = self.pool();
        let mut blockers: Vec<String> = main
            .clone()
            .into_iter()
            .chain(self.secondary_pools())
            .enumerate()
            .filter_map(|(index, pool)| {
                (pool.retirement_writer_holds() != 0)
                    .then(|| format!("backend:{index}:held_writer"))
            })
            .collect();
        if main.is_none() {
            blockers.push("main_backend_pool_inventory_unavailable".to_owned());
        }
        blockers
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
#[derive(Debug)]
enum ForwardOutcome {
    /// Successfully received and decoded a response frame.
    Response(Box<DaemonResponseFrame>),
    /// The socket is absent/refused, or the frame failed before it could reach
    /// dispatch. These outcomes are safe to route through recovery.
    NoSocket,
    /// Serialization produced a frame over the transport cap. `write_frame`
    /// would refuse it before writing any byte, so reconnecting or spawning
    /// cannot change the outcome.
    RequestTooLarge { bytes: usize },
    /// This process could not establish whether a daemon is listening. An OS
    /// access/policy failure is not proof that the daemon is absent, so it must
    /// never enter lifecycle recovery or local fallback (#1242).
    Unreachable {
        kind: std::io::ErrorKind,
        os_error_code: Option<i32>,
    },
    /// Invalid response framing/JSON or a response timeout after a full write.
    ParseFailure,
    /// EOF/reset after a full write; distinct from malformed frames/timeouts.
    ResponseLost,
    /// Connected and decoded a response, but the daemon's `daemon_protocol_version`
    /// does not match [`PROTOCOL_VERSION`], in either direction. Below: the
    /// new-client + old-daemon scenario, implicit (a pre-versioning daemon ignores
    /// the unknown request field and returns a decodable response whose protocol
    /// fields default to `false`/`0`) or explicit (`version_mismatch=true` with the
    /// daemon's lower number). Above: this bridge is the stale side, a rebuild
    /// swapped the on-disk binary and respawned the daemon under a newer protocol
    /// while this process kept running the old one. Since the real request was
    /// already written, the client treats both exactly like `ParseFailure`: a hard
    /// error without retrying, locally dispatching, killing, or respawning; the
    /// consumer arms the #714 self-heal beside it.
    ProtocolMismatch { daemon_protocol_version: u32 },
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

#[cfg(test)]
async fn try_forward_inner(frame: &DaemonRequestFrame) -> ForwardOutcome {
    try_forward_before(frame, None).await
}

fn socket_exchange_deadline(
    probe_only: bool,
    ops: &str,
    retry_deadline: Option<tokio::time::Instant>,
) -> tokio::time::Instant {
    // One absolute deadline bounds connect, write, and read together, so the
    // whole socket exchange can never exceed the ceiling the read phase alone
    // used to honour on its own. A same-UID peer that accepts the connection
    // and never reads it would otherwise block the write forever with
    // nothing above this function able to stop it.
    //
    // When this task inherited the caller's request-read deadline — the
    // spawned forward task in `server.rs` wraps this call in
    // `khive_storage::inherit_request_read_context` — that same absolute
    // instant is reused here, so an admitted forward cannot outlive the
    // deadline the rest of the request already obeys. Callers that never
    // scoped a request context (this function's own unit tests, or any
    // future caller outside the MCP bridge) fall back to a fresh relative
    // ceiling derived from the configured read timeout and the request's
    // valid long-poll waits plus a five-second transport margin.
    if probe_only {
        // Lifecycle probes are independent of the request that happened to
        // trigger recovery. Their caller supplies a fresh probe deadline; an
        // expired request deadline must never make a live daemon appear dead.
        retry_deadline.unwrap_or_else(|| {
            tokio::time::Instant::now() + khive_storage::request_read_timeout_from_env()
        })
    } else {
        let request_deadline = khive_storage::capture_request_read_context()
            .deadline()
            .map(khive_storage::RequestReadDeadline::async_at)
            .unwrap_or_else(|| {
                tokio::time::Instant::now()
                    + crate::request_policy::read_timeout(
                        ops,
                        khive_storage::request_read_timeout_from_env(),
                    )
            });
        retry_deadline.map_or(request_deadline, |retry| retry.min(request_deadline))
    }
}

async fn try_forward_before(
    frame: &DaemonRequestFrame,
    retry_deadline: Option<tokio::time::Instant>,
) -> ForwardOutcome {
    let payload = match serde_json::to_vec(frame) {
        Ok(p) => p,
        Err(_) => return ForwardOutcome::NoSocket,
    };
    // Check before even connecting. The same cap is enforced by write_frame,
    // but that function returns InvalidData before its first write; treating
    // that as NoSocket would enter deterministic, futile lifecycle recovery.
    if payload.len() > MAX_FRAME_BYTES {
        return ForwardOutcome::RequestTooLarge {
            bytes: payload.len(),
        };
    }
    let sock = socket_path();
    #[cfg(test)]
    {
        let forced_error = FORCED_CONNECT_ERROR.load(std::sync::atomic::Ordering::SeqCst);
        if forced_error != 0 {
            return classify_socket_connect_error(std::io::Error::from_raw_os_error(forced_error));
        }
    }

    let deadline = socket_exchange_deadline(frame.probe_only, &frame.ops, retry_deadline);

    let mut stream = match tokio::time::timeout_at(deadline, UnixStream::connect(&sock)).await {
        Ok(Ok(s)) => s,
        Ok(Err(error)) => return classify_socket_connect_error(error),
        Err(_elapsed) => {
            // Nothing was ever written to a peer, so this is exactly the
            // ordinary not-listening case: safe to let recovery proceed.
            tracing::warn!(
                "daemon connect timed out before the socket-exchange deadline — \
                 treating as no socket"
            );
            return ForwardOutcome::NoSocket;
        }
    };
    match tokio::time::timeout_at(deadline, write_frame(&mut stream, &payload)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return ForwardOutcome::NoSocket,
        Err(_elapsed) => {
            // The write itself never completed. Dropping `stream` here (it
            // goes out of scope on this return) closes the socket, so a peer
            // that later resumes reading observes end-of-stream in the
            // middle of the frame — at most the 4-byte length prefix and a
            // short partial body — and can never finish `read_frame`'s
            // `read_exact` for the full body. It cannot misdispatch a
            // truncated request.
            //
            // Because nothing was fully delivered, this is NOT the
            // post-write ambiguity the read-timeout arm below returns for
            // (where the write had already completed and the request may
            // already be executing on the daemon side). Treat it like
            // `NoSocket` — the pre-write, "nothing sent" case — so
            // `forward_or_spawn_with_exe`'s recovery path can kill and
            // respawn a stuck peer and retry, instead of surfacing a
            // permanent no-retry ambiguity error for what may just be a
            // wedged process squatting on the socket path.
            tracing::warn!(
                "daemon write timed out before the socket-exchange deadline — \
                 dropping the connection and treating as no socket"
            );
            return ForwardOutcome::NoSocket;
        }
    }
    // The request is now fully written and may already be executing (or
    // committed) on the daemon side, so a stalled or unresponsive daemon
    // must not leave this read pending forever — that would keep the
    // calling task (and the MCP handler awaiting it) alive indefinitely.
    // Bounded by the same deadline as the connect and write phases above.
    let resp = match tokio::time::timeout_at(deadline, read_frame(&mut stream)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            // The request was sent but the daemon closed the connection before
            // sending a response frame. This does not establish why it closed
            // or whether dispatch completed. Only classified reads can replay;
            // mutations remain ambiguous, with no recovery or local fallback.
            tracing::warn!(
                error = %e,
                "daemon response unavailable after full request write"
            );
            return if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) {
                ForwardOutcome::ResponseLost
            } else {
                ForwardOutcome::ParseFailure
            };
        }
        Err(_elapsed) => {
            // The daemon never answered within the deadline. The write
            // already completed, so a timeout remains terminal. It does not
            // establish whether the daemon is still executing this request.
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
            // above. `daemon_protocol_version > PROTOCOL_VERSION` means this bridge
            // binary is behind: a rebuild swapped the on-disk binary and respawned
            // the daemon under a newer protocol while this process kept running the
            // old one. That is the scenario the #714 self-heal exists for, so it
            // takes the same terminal path and the consumer arms the re-exec, which
            // picks up the on-disk binary the daemon itself was spawned from.
            // Leaving that direction to `map_response` returned the hard error on
            // every request for the rest of the process's life and never re-exec'd.
            if frame.daemon_protocol_version != PROTOCOL_VERSION {
                tracing::warn!(
                    daemon_version = frame.daemon_protocol_version,
                    expected = PROTOCOL_VERSION,
                    explicit_mismatch = frame.version_mismatch,
                    "daemon protocol version mismatch after request write — rejecting without retry",
                );
                return ForwardOutcome::ProtocolMismatch {
                    daemon_protocol_version: frame.daemon_protocol_version,
                };
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

const HANDOVER_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
const HANDOVER_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

fn bounded_retry_deadline() -> tokio::time::Instant {
    let deadline = tokio::time::Instant::now() + HANDOVER_RETRY_WINDOW;
    khive_storage::capture_request_read_context()
        .deadline()
        .map(khive_storage::RequestReadDeadline::async_at)
        .map_or(deadline, |caller| caller.min(deadline))
}

fn pid_file_directory_is_trusted_if_present(pid_file: &std::path::Path) -> Result<bool, String> {
    let parent = pid_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    match std::fs::metadata(parent) {
        // First startup may not have created the default rendezvous directory
        // yet. No PID record exists to read; daemon startup checks the parent
        // before it creates the PID file.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        _ => daemon::ensure_pid_file_dir_is_trusted(pid_file)
            .map(|()| true)
            .map_err(|error| format!("{error:#}")),
    }
}

fn recorded_daemon_is_alive() -> bool {
    let pid_file = pid_path();
    match pid_file_directory_is_trusted_if_present(&pid_file) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "daemon PID-file directory is not trusted; skipping the recorded-process probe"
            );
            return false;
        }
    }
    std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .is_some_and(process_is_alive)
}

/// A process supervisor's claim on the daemon rendezvous, read from
/// [`daemon::supervisor_marker_path`]. Presence gives the supervisor a bounded
/// opportunity to bind the socket before ordinary client bootstrap resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SupervisorMarker {
    job: String,
    pid: u32,
    restart_interval: std::time::Duration,
    modified: Option<std::time::SystemTime>,
}

const DEFAULT_SUPERVISOR_RESTART_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const SUPERVISOR_RESTART_ATTEMPTS: u32 = 3;

impl SupervisorMarker {
    fn pid_is_alive(&self) -> bool {
        process_is_alive(self.pid)
    }

    fn wait_deadline(&self, request_started: tokio::time::Instant) -> tokio::time::Instant {
        self.restart_interval
            .checked_mul(SUPERVISOR_RESTART_ATTEMPTS)
            .and_then(|bound| request_started.checked_add(bound))
            .unwrap_or_else(|| {
                request_started + DEFAULT_SUPERVISOR_RESTART_INTERVAL * SUPERVISOR_RESTART_ATTEMPTS
            })
    }

    fn age(&self) -> Option<std::time::Duration> {
        self.modified
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
    }
}

/// Read the supervision marker, if any. A marker file that exists but cannot
/// be parsed is not the same as no marker: it still names a claim (job
/// `<unreadable>`, pid 0 — never alive per [`process_is_alive`]'s `pid <= 0`
/// guard), so it still suppresses this client's spawn for the default finite
/// interval. Only a genuinely absent file means "no marker".
fn read_supervisor_marker() -> Option<SupervisorMarker> {
    let path = daemon::supervisor_marker_path();
    let modified = std::fs::symlink_metadata(&path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let read_regular_marker = || -> std::io::Result<String> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;

        // Inspect the opened file, not only a pathname that can be replaced.
        // Nonblocking open prevents a FIFO from stalling before that check;
        // no-follow treats a symlink as an unreadable declaration.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "supervisor marker is not a regular file",
            ));
        }
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(contents)
    };
    let contents = match read_regular_marker() {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            return Some(SupervisorMarker {
                job: "<unreadable>".to_owned(),
                pid: 0,
                restart_interval: DEFAULT_SUPERVISOR_RESTART_INTERVAL,
                modified,
            })
        }
    };
    let mut lines = contents.lines();
    let job = lines.next().unwrap_or("").trim();
    let pid = lines
        .next()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .unwrap_or(0);
    // Legacy two-line markers and malformed intervals retain the finite
    // default suppression, rather than accidentally authorizing an early spawn.
    let restart_interval = lines
        .next()
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_SUPERVISOR_RESTART_INTERVAL);
    Some(SupervisorMarker {
        job: if job.is_empty() {
            "<unnamed>".to_owned()
        } else {
            job.to_owned()
        },
        pid,
        restart_interval,
        modified,
    })
}

fn supervisor_marker_lock_path(marker: &std::path::Path) -> std::path::PathBuf {
    let mut lock_path = marker.as_os_str().to_os_string();
    lock_path.push(".lock");
    std::path::PathBuf::from(lock_path)
}

/// Serialize the client's last ownership decision with launcher publication.
/// The launcher locks this same permanent `<marker>.lock` inode through publish
/// and exec. A bounded nonblocking retry keeps a stalled launcher from pinning
/// a request worker; failure must never authorize a competing spawn.
async fn acquire_supervisor_marker_lock() -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    let marker = daemon::supervisor_marker_path();
    if let Some(parent) = marker.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(supervisor_marker_lock_path(&marker))?;
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(RECOVERER_LOCK_TIMEOUT_MS);
    let deadline = khive_storage::capture_request_read_context()
        .deadline()
        .map(khive_storage::RequestReadDeadline::async_at)
        .map_or(deadline, |caller| caller.min(deadline));
    loop {
        if khive_storage::request_read_is_cancelled() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                tokio::time::sleep_until((now + HANDOVER_RETRY_INTERVAL).min(deadline)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

struct SupervisorWaitResult {
    outcome: ForwardOutcome,
    /// Only true when the first marker's grace actually expired while a marker
    /// remained. Marker disappearance alone cannot authorize degraded bootstrap.
    degraded_bootstrap: bool,
}

/// Wait under the first observed restart interval, anchored to request entry.
/// Rewrites (including a fresh PID or interval) update diagnostics, not the
/// bound. A socket response, marker removal, or caller termination wins early.
async fn wait_for_supervisor(
    frame: &DaemonRequestFrame,
    replay: &mut ReadReplayBudget,
    request_started: tokio::time::Instant,
    initial_marker: SupervisorMarker,
) -> Result<SupervisorWaitResult, McpError> {
    #[cfg(test)]
    supervisor_discovery_hook(SupervisorDiscoveryPoint::SupervisorWait);
    let supervisor_deadline = initial_marker.wait_deadline(request_started);
    let caller_deadline = khive_storage::capture_request_read_context()
        .deadline()
        .map(khive_storage::RequestReadDeadline::async_at);
    let retry_deadline =
        caller_deadline.map_or(supervisor_deadline, |d| d.min(supervisor_deadline));
    loop {
        let Some(marker) = read_supervisor_marker() else {
            return Ok(SupervisorWaitResult {
                outcome: ForwardOutcome::NoSocket,
                degraded_bootstrap: false,
            });
        };
        let now = tokio::time::Instant::now();
        if khive_storage::request_read_is_cancelled()
            || caller_deadline.is_some_and(|deadline| now >= deadline)
        {
            return Err(supervised_daemon_error(&marker));
        }
        if now >= supervisor_deadline {
            tracing::warn!(
                reason = "supervised_daemon_bootstrap_degraded",
                job = %marker.job,
                pid = marker.pid,
                pid_alive = marker.pid_is_alive(),
                marker_age_secs = ?marker.age().map(|age| age.as_secs_f64()),
                time_waited_secs = request_started.elapsed().as_secs_f64(),
                "supervisor present, daemon absent; proceeding with guarded client bootstrap"
            );
            return Ok(SupervisorWaitResult {
                outcome: ForwardOutcome::NoSocket,
                degraded_bootstrap: true,
            });
        }
        sleep_until_retry(retry_deadline).await;
        if tokio::time::Instant::now() >= retry_deadline
            || khive_storage::request_read_is_cancelled()
        {
            continue;
        }
        let outcome = try_forward_with_read_replay(frame, replay, Some(retry_deadline)).await;
        if !matches!(outcome, ForwardOutcome::NoSocket) {
            return Ok(SupervisorWaitResult {
                outcome,
                degraded_bootstrap: false,
            });
        }
    }
}

async fn sleep_until_retry(deadline: tokio::time::Instant) {
    tokio::time::sleep_until((tokio::time::Instant::now() + HANDOVER_RETRY_INTERVAL).min(deadline))
        .await;
}

struct ReadReplayBudget {
    remaining: usize,
    deadline: Option<tokio::time::Instant>,
}

impl ReadReplayBudget {
    fn new(enabled: bool) -> Self {
        Self {
            remaining: if enabled { 1 } else { 0 },
            deadline: None,
        }
    }
}

async fn try_forward_with_read_replay(
    frame: &DaemonRequestFrame,
    replay: &mut ReadReplayBudget,
    attempt_deadline: Option<tokio::time::Instant>,
) -> ForwardOutcome {
    let outcome = try_forward_before(frame, attempt_deadline).await;
    if !matches!(outcome, ForwardOutcome::ResponseLost) || replay.remaining == 0 {
        return outcome;
    }
    let deadline = *replay.deadline.get_or_insert_with(|| {
        let allowance = crate::request_policy::read_timeout(
            &frame.ops,
            khive_storage::request_read_timeout_from_env(),
        );
        let deadline = tokio::time::Instant::now() + allowance + HANDOVER_RETRY_INTERVAL;
        let deadline = khive_storage::capture_request_read_context()
            .deadline()
            .map(khive_storage::RequestReadDeadline::async_at)
            .map_or(deadline, |caller| caller.min(deadline));
        attempt_deadline.map_or(deadline, |attempt| attempt.min(deadline))
    });
    while replay.remaining > 0
        && tokio::time::Instant::now() < deadline
        && !khive_storage::request_read_is_cancelled()
    {
        sleep_until_retry(deadline).await;
        if tokio::time::Instant::now() >= deadline || khive_storage::request_read_is_cancelled() {
            break;
        }
        replay.remaining -= 1;
        match try_forward_before(frame, Some(deadline)).await {
            ForwardOutcome::NoSocket | ForwardOutcome::ResponseLost => {}
            ForwardOutcome::Response(response)
                if response.config_mismatch
                    || response.namespace_mismatch
                    || !response.served_config_id.as_deref().is_some_and(|served| {
                        khive_runtime::daemon::config_ids_compatible(&frame.config_id, served)
                    }) =>
            {
                // A later identity rejection cannot erase the first dispatch
                // or permit map_response to select local fallback.
                return ForwardOutcome::ResponseLost;
            }
            other => return other,
        }
    }
    // A read may have executed before losing its response. Even if its retry
    // only saw missing sockets, never convert this to recovery/local fallback.
    ForwardOutcome::ResponseLost
}

fn daemon_mcp_error(message: impl Into<String>, data: Option<serde_json::Value>) -> McpError {
    let error = daemon::DaemonDispatchError::new(message, data);
    McpError::internal_error(error.message, Some(error.error_detail))
}

fn request_too_large_error(bytes: usize) -> McpError {
    let message =
        format!("request too large: {bytes} bytes exceeds {MAX_FRAME_BYTES} byte daemon IPC cap");
    let error = daemon::DaemonDispatchError::new(
        message,
        Some(serde_json::json!({
            "kind": "transport",
            "code": "request_frame_size_limit",
            "frame_bytes": bytes,
            "max_frame_bytes": MAX_FRAME_BYTES,
            "domain_disposition": khive_runtime::DomainDisposition::NotCommitted.as_str(),
        })),
    );
    McpError::invalid_params(error.message, Some(error.error_detail))
}

/// The operator-facing text for a protocol mismatch, by direction. A daemon ahead
/// of this bridge is the rebuilt-binary case: the bridge re-execs the on-disk binary
/// once this response has flushed (#714), so the caller's next request reaches a
/// bridge that matches.
fn protocol_mismatch_message(daemon_protocol_version: u32) -> String {
    if daemon_protocol_version > PROTOCOL_VERSION {
        format!(
            "daemon protocol mismatch: this bridge speaks version {PROTOCOL_VERSION}, the \
             daemon speaks {daemon_protocol_version}; the bridge re-execs the current binary \
             after this response, retry the request"
        )
    } else {
        format!(
            "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
             run `make local` to rebuild the daemon binary"
        )
    }
}

fn protocol_mismatch_error(message: String, data: Option<serde_json::Value>) -> McpError {
    let mut error = daemon::DaemonDispatchError::new(message, data);
    error.error_detail["domain_disposition"] =
        serde_json::json!(khive_runtime::DomainDisposition::Unknown.as_str());
    error.error_detail["code"] = serde_json::json!("version_mismatch");
    error.error_detail["kind"] = serde_json::json!("protocol");
    if let Some(fields) = error.error_detail.as_object_mut() {
        fields.remove("domain_result");
    }
    McpError::internal_error(error.message, Some(error.error_detail))
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
        return Some(Err(protocol_mismatch_error(msg, resp.error_detail)));
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
    // under a compatible config. A legacy daemon omits `served_config_id` (→ None)
    // and a daemon with any incompatible field echoes a different id — both fall back local.
    if !resp.served_config_id.as_deref().is_some_and(|served| {
        khive_runtime::daemon::config_ids_compatible(expected_config_id, served)
    }) {
        return fallback_or_reject(
            FallbackReason::ConfigMismatch,
            expected_config_id,
            resp.served_config_id.as_deref(),
            namespace_client,
        );
    }
    if resp.ok {
        if resp
            .error_detail
            .as_ref()
            .and_then(|detail| detail.get("lexical_timeout"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            tracing::warn!(source = "daemon_response", "lexical read timed out");
        }
        Some(Ok(resp.result.unwrap_or_default()))
    } else {
        let msg = resp.error.unwrap_or_else(|| {
            format!(
                "daemon returned an error without a message \
                 (code: internal_error; daemon config: {})",
                resp.served_config_id.as_deref().unwrap_or("unknown"),
            )
        });
        Some(Err(daemon_mcp_error(msg, resp.error_detail)))
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

fn daemon_launch_command(
    exe: &std::path::Path,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
) -> std::process::Command {
    // The binary is `kkernel`; the MCP server (and its daemon mode) live under
    // the `mcp` subcommand.
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("mcp")
        .arg("--daemon")
        .arg("--lifetime")
        .arg("demand");
    // A client-started daemon must not inherit a launcher incarnation claim
    // from an embedding process that happened to originate under supervision.
    #[cfg(unix)]
    cmd.env_remove(khive_runtime::daemon::SUPERVISOR_CLAIM_ENV);
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
    cmd
}

fn spawn_daemon_with_exe_and_config(
    exe: &std::path::Path,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
) -> std::io::Result<std::process::Child> {
    #[cfg(test)]
    SPAWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let mut cmd = daemon_launch_command(exe, config, db, packs);
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
    RequestExpired,
    PidFileDirectoryUntrusted(String),
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

#[derive(Debug, PartialEq, Eq)]
enum PidFileSnapshot {
    Missing,
    Present(Vec<u8>),
    Unreadable,
}

impl PidFileSnapshot {
    fn read(path: &std::path::Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => Self::Present(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Missing,
            Err(_) => Self::Unreadable,
        }
    }

    fn pid(&self) -> Option<u32> {
        match self {
            Self::Present(bytes) => std::str::from_utf8(bytes).ok()?.trim().parse().ok(),
            Self::Missing | Self::Unreadable => None,
        }
    }
}

/// Signal under the boot lock, but release it before waiting: the incumbent
/// needs that same lock to finish its own shutdown cleanup.
async fn kill_stale_daemon_inner(
    exit_timeout: std::time::Duration,
    boot_guard: Option<std::fs::File>,
) -> Result<PidFileSnapshot, RecoveryError> {
    #[cfg(test)]
    KILL_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let pid_file = pid_path();
    let expected_snapshot = match pid_file_directory_is_trusted_if_present(&pid_file) {
        Ok(true) => PidFileSnapshot::read(&pid_file),
        Ok(false) => PidFileSnapshot::Missing,
        Err(message) => return Err(RecoveryError::PidFileDirectoryUntrusted(message)),
    };
    let expected_pid = expected_snapshot.pid();

    let wait_for_exit = if let Some(pid) = expected_pid {
        match classify_pid_identity(pid) {
            PidIdentity::KhiveDaemon => {
                if let Ok(signed) = i32::try_from(pid) {
                    if signed > 0 {
                        #[cfg(test)]
                        SIGTERM_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        }
    } else {
        false
    };
    drop(boot_guard);
    if let Some(pid) = expected_pid {
        if wait_for_exit && !wait_for_process_exit(pid, exit_timeout).await {
            return Err(RecoveryError::IncumbentStillAlive { pid });
        }
    }

    Ok(expected_snapshot)
}

/// Remove `pid_file`/the daemon socket only if ownership has not changed since
/// `expected_snapshot` was observed: the PID file must be unchanged or absent,
/// and connecting to the socket must report absence or connection refusal.
/// Other connect errors leave ownership uncertain and refuse cleanup.
/// Either signal changing means a replacement daemon claimed the rendezvous
/// between the observation and this call, and unlinking would delete its live
/// paths instead of the truly-stale ones (#645).
fn remove_daemon_paths_if_still_stale(
    pid_file: &std::path::Path,
    expected_snapshot: &PidFileSnapshot,
) -> bool {
    let (current_snapshot, pid_directory_trusted) =
        match pid_file_directory_is_trusted_if_present(pid_file) {
            Ok(true) => (PidFileSnapshot::read(pid_file), true),
            Ok(false) => (PidFileSnapshot::Missing, false),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "daemon PID-file directory is not trusted; skipping stale-path cleanup"
                );
                return false;
            }
        };
    if matches!(expected_snapshot, PidFileSnapshot::Unreadable)
        || matches!(current_snapshot, PidFileSnapshot::Unreadable)
    {
        return false;
    }
    // Graceful incumbent cleanup may already have removed its own PID file.
    if current_snapshot != PidFileSnapshot::Missing && &current_snapshot != expected_snapshot {
        tracing::warn!(
            expected_pid = ?expected_snapshot.pid(),
            current_pid = ?current_snapshot.pid(),
            "pid file changed during stale-daemon cleanup — a replacement daemon \
             already claimed it; skipping unlink to avoid deleting its live paths"
        );
        return false;
    }

    let sock = socket_path();
    // A plain blocking connect is enough here: any success means *something*
    // is now listening at this path, which can only be a replacement daemon
    // that bound after our probe found the old one dead. Combined with the
    // PID-file recheck above, this closes the window even when the recovery
    // lock alone did not exclude the replacement's boot.
    match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(_) => {
            tracing::warn!(socket = ?sock, "live listener claimed rendezvous; skipping cleanup and launch");
            return false;
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(_) => return false,
    }

    if pid_directory_trusted {
        if let Err(error) = std::fs::remove_file(pid_file) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return false;
            }
        }
    }
    if let Err(error) = std::fs::remove_file(&sock) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return false;
        }
    }
    true
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

/// The kernel identity of a socket holder that returned a decoded khived
/// identity response. A mismatching configuration can still name a daemon;
/// the explicit handover acknowledgement is checked separately. Unlike
/// client recovery, the launcher does not require its own configuration id.
#[cfg(unix)]
#[derive(Debug)]
pub struct SupervisorDaemonPeer {
    pub pid: Option<u32>,
    pub uid: Option<u32>,
    pub protocol_version: u32,
    pub served_config_id: String,
    pub supervisor_claim: Option<String>,
    pub handover_accepted: bool,
}

#[cfg(unix)]
#[derive(Debug)]
pub enum SupervisorSocketProbe {
    Absent,
    Daemon(SupervisorDaemonPeer),
    Unidentified {
        pid: Option<u32>,
        uid: Option<u32>,
        reason: &'static str,
    },
}

/// Probe the launcher rendezvous without trusting a PID file or demanding an
/// identity match. Only a decoded daemon frame that explicitly reports its
/// protocol and served configuration can identify the connected holder.
/// Connect, write, and read share one timeout; an accepting silent/foreign
/// socket is never classified as absent.
#[cfg(unix)]
pub async fn probe_supervisor_socket(timeout: std::time::Duration) -> SupervisorSocketProbe {
    probe_supervisor_socket_inner(timeout, false).await
}

/// Keep the existing socket-owner refusal ahead of a contended store claim.
/// The caller holds the HOME boot lock; this read-only probe cannot open SQLite
/// or disturb the incumbent's rendezvous. Unknown peers still reach the normal
/// boot fence after the store claims have been acquired.
#[cfg(unix)]
pub async fn refuse_serving_socket_before_store_claim() -> anyhow::Result<()> {
    if let SupervisorSocketProbe::Daemon(peer) =
        probe_supervisor_socket(std::time::Duration::from_millis(500)).await
    {
        let holder = peer
            .pid
            .map_or_else(|| "unknown pid".to_string(), |pid| format!("pid {pid}"));
        anyhow::bail!(
            "refusing to start: a khived instance is already serving this socket {} ({holder})",
            socket_path().display()
        );
    }
    Ok(())
}

/// Ask the process on the connected socket to enter its own SIGTERM shutdown
/// path. The peer receives this on the same connection that supplies its
/// credentials and identity response, so no later numeric PID signal is sent.
#[cfg(unix)]
pub async fn request_supervisor_handover(timeout: std::time::Duration) -> SupervisorSocketProbe {
    probe_supervisor_socket_inner(timeout, true).await
}

#[cfg(unix)]
async fn probe_supervisor_socket_inner(
    timeout: std::time::Duration,
    handover: bool,
) -> SupervisorSocketProbe {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut stream =
        match tokio::time::timeout_at(deadline, UnixStream::connect(socket_path())).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return SupervisorSocketProbe::Absent;
            }
            Err(_) => {
                return SupervisorSocketProbe::Unidentified {
                    pid: None,
                    uid: None,
                    reason: "socket connection timed out",
                };
            }
            Ok(Err(_)) => {
                return SupervisorSocketProbe::Unidentified {
                    pid: None,
                    uid: None,
                    reason: "socket connection failed",
                };
            }
        };
    let credentials = stream.peer_cred().ok();
    let pid = credentials
        .as_ref()
        .and_then(|credentials| credentials.pid())
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    let uid = credentials.as_ref().map(|credentials| credentials.uid());
    if handover && (pid.is_none() || uid != Some(supervisor_effective_uid())) {
        return SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "handover requires a same-uid peer with a kernel pid",
        };
    }
    let probe = DaemonRequestFrame {
        probe_only: true,
        protocol_version: PROTOCOL_VERSION,
        // A mismatch is expected and useful: the answer still reports the
        // incumbent's real served configuration, without dispatching a verb.
        config_id: String::new(),
        ..Default::default()
    };
    let Ok(mut probe) = serde_json::to_value(&probe) else {
        return SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "identity probe could not be encoded",
        };
    };
    if handover {
        probe["supervisor_handover"] = serde_json::Value::Bool(true);
    }
    let Ok(payload) = serde_json::to_vec(&probe) else {
        return SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "identity probe could not be encoded",
        };
    };
    if !matches!(
        tokio::time::timeout_at(deadline, write_frame(&mut stream, &payload)).await,
        Ok(Ok(()))
    ) {
        return SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "identity probe write failed",
        };
    }
    let raw = match tokio::time::timeout_at(deadline, read_frame(&mut stream)).await {
        Ok(Ok(raw)) => raw,
        _ => {
            return SupervisorSocketProbe::Unidentified {
                pid,
                uid,
                reason: "identity probe received no daemon response",
            };
        }
    };
    let decoded: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(decoded) => decoded,
        Err(_) => {
            return SupervisorSocketProbe::Unidentified {
                pid,
                uid,
                reason: "identity probe response did not decode",
            };
        }
    };
    let reported_protocol = decoded
        .get("daemon_protocol_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok());
    let reported_config = decoded
        .get("served_config_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let supervisor_claim = decoded
        .get("supervisor_claim")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let handover_accepted = decoded
        .get("supervisor_handover_accepted")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let frame = serde_json::from_value::<DaemonResponseFrame>(decoded);
    match (frame, reported_protocol, reported_config) {
        (Ok(_frame), Some(protocol_version), Some(served_config_id)) => {
            SupervisorSocketProbe::Daemon(SupervisorDaemonPeer {
                pid,
                uid,
                protocol_version,
                served_config_id,
                supervisor_claim,
                handover_accepted,
            })
        }
        _ => SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "identity probe response omitted daemon identity",
        },
    }
}

#[cfg(unix)]
pub fn supervisor_pid_is_alive(pid: u32) -> bool {
    process_is_alive(pid)
}

#[cfg(unix)]
pub fn supervisor_effective_uid() -> u32 {
    // SAFETY: `geteuid` reads the process's kernel credential.
    unsafe { libc::geteuid() }
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
        plan: false,
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
    let probe_deadline = tokio::time::Instant::now() + deadline;
    let exchange = tokio::time::timeout_at(
        probe_deadline,
        try_forward_before(&probe, Some(probe_deadline)),
    )
    .await;
    // The inner connect/read timeout and this outer probe timeout can become
    // ready in the same scheduler turn. A timed-out inner NoSocket/ParseFailure
    // is uncertainty about a slow peer, never evidence that it is dead.
    if tokio::time::Instant::now() >= probe_deadline
        && !matches!(&exchange, Ok(ForwardOutcome::Response(_)))
    {
        return ProbeOutcome::Timeout;
    }
    match exchange {
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
                && resp.served_config_id.as_deref().is_some_and(|served| {
                    khive_runtime::daemon::config_ids_compatible(config_id, served)
                })
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
            | ForwardOutcome::ResponseLost
            | ForwardOutcome::ProtocolMismatch { .. },
        ) => ProbeOutcome::Dead,
        Ok(ForwardOutcome::RequestTooLarge { .. }) => ProbeOutcome::Timeout,
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
    confirm_genuinely_dead_with_round_observer(config_id, namespace, |_, _| {}).await
}

async fn confirm_genuinely_dead_with_round_observer(
    config_id: &str,
    namespace: &str,
    mut observe_round: impl FnMut(u32, &ProbeOutcome),
) -> ProbeOutcome {
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
        let outcome =
            quiesce_then_probe_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await;
        observe_round(round, &outcome);
        match outcome {
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

    // The bridge may have stopped waiting while this task waited for a peer
    // recoverer. Its detached task must not now classify or kill a daemon.
    if khive_storage::request_read_is_cancelled() {
        return Err(RecoveryError::RequestExpired);
    }

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
            let boot_lock = acquire_recovery_lock();
            match probe_daemon_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await {
                ProbeOutcome::Alive | ProbeOutcome::Timeout => return Ok(RecoveryOutcome::Skipped),
                ProbeOutcome::LockContended => return Ok(RecoveryOutcome::Uncertain),
                ProbeOutcome::Dead => {}
            }
            if khive_storage::request_read_is_cancelled() {
                return Err(RecoveryError::RequestExpired);
            }
            let expected_snapshot = kill_stale_daemon_inner(exit_timeout, boot_lock).await?;
            // Keep the recoverer-only lock throughout, but let graceful exit
            // take the boot lock. An independently started successor can win
            // while we wait, so recheck under the boot lock before unlink/spawn.
            let _boot_lock = acquire_recovery_lock();
            match probe_daemon_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await {
                ProbeOutcome::Alive | ProbeOutcome::Timeout => Ok(RecoveryOutcome::Skipped),
                ProbeOutcome::LockContended => Ok(RecoveryOutcome::Uncertain),
                ProbeOutcome::Dead => {
                    if !remove_daemon_paths_if_still_stale(&pid_path(), &expected_snapshot) {
                        return Ok(RecoveryOutcome::Uncertain);
                    }
                    launcher
                        .launch()
                        .map(RecoveryOutcome::Spawned)
                        .map_err(RecoveryError::Spawn)
                }
            }
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
    daemon_mcp_error(
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
    daemon_mcp_error(
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
    daemon_mcp_error(
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
    daemon_mcp_error(
        format!("daemon recovery refused: incumbent PID {pid} is still alive after the deadline"),
        Some(data),
    )
}

fn untrusted_pid_file_directory_error(message: String) -> McpError {
    tracing::error!(
        reason = "untrusted_pid_file_directory",
        error = %message,
        "daemon recovery refused because its PID-file directory is not trusted"
    );
    let mut data = serde_json::json!({
        "reason": "untrusted_pid_file_directory",
        "error": message,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(format!("daemon recovery refused: {message}"), Some(data))
}

/// A socket-less rendezvous claimed by a supervisor returns a retryable error
/// when the caller's startup budget ends. It never falls through to local
/// dispatch or spawns a competing daemon while the claim is held.
fn supervised_daemon_error(marker: &SupervisorMarker) -> McpError {
    let mut data = serde_json::json!({
        "reason": "supervised_daemon_starting",
        "retryable": true,
        "job": marker.job,
        "pid": marker.pid,
        "pid_alive": marker.pid_is_alive(),
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(
        format!(
            "supervised daemon starting: job \"{}\" (pid {}) has not bound the socket; \
             the request deadline expired or the request was cancelled before dispatch; retry",
            marker.job, marker.pid
        ),
        Some(data),
    )
}

/// No request bytes were sent while another launcher or client held the
/// marker lock. Retrying cannot duplicate dispatch.
fn supervisor_marker_lock_wait_error() -> McpError {
    let mut data = serde_json::json!({
        "reason": "supervised_daemon_starting",
        "retryable": true,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(
        "daemon startup ownership is in progress; the marker lock wait ended before dispatch; retry",
        Some(data),
    )
}

fn daemon_reconnect_expired_before_dispatch_error() -> McpError {
    daemon_mcp_error(
        "daemon reconnect deadline expired or request cancelled before dispatch; no lifecycle recovery attempted",
        Some(serde_json::json!({"reason": "daemon_reconnect_expired"})),
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
struct PendingSelfHeal {
    action: MismatchRecovery,
    executable: Option<std::path::PathBuf>,
}

static PENDING_SELF_HEAL: std::sync::Mutex<Option<PendingSelfHeal>> = std::sync::Mutex::new(None);

fn arm_executable_self_heal(executable: std::path::PathBuf) {
    *PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PendingSelfHeal {
        action: MismatchRecovery::ReexecScheduled,
        executable: Some(executable),
    });
}

fn arm_pending_self_heal(action: MismatchRecovery) {
    let mut slot = PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some(PendingSelfHeal {
        action,
        executable: None,
    });
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
        Some(PendingSelfHeal {
            action: MismatchRecovery::ReexecScheduled,
            executable,
        }) => reexec_in_place(executable),
        Some(PendingSelfHeal {
            action: MismatchRecovery::DrainAndExit,
            ..
        }) => exit_process(),
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

/// Perform the actual re-exec: use the recorded path for an executable
/// replacement, or resolve the on-disk binary at *exec time* via
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
fn reexec_in_place(executable: Option<std::path::PathBuf>) {
    use std::os::unix::process::CommandExt;

    let exe = match executable.map(Ok).unwrap_or_else(std::env::current_exe) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "bridge self-heal re-exec failed: could not resolve current_exe");
            return;
        }
    };
    // Identity-triggered re-execs can recur across installs; retain one marker
    // so protocol mismatches still apply the existing resumed loop-breaker.
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
fn reexec_in_place(_executable: Option<std::path::PathBuf>) {
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
            return BootFenceOutcome::HardError(daemon_mcp_error(
                format!(
                    "failed to acquire daemon boot/recovery lock while waiting for \
                     cold-boot quiescence: {e}"
                ),
                None,
            ));
        }
        Err(e) => {
            return BootFenceOutcome::HardError(daemon_mcp_error(
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
        ProbeOutcome::Timeout => BootFenceOutcome::HardError(daemon_mcp_error(
            "daemon state uncertain after cold-boot quiescence; not falling back to \
             local dispatch to avoid racing a possibly still-initializing index",
            None,
        )),
        // `probe_daemon_identity` (unlike `quiesce_then_probe_identity`) never
        // constructs `LockContended` — it has no lock-acquisition step of its
        // own. Handled here only for match exhaustiveness over the shared
        // `ProbeOutcome` type; same fail-safe HardError as `Timeout` if it
        // were ever reached.
        ProbeOutcome::LockContended => BootFenceOutcome::HardError(daemon_mcp_error(
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
/// This conservative entry point never replays a fully written frame.
/// The CLI/MCP policy-aware entry points additionally permit one classified
/// read replay after EOF/reset. A `NoSocket` outcome never writes anything,
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
    let replay_read_only = !frame.probe_only
        && !frame.metrics_only
        && !frame.plan
        && crate::request_policy::read_replay_safe(&frame.ops);
    forward_or_spawn_with_replay_policy(frame, config, db, packs, replay_read_only).await
}

pub(crate) async fn forward_or_spawn_with_replay_policy(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
    replay_read_only: bool,
) -> Option<Result<String, McpError>> {
    #[cfg(any(test, feature = "test-forward-seam"))]
    if let Some(intercepted) = test_forward_seam::intercept(frame, packs) {
        return intercepted;
    }
    let spawn = || {
        let exe = std::env::current_exe()?;
        spawn_daemon_with_exe_and_config(&exe, config, db, packs)
    };
    forward_or_spawn_with_policy(frame, &spawn, replay_read_only).await
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
    forward_or_spawn_with_policy(frame, spawn, false).await
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SupervisorDiscoveryPoint {
    UnmanagedRetry,
    BeforeRecovery,
    RecoveryAdmission,
    SupervisorWait,
}

#[cfg(test)]
type SupervisorDiscoveryHook = (SupervisorDiscoveryPoint, Box<dyn FnOnce() + Send>);

#[cfg(test)]
static SUPERVISOR_DISCOVERY_HOOK: std::sync::Mutex<Option<SupervisorDiscoveryHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn supervisor_discovery_hook(point: SupervisorDiscoveryPoint) {
    let action = {
        let mut hook = SUPERVISOR_DISCOVERY_HOOK.lock().expect("discovery hook");
        if hook.as_ref().is_some_and(|(at, _)| *at == point) {
            hook.take().map(|(_, action)| action)
        } else {
            None
        }
    };
    if let Some(action) = action {
        action();
    }
}

async fn forward_or_spawn_with_policy<F>(
    frame: &DaemonRequestFrame,
    spawn: &F,
    replay_read_only: bool,
) -> Option<Result<String, McpError>>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    if env_truthy("KHIVE_NO_DAEMON") {
        return None;
    }

    let request_started = tokio::time::Instant::now();
    let mut replay = ReadReplayBudget::new(
        replay_read_only && crate::request_policy::read_replay_safe(&frame.ops),
    );
    let mut first = try_forward_with_read_replay(frame, &mut replay, None).await;
    let mut initial_supervisor_marker: Option<SupervisorMarker> = None;
    let mut degraded_bootstrap = false;
    if matches!(first, ForwardOutcome::NoSocket) {
        if let Some(marker) = read_supervisor_marker() {
            let budget = initial_supervisor_marker.get_or_insert(marker).clone();
            let waited =
                match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                    Ok(outcome) => outcome,
                    Err(error) => return Some(Err(error)),
                };
            degraded_bootstrap |= waited.degraded_bootstrap;
            first = waited.outcome;
        } else if recorded_daemon_is_alive() {
            // Unmanaged daemon handover retains its existing reconnect grace.
            let deadline = bounded_retry_deadline();
            while matches!(first, ForwardOutcome::NoSocket)
                && tokio::time::Instant::now() < deadline
                && !khive_storage::request_read_is_cancelled()
            {
                sleep_until_retry(deadline).await;
                #[cfg(test)]
                supervisor_discovery_hook(SupervisorDiscoveryPoint::UnmanagedRetry);
                if let Some(marker) = read_supervisor_marker() {
                    let budget = initial_supervisor_marker.get_or_insert(marker).clone();
                    let waited = match wait_for_supervisor(
                        frame,
                        &mut replay,
                        request_started,
                        budget,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => return Some(Err(error)),
                    };
                    degraded_bootstrap |= waited.degraded_bootstrap;
                    first = waited.outcome;
                    break;
                }
                if tokio::time::Instant::now() >= deadline
                    || khive_storage::request_read_is_cancelled()
                {
                    break;
                }
                first = try_forward_with_read_replay(frame, &mut replay, Some(deadline)).await;
            }
        }
    }
    // A claim may arrive at the end of unmanaged grace or without a PID file.
    // Never restart a supervisor budget already consumed by this request.
    if matches!(first, ForwardOutcome::NoSocket) && !degraded_bootstrap {
        #[cfg(test)]
        supervisor_discovery_hook(SupervisorDiscoveryPoint::BeforeRecovery);
        if let Some(marker) = read_supervisor_marker() {
            let budget = initial_supervisor_marker.get_or_insert(marker).clone();
            let waited =
                match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                    Ok(outcome) => outcome,
                    Err(error) => return Some(Err(error)),
                };
            degraded_bootstrap |= waited.degraded_bootstrap;
            first = waited.outcome;
        }
    }
    let marker_guard = loop {
        if matches!(first, ForwardOutcome::NoSocket)
            && (khive_storage::request_read_is_cancelled()
                || khive_storage::capture_request_read_context()
                    .deadline()
                    .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at()))
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        match first {
            ForwardOutcome::Response(resp) => {
                return map_response(*resp, &frame.config_id, &frame.namespace)
            }
            ForwardOutcome::NoSocket => {
                // No claim yet, or its bounded startup grace expired.
            }
            ForwardOutcome::Unreachable {
                kind,
                os_error_code,
            } => return Some(Err(daemon_unreachable_error(frame, kind, os_error_code))),
            ForwardOutcome::ParseFailure | ForwardOutcome::ResponseLost => {
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
            ForwardOutcome::ProtocolMismatch {
                daemon_protocol_version,
            } => {
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
                return Some(Err(protocol_mismatch_error(
                    protocol_mismatch_message(daemon_protocol_version),
                    None,
                )));
            }
            ForwardOutcome::RequestTooLarge { bytes } => {
                return Some(Err(request_too_large_error(bytes)));
            }
        }

        // This is after the last unlocked marker read and inside recovery
        // admission. A launcher that publishes first holds the same lock.
        #[cfg(test)]
        supervisor_discovery_hook(SupervisorDiscoveryPoint::RecoveryAdmission);
        let guard = match acquire_supervisor_marker_lock().await {
            Ok(guard) => guard,
            Err(error) => {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                ) {
                    return Some(Err(supervisor_marker_lock_wait_error()));
                }
                tracing::warn!(error = %error, "supervisor marker lock unavailable; suppressing lifecycle recovery");
                return Some(Err(daemon_mcp_error(
                    "supervisor marker ownership could not be established; retry the request",
                    Some(serde_json::json!({"reason": "supervisor_marker_lock_unavailable"})),
                )));
            }
        };
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if let Some(marker) = read_supervisor_marker() {
            if !degraded_bootstrap {
                drop(guard);
                let budget = initial_supervisor_marker.get_or_insert(marker).clone();
                let waited =
                    match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                        Ok(outcome) => outcome,
                        Err(error) => return Some(Err(error)),
                    };
                degraded_bootstrap |= waited.degraded_bootstrap;
                first = waited.outcome;
                continue;
            }
        }
        // The first supervisor's bounded grace has elapsed, or the locked
        // read found no declaration. Rewrites never restart that first bound.
        // Keep the lock through this client's spawn and readiness wait, never
        // through a supervisor wait (whose launcher must reacquire it).
        break guard;
    };

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
    // Only a client that actually spawns keeps the marker lock through its
    // ADR-049 readiness wait. A skipped/uncertain recovery did not acquire a
    // child to protect and must let a waiting launcher publish immediately.
    let mut marker_guard = Some(marker_guard);
    let recovery = kill_and_respawn(&frame.config_id, &frame.namespace, spawn).await;
    match recovery {
        Err(RecoveryError::RequestExpired) => {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
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
        Err(RecoveryError::PidFileDirectoryUntrusted(message)) => {
            return Some(Err(untrusted_pid_file_directory_error(message)));
        }
        Ok(RecoveryOutcome::Skipped) => {
            // A concurrent client already has a live matching daemon ready.
            drop(marker_guard.take());
        }
        Ok(RecoveryOutcome::Spawned(child)) => {
            // Give the kernel a moment to release the socket path and let the
            // spawned daemon process start.
            spawned_child = Some(child);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(RecoveryOutcome::Uncertain) => {
            drop(marker_guard.take());
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

    // Send the real frame now that a daemon is confirmed ready
    // (or believed ready via Skipped). The connect attempt inside
    // `try_forward_before` doubles as the readiness check — a `NoSocket`
    // outcome here just means "not listening yet" (nothing written), so keep
    // retrying. Only the explicit read policy may replay a lost response;
    // every other post-write outcome is terminal and returned immediately.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut boot_fence_confirmed = false;
    loop {
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if tokio::time::Instant::now() >= deadline {
            if boot_fence_confirmed {
                return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
            }
            // #667: a bare timeout here does not mean "no daemon" — it may
            // mean "daemon is still inside cold-boot schema init". Wait for
            // that boot (if any) to quiesce and re-probe before deciding.
            match wait_for_boot_quiescence_then_reprobe(frame).await {
                BootFenceOutcome::DaemonReady => {
                    // This 500 ms identity probe already confirmed readiness.
                    // Release the launcher's marker lock before dispatch;
                    // repeating the shorter 100 ms probe can otherwise loop
                    // forever after the five-second retry bound.
                    drop(marker_guard.take());
                    boot_fence_confirmed = true;
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
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if spawned_child.is_some() && marker_guard.is_some() {
            // The launcher may be waiting for this marker lock. Release it as
            // soon as our daemon answers an identity probe, before the real
            // request is dispatched (which may run for much longer).
            if matches!(
                probe_daemon_identity(&frame.config_id, &frame.namespace, 100).await,
                ProbeOutcome::Alive
            ) {
                drop(marker_guard.take());
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        }
        match try_forward_with_read_replay(frame, &mut replay, None).await {
            ForwardOutcome::Response(resp) => {
                return map_response(*resp, &frame.config_id, &frame.namespace)
            }
            ForwardOutcome::RequestTooLarge { bytes } => {
                return Some(Err(request_too_large_error(bytes)));
            }
            ForwardOutcome::ParseFailure | ForwardOutcome::ResponseLost => {
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
            ForwardOutcome::ProtocolMismatch {
                daemon_protocol_version,
            } => {
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
                return Some(Err(protocol_mismatch_error(
                    protocol_mismatch_message(daemon_protocol_version),
                    None,
                )));
            }
            ForwardOutcome::Unreachable {
                kind,
                os_error_code,
            } => return Some(Err(daemon_unreachable_error(frame, kind, os_error_code))),
            ForwardOutcome::NoSocket => {
                if boot_fence_confirmed {
                    return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Let sibling unit tests compose the real frame mapping with a captured dispatch.
#[cfg(all(unix, test))]
pub(crate) fn map_response_for_test(
    response: DaemonResponseFrame,
    expected_config_id: &str,
    namespace: &str,
) -> Option<Result<String, McpError>> {
    map_response(response, expected_config_id, namespace)
}

#[cfg(test)]
mod demand_launch_tests;

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
