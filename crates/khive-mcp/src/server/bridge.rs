//! MCP request cancellation, planning, daemon forwarding, and bridge diagnostics.

use std::future::Future;

use khive_request::{parse_request, ArgValue, ExecutionMode, ParsedRequest};
use khive_runtime::{
    present_with_policy, render_format, DomainDisposition, OutputFormat, PresentationMode,
    VerbPresentationPolicy,
};
use rmcp::ErrorData as McpError;
use serde_json::{json, Value};

use crate::tools::request::RequestParams;

#[cfg(not(unix))]
use super::bridge_instance_id;
#[cfg(unix)]
use super::{aborted_entry, batch_status, cancelled_forward_error, failure_entry};
use super::{
    dsl_err_to_mcp, error_with_disposition, invalid_request_error, parse_output_format,
    parse_presentation_mode, serialize_response_value, validate_request_overrides, KhiveMcpServer,
};

// ── single MCP tool ─────────────────────────────────────────────────────────

pub(super) fn request_read_timeout() -> std::time::Duration {
    static TIMEOUT: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        // The storage resolver owns the accepted range and warns on a corrected value.
        let timeout = khive_storage::request_read_timeout_from_env();
        tracing::info!(
            target: "khive::config_lock",
            name = "KHIVE_REQUEST_READ_TIMEOUT_SECS",
            value = timeout.as_secs(),
        );
        timeout
    })
}

/// Ensure every MCP request attempt has a daemon/audit correlation id.
///
/// A caller-supplied id is an explicit retry/correlation key and therefore
/// wins unchanged. Otherwise the bridge mints an opaque nonzero 64-bit id from
/// UUID entropy before daemon forwarding or local fallback. The same value is
/// then echoed by the daemon frame and stamped into every operation's audit
/// resource, making a handler that disappears after admission observable.
pub(super) fn ensure_bridge_request_id(params: &mut RequestParams) -> u64 {
    if let Some(request_id) = params.request_id {
        return request_id;
    }

    let bytes = uuid::Uuid::new_v4().into_bytes();
    let high = u64::from_be_bytes(
        bytes[..8]
            .try_into()
            .expect("UUID high half is eight bytes"),
    );
    let low = u64::from_be_bytes(bytes[8..].try_into().expect("UUID low half is eight bytes"));
    let request_id = (high ^ low).max(1);
    params.request_id = Some(request_id);
    request_id
}

#[cfg(test)]
pub(super) async fn scope_mcp_request_read_cancellation<F>(
    cancellation: tokio_util::sync::CancellationToken,
    future: F,
) -> F::Output
where
    F: Future,
{
    scope_mcp_request_read_cancellation_with_timeout(cancellation, request_read_timeout(), future)
        .await
}

pub(super) async fn scope_mcp_request_read_cancellation_with_timeout<F>(
    cancellation: tokio_util::sync::CancellationToken,
    timeout: std::time::Duration,
    future: F,
) -> F::Output
where
    F: Future,
{
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    // Seed synchronously: a pre-cancelled rmcp context must be visible even
    // when the wrapped request future is ready before the bridge task's first
    // poll.
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(cancellation.is_cancelled());
    let _bridge = AbortOnDrop(tokio::spawn(async move {
        cancellation.cancelled().await;
        let _ = cancel_tx.send(true);
    }));
    khive_storage::scope_request_read_cancellation(
        cancel_rx,
        khive_storage::scope_request_read_deadline(timeout, future),
    )
    .await
}

/// Owned boxed future returned by the daemon-forwarding seam.
///
/// Ownership is deliberate: once daemon forwarding is admitted the exchange
/// runs in its own task, so dropping the outer MCP handler cannot drop the
/// socket future after the daemon may already have committed work.
#[cfg(unix)]
pub(super) type ForwardFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Option<Result<String, McpError>>> + Send + 'static>,
>;

/// Function pointer type for the daemon-forwarding seam, parameterized so
/// tests can inject a spy in place of the real `forward_or_spawn_with_config_and_packs`
/// call — the real call spawns/contacts an actual daemon process, which
/// tests must not do. `packs` mirrors `forward_or_spawn_with_config_and_packs`'s own
/// optional `packs` argument exactly: the `Some`/`None` decision is made by
/// the shared call site in `request_with_forward`, not inside the adapter,
/// so a spy standing in for this seam observes the same optional argument
/// the real function would receive. `config`/`db` are always `None` at this
/// call site.
#[cfg(unix)]
pub(super) type ForwardFnPtr =
    fn(khive_runtime::DaemonRequestFrame, Option<Vec<String>>, bool) -> ForwardFuture;

