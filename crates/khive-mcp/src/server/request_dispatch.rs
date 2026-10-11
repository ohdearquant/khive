use khive_request::{parse_request, parse_typed_json_batch, ParsedRequest, TypedJsonOp};
use khive_runtime::{OutputFormat, PresentationMode};
use rmcp::ErrorData as McpError;

use crate::tools::request::RequestParams;

use super::batch_types::{DispatchOrigin, ParsedDispatchPolicy, RunParsedContext};
use super::{
    attach_audit_persistence_advisories, attach_strict_refusal_reasons, dsl_err_to_mcp,
    invalid_request_error, mark_daemon_lexical_timeout, parse_output_format,
    parse_presentation_mode, parsed_contains_bridge_diagnostics, render_result,
    request_internal_error, request_read_timeout, save_to_write_error, validate_request_overrides,
    KhiveMcpServer, RenderContext,
};

impl KhiveMcpServer {
    /// Build the daemon forward-frame for an agent-facing `request` tool call.
    ///
    /// `from_wire` is unconditionally `true`: this is the agent wire surface, so
    /// `Visibility::Subhandler` verbs must be rejected whether the request runs
    /// on the warm daemon or via the local fallback. Keeping the bit in one
    /// named, unit-tested place stops the daemon-forward path from silently
    /// diverging from `dispatch_request_wire`.
    #[cfg(unix)]
    pub(crate) fn wire_daemon_frame(&self, p: &RequestParams) -> khive_runtime::DaemonRequestFrame {
        khive_runtime::DaemonRequestFrame {
            plan: false,
            ops: p.ops.clone(),
            presentation: p.presentation.clone(),
            presentation_per_op: p.presentation_per_op.clone(),
            namespace: self.default_namespace.clone(),
            // ADR-096 Fork 1: carry this server's OWN resolved actor/visibility
            // identity on the frame so a warm daemon with a *different* baked
            // identity serves the request under this caller's identity instead
            // of rejecting it or silently stamping writes under its own actor.
            actor_id: self.actor_id().map(str::to_string),
            process_ref: khive_runtime::process_ref_from_env(),
            visible_namespaces: self
                .visible_namespaces()
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            config_id: self.config_id.clone(),
            protocol_version: khive_runtime::daemon::PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: p.format.clone(),
            format_per_op: p.format_per_op.clone(),
            from_wire: true,
            // khive#948: forwarded unchanged from the tool caller's params.
            // `None` when the caller supplied no id (pre-#948 client).
            request_id: p.request_id,
        }
    }

    /// Parse and dispatch a request against this server's own registry.
    ///
    /// This is the canonical **operator** dispatch path: subhandler verbs are
    /// allowed. `kkernel exec`, in-process callers, and tests use this. The
    /// agent-facing MCP wire surface goes through `dispatch_request_wire`
    /// (or sets `from_wire` on the daemon frame), which enforces verb visibility.
    ///
    /// Pure local dispatch: no [`khive_runtime::RequestIdentity`] override is
    /// applied by this caller (ADR-096 Fork 1) — this server's own
    /// construction-baked namespace/actor/visibility is used, unchanged from
    /// before per-request identity existed. `dispatch_request_inner` (khive#948)
    /// may still synthesize an identity carrying those same baked scalars if
    /// `p.request_id` is set, purely so the audit row is correlatable.
    pub async fn dispatch_request_local(&self, p: RequestParams) -> Result<String, McpError> {
        self.dispatch_request_inner(p, false, None, DispatchOrigin::Local)
            .await
    }

    /// Operator dispatch used by `kkernel exec`.
    ///
    /// When `strict_refusals` is true, otherwise-unclassified failed/aborted
    /// rows receive `strict-op-failure` before `save_to` writes and checksums
    /// JSONL. Stderr emission remains at the CLI boundary.
    pub async fn dispatch_request_local_for_exec(
        &self,
        p: RequestParams,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        self.dispatch_request_inner_with_strict_refusals(
            p,
            false,
            None,
            DispatchOrigin::Local,
            strict_refusals,
        )
        .await
    }

