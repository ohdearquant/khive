use super::*;

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
    pub(super) fn severity(self) -> FallbackSeverity {
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
pub(super) enum FallbackSeverity {
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
pub(super) fn is_daemon_strict_mode() -> bool {
    env_truthy("KHIVE_DAEMON_STRICT")
}

/// `khive_daemon_fallback_total{reason="config_mismatch"}`
static FALLBACK_CONFIG_MISMATCH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// `khive_daemon_fallback_total{reason="namespace_mismatch"}`
static FALLBACK_NAMESPACE_MISMATCH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// `khive_daemon_fallback_total{reason="no_socket"}`
pub(super) static FALLBACK_NO_SOCKET: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// `khive_daemon_fallback_total{reason="parse_failure"}`
pub(super) static FALLBACK_PARSE_FAILURE: std::sync::atomic::AtomicUsize =
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

pub(super) fn first_config_mismatch_field(client: &str, daemon: Option<&str>) -> &'static str {
    khive_runtime::daemon::first_config_mismatch_field(client, daemon)
}

pub(super) fn opaque_config_id(config_id: &str) -> String {
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
pub(super) fn record_fallback(
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
            target: "khive_mcp::daemon",
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
            target: "khive_mcp::daemon",
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
pub(super) fn fallback_or_reject(
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