/// Adapts the real `forward_or_spawn_with_replay_policy` to the `ForwardFnPtr`
/// signature. A pure pass-through — the `Some`/`None` decision already
/// happened at the call site — so this boundary carries no logic a test
/// spy could fail to observe.
#[cfg(unix)]
fn forward_or_spawn_boxed(
    frame: khive_runtime::DaemonRequestFrame,
    packs: Option<Vec<String>>,
    replay_read_only: bool,
) -> ForwardFuture {
    Box::pin(async move {
        crate::daemon::forward_or_spawn_with_replay_policy(
            &frame,
            None,
            None,
            packs.as_deref(),
            replay_read_only,
        )
        .await
    })
}

impl KhiveMcpServer {
    pub(crate) fn plan_ops(&self, ops: &str) -> String {
        let catalog = self
            .registry
            .all_verbs_with_names()
            .into_iter()
            .map(|(pack, handler)| (handler.name.to_string(), pack.to_string()))
            .chain(
                self.registry
                    .mounted_verb_snapshot()
                    .into_iter()
                    .map(|verb| {
                        (
                            verb["verb"].as_str().unwrap_or_default().to_owned(),
                            verb["pack"].as_str().unwrap_or_default().to_owned(),
                        )
                    }),
            )
            .collect();
        khive_request::plan_request(ops, &catalog).to_string()
    }

    fn plan_bridge_ops(&self, ops: &str) -> String {
        let mut plan: Value =
            serde_json::from_str(&self.plan_ops(ops)).expect("plan output is always JSON");
        if let Some(stages) = plan.get_mut("stages").and_then(Value::as_array_mut) {
            for stage in stages {
                if stage["verb"] == "bridge.diagnostics" {
                    stage["known"] = Value::Bool(true);
                    stage["pack"] = json!("bridge-control");
                }
            }
        }
        plan.to_string()
    }

    fn bridge_diagnostics_response(
        &self,
        p: &RequestParams,
        parsed: &ParsedRequest,
    ) -> Result<String, McpError> {
        if !self.stdio_bridge {
            return Err(invalid_request_error(
                "bridge.diagnostics is available only on the stdio bridge".into(),
            ));
        }
        if parsed.mode != ExecutionMode::Single || parsed.ops.len() != 1 {
            return Err(invalid_request_error(
                "bridge.diagnostics must be the only operation in an unbatched request".into(),
            ));
        }
        if p.save_to.is_some() {
            return Err(invalid_request_error(
                "bridge.diagnostics does not accept save_to".into(),
            ));
        }
        let op = &parsed.ops[0];
        let is_help = op.args.len() == 1
            && matches!(
                op.args.get("help"),
                Some(ArgValue::Value(Value::Bool(true)))
            );
        if !op.args.is_empty() && !is_help {
            return Err(invalid_request_error(
                "bridge.diagnostics accepts only help=true".into(),
            ));
        }
        validate_request_overrides(p, 1)?;
        if khive_storage::request_read_is_cancelled() {
            return Err(McpError::internal_error(
                "request cancelled before bridge diagnostics read",
                Some(error_with_disposition(
                    json!({"kind":"cancelled", "message":"request cancelled before bridge diagnostics read"}),
                    DomainDisposition::NotCommitted,
                )),
            ));
        }

        let result = if is_help {
            bridge_diagnostics_help()
        } else {
            #[cfg(unix)]
            {
                bridge_diagnostics_value().ok_or_else(|| {
                    McpError::internal_error(
                        "bridge fallback total overflow",
                        Some(error_with_disposition(
                            json!({"kind":"counter_overflow", "message":"bridge fallback total overflow"}),
                            DomainDisposition::NotCommitted,
                        )),
                    )
                })?
            }
            #[cfg(not(unix))]
            {
                bridge_diagnostics_value()
            }
        };
        let presentation =
            parse_presentation_mode(p.presentation.as_deref()).map_err(invalid_request_error)?;
        let presentation_per_op: Option<Vec<Option<PresentationMode>>> =
            p.presentation_per_op.as_ref().map(|modes| {
                modes
                    .iter()
                    .map(|mode| {
                        mode.as_deref().map(|mode| {
                            parse_presentation_mode(Some(mode))
                                .expect("presentation override was validated")
                        })
                    })
                    .collect()
            });
        let effective_presentation = presentation_per_op
            .as_ref()
            .and_then(|modes| modes.first())
            .and_then(|mode| *mode)
            .unwrap_or(presentation);
        let result = present_with_policy(
            result,
            effective_presentation,
            chrono::Utc::now().timestamp(),
            VerbPresentationPolicy::Standard,
        );
        let batch_format = parse_output_format(p.format.as_deref())
            .map_err(invalid_request_error)?
            .unwrap_or(self.default_output_format);
        let format_per_op: Option<Vec<Option<OutputFormat>>> =
            p.format_per_op.as_ref().map(|formats| {
                formats
                    .iter()
                    .map(|format| {
                        parse_output_format(format.as_deref())
                            .expect("format override was validated")
                    })
                    .collect()
            });
        let effective_format = format_per_op
            .as_ref()
            .and_then(|formats| formats.first())
            .and_then(|format| *format)
            .unwrap_or(batch_format);
        let formatted = if effective_format == OutputFormat::Json {
            khive_runtime::presentation::prepare_format_value(
                result,
                effective_format,
                effective_presentation,
            )
        } else {
            Value::String(render_format(
                result,
                effective_format,
                effective_presentation,
            ))
        };
        Ok(serialize_response_value(&json!({
            "results": [{"ok": true, "tool": "bridge.diagnostics", "result": formatted}],
            "summary": {"total": 1, "succeeded": 1, "failed": 0, "aborted": 0},
            "status": "success",
        })))
    }