    /// Dispatch a bounded, already-decoded JSON batch for `kkernel exec --ops-file`.
    ///
    /// The ops-file reader owns its 96 MiB line, 512 MiB file, 32 MiB chunk,
    /// and 100-op limits. This seam preserves JSON-form validation but avoids
    /// serializing those typed values back into the public raw-DSL parser,
    /// whose independent 1 MiB limit remains unchanged for MCP, HTTP, daemon,
    /// inline exec, and every other string request surface.
    pub async fn dispatch_typed_json_batch_local_for_exec(
        &self,
        ops: Vec<TypedJsonOp>,
        presentation: Option<String>,
        format: Option<String>,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        self.dispatch_typed_json_batch_local_for_exec_with_policy(
            ops,
            presentation,
            format,
            ParsedDispatchPolicy::bounded_parallel(strict_refusals),
        )
        .await
    }

    /// Dispatch one full typed ops-file chunk with exactly one handler in
    /// flight while retaining ordinary parallel-batch semantics.
    ///
    /// Parsing, write-key conflict detection, aggregate response budgeting,
    /// result ordering, presentation, audit, and strict-refusal handling remain
    /// shared with [`Self::dispatch_typed_json_batch_local_for_exec`]. Only the
    /// trusted local scheduler's concurrency cap changes.
    pub async fn dispatch_typed_json_batch_serial_local_for_exec(
        &self,
        ops: Vec<TypedJsonOp>,
        presentation: Option<String>,
        format: Option<String>,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        self.dispatch_typed_json_batch_local_for_exec_with_policy(
            ops,
            presentation,
            format,
            ParsedDispatchPolicy::serial(strict_refusals),
        )
        .await
    }

    async fn dispatch_typed_json_batch_local_for_exec_with_policy(
        &self,
        ops: Vec<TypedJsonOp>,
        presentation: Option<String>,
        format: Option<String>,
        policy: ParsedDispatchPolicy,
    ) -> Result<String, McpError> {
        debug_assert!(policy.max_batch_concurrency > 0);
        let parsed = parse_typed_json_batch(ops).map_err(dsl_err_to_mcp)?;
        let p = RequestParams {
            plan: None,
            ops: String::new(),
            presentation,
            presentation_per_op: None,
            save_to: None,
            format,
            format_per_op: None,
            request_id: None,
        };
        let timeout = crate::request_policy::parsed_read_timeout(&parsed, request_read_timeout());
        let dispatch = Box::pin(self.dispatch_parsed_request_inner_scoped(
            p,
            parsed,
            false,
            None,
            DispatchOrigin::Local,
            policy,
        ));
        khive_storage::scope_request_read_deadline(timeout, dispatch).await
    }

    /// Replay one stored public-surface request under a host-verified actor.
    ///
    /// An attributed actor must come from an out-of-band provenance check,
    /// never from a field inside the stored request. `None` is reserved for a
    /// provenance-verified anonymous/local creator, preserving that actor kind.
    /// Replay deliberately sets `from_wire=true`: scheduling delays a public
    /// request; it does not upgrade that request into the operator-only local
    /// surface where [`khive_runtime::Visibility::Subhandler`] verbs are callable.
    pub(crate) async fn dispatch_request_replay_as(
        &self,
        p: RequestParams,
        namespace: &str,
        verified_actor: Option<khive_runtime::VerifiedActor>,
    ) -> Result<String, McpError> {
        let identity = khive_runtime::RequestIdentity {
            namespace: namespace.to_string(),
            // `None` is the provenance-verified anonymous/local identity;
            // spelling that identity as `Some("local")` would incorrectly
            // reconstruct it as the distinct authenticated `actor:local`.
            actor_id: verified_actor.map(|actor| actor.as_str().to_string()),
            process_ref: khive_runtime::process_ref_from_env(),
            // A scheduled action is scoped exactly to its event namespace;
            // it never inherits the daemon's broader read visibility.
            visible_namespaces: Vec::new(),
            request_id: None,
        };
        self.dispatch_request_inner(p, true, Some(identity), DispatchOrigin::Local)
            .await
    }