    pub(super) fn plan_response(&self, p: &RequestParams) -> Result<Option<String>, McpError> {
        if p.plan != Some(true) {
            return Ok(None);
        }
        p.validate_plan_envelope()?;
        Ok(Some(self.plan_ops(&p.ops)))
    }

    pub(super) async fn request_with_cancellation(
        &self,
        p: RequestParams,
    ) -> Result<String, McpError> {
        let parsed = parse_request(&p.ops);
        if let Ok(ref parsed) = parsed {
            if parsed_contains_bridge_diagnostics(parsed) {
                if p.plan == Some(true) && self.stdio_bridge {
                    p.validate_plan_envelope()?;
                    return Ok(self.plan_bridge_ops(&p.ops));
                }
                if p.plan != Some(true) {
                    return self.bridge_diagnostics_response(&p, parsed);
                }
            }
        }
        #[cfg(unix)]
        if let Some(executable) = &self.bridge_executable {
            executable
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .check()?;
        }
        if let Some(plan) = self.plan_response(&p)? {
            return Ok(plan);
        }
        parsed.map_err(dsl_err_to_mcp)?;
        let mut p = p;
        let request_id = ensure_bridge_request_id(&mut p);
        tracing::debug!(
            target: "khive_mcp::server",
            request_id,
            "MCP request admitted with bridge correlation id"
        );
        #[cfg(unix)]
        return self.request_with_forward(p, forward_or_spawn_boxed).await;
        #[cfg(not(unix))]
        return self.request_with_forward(p).await;
    }

    /// Inner implementation of `request_with_cancellation`, parameterized
    /// over the daemon-forwarding seam so tests can drive the real dispatch
    /// path (registry resolution included) while asserting on what would
    /// have reached `forward_or_spawn_with_config_and_packs`, without spawning or
    /// contacting a real daemon.
    pub(super) async fn request_with_forward(
        &self,
        p: RequestParams,
        #[cfg(unix)] forward_fn: ForwardFnPtr,
    ) -> Result<String, McpError> {
        if let Some(plan) = self.plan_response(&p)? {
            return Ok(plan);
        }
        // Parse before the daemon decision. The daemon protocol's historical
        // error channel is string-only, so forwarding malformed DSL would turn
        // `invalid_params` plus its structured `parse-error` reason into an
        // untyped `internal_error` on the bridge back to MCP. This cheap,
        // side-effect-free preflight keeps the local and warm-daemon surfaces on
        // the same RPC contract; valid requests are still parsed authoritatively
        // inside `dispatch_request_inner` at the dispatch seam.
        let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;
        if parsed_contains_bridge_diagnostics(&parsed) {
            return self.bridge_diagnostics_response(&p, &parsed);
        }
        validate_request_overrides(&p, parsed.ops.len())?;
        #[cfg(unix)]
        let replay_read_only = !parsed.ops.is_empty()
            && parsed
                .ops
                .iter()
                .all(|op| self.registry.is_read_replay_safe(&op.tool));
        #[cfg(not(unix))]
        let _ = parsed;

        // Forward to the warm daemon when reachable, auto-spawning it
        // on first use. An ordinary no-socket condition, a namespace
        // mismatch, or KHIVE_NO_DAEMON falls through to local dispatch.
        // A confirmed respawn failure (spawn error, or the child exits
        // before binding the socket) instead returns a caller-visible
        // `respawn_failed` error without local dispatch, per ADR-049
        // Amendment 2.
        //
        // MCP-AUD-002: the daemon wire frame has no `save_to` field, so
        // daemon-forwarded requests silently drop the sink and return the
        // inline result instead. Bypass daemon forwarding whenever `save_to`
        // is set so the local path's manifest/file behavior always applies,
        // matching the existing `kkernel exec --save-file` precedent.
        // A request cancelled before daemon admission must not start new
        // work — this must hold regardless of whether the request would go
        // through daemon forwarding or straight to local dispatch via the
        // `save_to` bypass below, so the check runs before either branch is
        // chosen. After admission, however, cancellation cannot safely drop
        // the forward: the daemon may already have committed one or more
        // operations, so the bridge must preserve its real response.
        #[cfg(unix)]
        if khive_storage::request_read_is_cancelled() {
            return Err(McpError::internal_error(
                "request cancelled before daemon dispatch",
                Some(error_with_disposition(
                    json!({"kind":"cancelled", "message":"request cancelled before daemon dispatch"}),
                    DomainDisposition::NotCommitted,
                )),
            ));
        }
        // In-memory state belongs to this runtime. Even a daemon with the same
        // configuration would own a separate store, so keep these requests local.
        #[cfg(unix)]
        let in_memory = self
            .runtime
            .as_ref()
            .is_some_and(|runtime| !runtime.backend().is_file_backed());
        #[cfg(unix)]
        if p.save_to.is_none() && !in_memory {
            let frame = self.wire_daemon_frame(&p);
            let request_id = frame.request_id;
            // Forward this server's own resolved pack list so a daemon this
            // call spawns serves the SAME packs this process registered —
            // `pack_names()` reflects the actual loaded registry regardless
            // of whether that selection came from `--pack`, `KHIVE_PACKS`,
            // or a discovered `[runtime].packs` config entry, none of which
            // otherwise reach a freshly spawned child (khive-oss#1941).
            let resolved_packs: Vec<String> = self
                .registry
                .pack_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            // Captured at the moment the forward is admitted so the
            // post-cancellation wait below can bound itself by the time
            // actually left on the REQUEST's own deadline, not by a fresh
            // full ceiling starting from whenever cancellation happens to
            // arrive. Also propagated into the spawned task itself
            // (`inherit_request_read_context`) so the forwarding path's
            // socket-exchange deadline is this exact same instant, instead
            // of a second, independently-ticking relative timer.
            let post_cancellation_deadline = khive_storage::capture_request_read_context()
                .deadline()
                .map(khive_storage::RequestReadDeadline::async_at);
            let mut forward_task =
                tokio::spawn(khive_storage::inherit_request_read_context(async move {
                    let outcome = forward_fn(frame, Some(resolved_packs), replay_read_only).await;
                    tracing::debug!(
                        target: "khive_mcp::server",
                        request_id,
                        daemon_outcome_present = outcome.is_some(),
                        "admitted daemon forward reached a terminal outcome"
                    );
                    outcome
                }));
            let mut cancelled_during_forward = false;
            let forwarded = tokio::select! {
                result = &mut forward_task => result,
                _ = khive_storage::wait_for_request_read_cancellation() => {
                    cancelled_during_forward = true;
                    tracing::warn!(
                        target: "khive_mcp::server",
                        request_id,
                        "request cancellation arrived after daemon admission; shielding the \
                         forward until its per-operation outcome is known"
                    );
                    // Shielding the forward must still be bounded: a stalled
                    // daemon or a silent same-UID socket peer must not keep
                    // this handler (and the task awaiting it) alive forever.
                    // Wait only for whatever time remains on the request's
                    // own absolute deadline (captured above, before this
                    // select ever ran) rather than starting a fresh
                    // `request_read_timeout()` ceiling from the moment
                    // cancellation happens to arrive — a request cancelled a
                    // moment before its own deadline must not then hold this
                    // handler for almost another full ceiling. When no
                    // deadline was installed (a direct `request_with_forward`
                    // call outside `scope_mcp_request_read_cancellation`, as
                    // some tests do), fall back to `request_read_timeout()`.
                    // If the deadline had already passed by the time
                    // cancellation arrived, `timeout_at` elapses on its very
                    // next poll — an immediate unknown-outcome return, which
                    // is the only consistent reading: there is no time left
                    // to shield the forward with.
                    let wait_result = match post_cancellation_deadline {
                        Some(deadline) => {
                            tokio::time::timeout_at(deadline, &mut forward_task).await
                        }
                        None => {
                            tokio::time::timeout(request_read_timeout(), &mut forward_task).await
                        }
                    };
                    match wait_result
                    {
                        Ok(result) => result,
                        Err(_elapsed) => {
                            // Never abort the task: the daemon may already
                            // have committed. Just stop waiting on it here —
                            // dropping the `JoinHandle` detaches without
                            // cancelling, so the forward keeps running to
                            // completion on its own.
                            tracing::warn!(
                                target: "khive_mcp::server",
                                request_id,
                                "admitted daemon forward did not resolve within the \
                                 post-cancellation wait bound; reporting an unknown \
                                 outcome and leaving the forward to finish on its own"
                            );
                            return Err(cancelled_forward_error(request_id));
                        }
                    }
                }
            };
            let forwarded = forwarded.map_err(|error| {
                McpError::internal_error(
                    format!(
                        "daemon forwarding task failed after admission ({error}); outcome is \
                         unknown and the request must not be retried blindly"
                    ),
                    Some(json!({
                        "outcome": "unknown",
                        "kind": "transport",
                        "message": format!("daemon forwarding task failed after admission ({error}); outcome is unknown and the request must not be retried blindly"),
                                    "domain_disposition": "unknown",
                        "retryable": false,
                        "request_id": request_id,
                    })),
                )
            })?;
            if let Some(res) = forwarded {
                return match res {
                    Ok(s) => Ok(s),
                    // #947/#898: a strict-mode pre-dispatch rejection is
                    // tagged with
                    // `daemon::STRICT_FALLBACK_MARKER` so it can be reshaped
                    // into the normal per-op envelope instead of surfacing as
                    // an RPC-level error. Every other daemon-forward error
                    // (non-strict respawn failure, protocol mismatch,
                    // oversized frame, ambiguous post-write outcome) is
                    // untagged and passes through unchanged.
                    Err(e) => match strict_fallback_reason(&e) {
                        Some(reason) => strict_fallback_envelope_response(&p, reason),
                        None => Err(e),
                    },
                };
            }
            // The forward produced no daemon-side outcome (e.g. no socket,
            // `KHIVE_NO_DAEMON`). `tokio::select!` is unbiased, so the
            // task-result arm can win with `None` in the same instant
            // cancellation becomes visible — `cancelled_during_forward` alone
            // would miss that race. Re-check the flag directly so local
            // dispatch never runs after an admitted cancellation regardless
            // of which arm happened to win.
            if cancelled_during_forward || khive_storage::request_read_is_cancelled() {
                return Err(McpError::internal_error(
                    "request cancelled before local fallback dispatch",
                    Some(json!({
                        "outcome": "not_dispatched",
                        "kind": "cancelled",
                        "message": "request cancelled before local fallback dispatch",
                        "domain_disposition": "not_committed",
                        "retryable": true,
                        "request_id": request_id,
                    })),
                ));
            }
        }
        self.dispatch_request_wire(p).await
    }
}