    /// Wire-surface dispatch: same as [`Self::dispatch_request_local`] but
    /// enforces verb visibility (`Visibility::Subhandler` verbs are rejected).
    /// Used by the stdio `request` tool's local-fallback path.
    pub(crate) async fn dispatch_request_wire(&self, p: RequestParams) -> Result<String, McpError> {
        self.dispatch_request_inner(p, true, None, DispatchOrigin::Local)
            .await
    }

    /// Shared body for both dispatch surfaces. `from_wire` decides whether the
    /// subhandler-visibility gate fires (see [`run_parsed`](Self::run_parsed)).
    ///
    /// `identity` is the per-request identity context threaded from a daemon
    /// frame (ADR-096 Fork 1, see `crate::daemon`'s `DaemonDispatch` impl).
    /// `None` for every local (non-daemon-served) call — this server's own
    /// baked identity applies, exactly as before this parameter existed.
    /// `origin` independently controls daemon-frame response fitting; wire
    /// visibility does not imply that the response travels through the daemon.
    ///
    /// khive#948: when `identity` is `None` (every local-dispatch call —
    /// `KHIVE_NO_DAEMON`/soft daemon-fallback and the `save_to` bypass both
    /// route here via `dispatch_request_wire`) and the caller supplied a
    /// `request_id`, a `RequestIdentity` is synthesized so the audit row
    /// stamped by this dispatch is still correlatable. The synthesized
    /// identity mirrors this server's own baked `default_namespace` /
    /// `actor_id` / `visible_namespaces` exactly — it changes no dispatch
    /// semantics, only adds the correlation id — so a request with no
    /// `request_id` still dispatches through the untouched `identity = None`
    /// path.
    pub(crate) async fn dispatch_request_inner(
        &self,
        p: RequestParams,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
        origin: DispatchOrigin,
    ) -> Result<String, McpError> {
        self.dispatch_request_inner_with_strict_refusals(p, from_wire, identity, origin, false)
            .await
    }

    async fn dispatch_request_inner_with_strict_refusals(
        &self,
        p: RequestParams,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
        origin: DispatchOrigin,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        if let Some(plan) = self.plan_response(&p)? {
            return Ok(plan);
        }
        // `dispatch_request_inner_scoped` is the complete parse/dispatch/render
        // pipeline. Keep that large generator behind one pointer before handing
        // it to the generic task-local scope: otherwise the scope embeds the
        // pipeline in every MCP, local-exec, and replay request future. LLVM
        // coverage instrumentation amplifies the resulting poll stack enough to
        // overflow Tokio's normal worker stack even for unrelated small verbs.
        let timeout = crate::request_policy::read_timeout(&p.ops, request_read_timeout());
        let dispatch = Box::pin(self.dispatch_request_inner_scoped(
            p,
            from_wire,
            identity,
            origin,
            strict_refusals,
        ));
        khive_storage::scope_request_read_deadline(timeout, dispatch).await
    }

    async fn dispatch_request_inner_scoped(
        &self,
        p: RequestParams,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
        origin: DispatchOrigin,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;
        self.dispatch_parsed_request_inner_scoped(
            p,
            parsed,
            from_wire,
            identity,
            origin,
            ParsedDispatchPolicy::bounded_parallel(strict_refusals),
        )
        .await
    }