#[cfg(unix)]
fn bridge_diagnostics_value() -> Option<Value> {
    crate::daemon::bridge_diagnostics_snapshot()
        .map(|snapshot| serde_json::to_value(snapshot).expect("bridge snapshot is serializable"))
}

#[cfg(not(unix))]
fn bridge_diagnostics_value() -> Value {
    json!({
        "bridge_instance_id": bridge_instance_id(),
        "pid": std::process::id(),
        "fallback_reasons": {
            "config_mismatch": 0,
            "namespace_mismatch": 0,
            "no_socket": 0,
            "parse_failure": 0,
            "protocol_mismatch": 0,
        },
        "fallback_total": 0,
        "strict_violations": 0,
        "daemon_slow": {"before_dispatch": 0, "after_write": 0},
    })
}

pub(super) fn parsed_contains_bridge_diagnostics(parsed: &ParsedRequest) -> bool {
    parsed.ops.iter().any(|op| op.tool == "bridge.diagnostics")
}

fn bridge_diagnostics_help() -> Value {
    let counter = json!({"type":"integer", "minimum":0});
    json!({
        "verb": "bridge.diagnostics",
        "pack": "bridge-control",
        "description": "Read this stdio bridge process image's fallback and slow-daemon counters without daemon or store access.",
        "category": "Read",
        "params": [{
            "name": "help",
            "type": "boolean",
            "required": false,
            "description": "Set to true to return this local schema. No other operation argument is accepted."
        }],
        "input_schema": {
            "type": "object",
            "properties": {"help": {"const": true}},
            "additionalProperties": false
        },
        "result_schema": {
            "type": "object",
            "required": ["bridge_instance_id", "pid", "fallback_reasons", "fallback_total", "strict_violations", "daemon_slow"],
            "properties": {
                "bridge_instance_id": {"type":"string", "format":"uuid", "description":"Full 36-character generation UUID; changes after an in-place re-exec."},
                "pid": {"type":"integer", "minimum":0},
                "fallback_reasons": {
                    "type":"object",
                    "required": ["config_mismatch", "namespace_mismatch", "no_socket", "parse_failure", "protocol_mismatch"],
                    "properties": {
                        "config_mismatch": counter,
                        "namespace_mismatch": counter,
                        "no_socket": counter,
                        "parse_failure": counter,
                        "protocol_mismatch": counter
                    },
                    "additionalProperties": false
                },
                "fallback_total": counter,
                "strict_violations": counter,
                "daemon_slow": {
                    "type":"object",
                    "description":"Requests refused because the daemon was alive but did not answer in time; not fallbacks, so not in fallback_total.",
                    "required": ["before_dispatch", "after_write"],
                    "properties": {
                        "before_dispatch": counter,
                        "after_write": counter
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        },
        "identifier_resolution": khive_runtime::pack::identifier_resolution_help(),
    })
}

/// Extract the fallback-reason string from a strict-mode rejection's
/// [`McpError`] (#947), or `None` if `e` is not tagged with
/// [`crate::daemon::STRICT_FALLBACK_MARKER`] — i.e. some other daemon-forward
/// error that must stay an RPC-level error.
#[cfg(unix)]
fn strict_fallback_reason(e: &McpError) -> Option<String> {
    let data = e.data.as_ref()?;
    if data.get(crate::daemon::STRICT_FALLBACK_MARKER)?.as_bool() != Some(true) {
        return None;
    }
    data.get("reason")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Build the wire-contract failed-op envelope for a strict-mode daemon
/// fallback rejection (#947 Medium finding).
///
/// The request was never attempted — locally or on the daemon — but the wire
/// response must still be a normal per-op envelope
/// (`{"results": [...], "summary": {...}}`) reporting the fallback reason as
/// each op's `error`, not an RPC-level `McpError`. Chain mode aborts after the
/// first op, matching `run_parsed`'s `Chain` arm and the wire contract's
/// documented abort-on-failure behavior for `|`-chained ops.
#[cfg(unix)]
pub(super) fn strict_fallback_envelope_response(
    p: &RequestParams,
    reason: String,
) -> Result<String, McpError> {
    let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;
    let total = parsed.ops.len();
    let error_msg = format!(
        "daemon fallback rejected in strict mode (KHIVE_DAEMON_STRICT, or a daemon \
         socket or PID file on disk): reason={reason}; refusing to complete the request \
         via local dispatch; rebuild with `make local` and retry"
    );

    let results: Vec<Value> = match parsed.mode {
        ExecutionMode::Chain => parsed
            .ops
            .iter()
            .enumerate()
            .map(|(i, op)| {
                if i == 0 {
                    failure_entry(
                        op.tool.clone(),
                        json!(error_msg),
                        DomainDisposition::NotCommitted,
                    )
                } else {
                    aborted_entry(op.tool.clone(), None)
                }
            })
            .collect(),
        ExecutionMode::Single | ExecutionMode::Parallel => parsed
            .ops
            .iter()
            .map(|op| {
                failure_entry(
                    op.tool.clone(),
                    json!(error_msg),
                    DomainDisposition::NotCommitted,
                )
            })
            .collect(),
    };

    let aborted = if parsed.mode == ExecutionMode::Chain {
        total.saturating_sub(1)
    } else {
        0
    };
    let failed = total - aborted;
    Ok(serde_json::to_string(&json!({
        "results": results,
        "summary": { "total": total, "succeeded": 0, "failed": failed, "aborted": aborted },
        "status": batch_status(failed, aborted),
    }))
    .expect("envelope of string/bool JSON values always serializes"))
}