    async fn dispatch_parsed_request_inner_scoped(
        &self,
        p: RequestParams,
        parsed: ParsedRequest,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
        origin: DispatchOrigin,
        policy: ParsedDispatchPolicy,
    ) -> Result<String, McpError> {
        if parsed_contains_bridge_diagnostics(&parsed) {
            return Err(invalid_request_error(
                "bridge.diagnostics is available only on the stdio bridge".into(),
            ));
        }
        validate_request_overrides(&p, parsed.ops.len())?;
        let ParsedDispatchPolicy {
            strict_refusals,
            max_batch_concurrency,
        } = policy;
        let save_to = p.save_to.clone();
        let identity = identity.or_else(|| {
            p.request_id
                .map(|request_id| khive_runtime::RequestIdentity {
                    namespace: self.default_namespace.clone(),
                    actor_id: self.actor_id().map(str::to_string),
                    process_ref: khive_runtime::process_ref_from_env(),
                    visible_namespaces: self
                        .visible_namespaces()
                        .iter()
                        .map(|ns| ns.as_str().to_string())
                        .collect(),
                    request_id: Some(request_id),
                })
        });

        // Parse presentation strings → PresentationMode.
        let presentation =
            parse_presentation_mode(p.presentation.as_deref()).map_err(invalid_request_error)?;
        let presentation_per_op: Option<Vec<Option<PresentationMode>>> =
            if let Some(per_op_strs) = p.presentation_per_op {
                let mut modes = Vec::with_capacity(per_op_strs.len());
                for s in per_op_strs {
                    let mode = match s.as_deref() {
                        None => None,
                        Some(v) => {
                            Some(parse_presentation_mode(Some(v)).map_err(invalid_request_error)?)
                        }
                    };
                    modes.push(mode);
                }
                Some(modes)
            } else {
                None
            };

        // Resolve the output format for this request (ADR-078 §2 precedence):
        // per-request `format` field → server default (already resolved from
        // env + toml + builtin by `serve.rs`).
        let batch_format = parse_output_format(p.format.as_deref())
            .map_err(invalid_request_error)?
            .unwrap_or(self.default_output_format);

        // Per-op format overrides (ADR-078 §8.4).
        let format_per_op: Option<Vec<Option<OutputFormat>>> =
            if let Some(per_op_strs) = p.format_per_op {
                let mut fmts = Vec::with_capacity(per_op_strs.len());
                for s in per_op_strs {
                    let fmt = match s.as_deref() {
                        None => None,
                        Some(v) => Some(
                            parse_output_format(Some(v))
                                .map_err(invalid_request_error)?
                                .unwrap_or(batch_format),
                        ),
                    };
                    fmts.push(fmt);
                }
                Some(fmts)
            } else {
                None
            };

        // Reserve and validate the destination before any operation can run.
        // Wire requests are restricted to the export root; the trusted CLI
        // keeps its documented unrestricted destination policy.
        let save_sink = save_to
            .as_deref()
            .map(|path| {
                crate::save_sink::JsonlSaveSink::new(std::path::Path::new(path), from_wire)
                    .map_err(|error| invalid_request_error(format!("save_to: {error}")))
            })
            .transpose()?;

        let (mut result, content_scopes) = self
            .run_parsed(
                parsed.ops,
                parsed.mode,
                parsed.ranges,
                presentation,
                presentation_per_op.clone(),
                RunParsedContext {
                    enforce_response_budget: save_to.is_none(),
                    max_batch_concurrency,
                    from_wire,
                    identity: identity.as_ref(),
                },
            )
            .await;

        attach_audit_persistence_advisories(&mut result, &self.registry);

        if strict_refusals {
            attach_strict_refusal_reasons(&mut result);
        }

        if let Some(sink) = save_sink {
            let manifest = sink
                .write_envelope(&result)
                .map_err(|error| save_to_write_error(format!("save_to: {error}"), &result))?;
            // Manifests are always compact JSON regardless of format (lossless metadata).
            return serde_json::to_string(&manifest)
                .map_err(|e| request_internal_error(format!("serialize manifest: {e}")));
        }

        if origin == DispatchOrigin::Daemon {
            mark_daemon_lexical_timeout(&mut result);
        }

        // Apply per-op format rendering (ADR-078 §8.4 and §9).
        Ok(render_result(
            result,
            batch_format,
            &format_per_op,
            presentation,
            &presentation_per_op,
            &RenderContext {
                registry: &self.registry,
                content_scopes: &content_scopes,
            },
            (origin == DispatchOrigin::Daemon).then_some(self.config_id.as_str()),
        ))
    }
}
