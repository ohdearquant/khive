use std::{ops::Range, sync::Arc};

use khive_request::{unit_write_key_conflicts, ArgValue, ExecutionMode, ParsedOp};
use khive_runtime::presentation::{NoteContentScope, PresentationNow};
#[cfg(doc)]
use khive_runtime::VerbRegistry;
use khive_runtime::{DomainDisposition, PresentationMode, VerbPresentationPolicy};
use rmcp::ServiceExt;
use serde_json::{json, Value};

use crate::coordinator::CoordinatorService;

use super::batch_types::{
    aborted_entry, failure_entry, BatchTask, DispatchFailure, RunParsedContext, UnitBudget,
    UnitOutcome, UnitTask,
};
use super::config_id::build_verb_catalog;
use super::search_diagnostics::{op_success_from_registry_result, validated_search_text_mode};
use super::server_core::{
    stdio_bridge_idle_timeout_from_env, stdio_bridge_max_outstanding_requests_from_env,
    stdio_bridge_request_obligation_ttl_from_env, stdio_bridge_response_deadline_from_env,
};
#[cfg(unix)]
use super::server_core::{stdio_serve_mode_for, StdioServeMode};
#[cfg(doc)]
use super::MAX_BATCH_CONCURRENCY;
use super::{
    apply_presentation_to_result, batch_budget_error, batch_status, bridge_instance_id,
    chain_aggregation_depth_reject, chain_ok_envelope_or_depth_error,
    decorate_schedule_agenda_with_ticker_health, dispatch_via_coordinator_inner,
    execute_bounded_batch, execute_bounded_units, note_content_scope, parallel_batch_envelope,
    parse_content_requested, present_ok_envelope_or_depth_error, stamp_usage,
    substitution_error_payload, KhiveMcpServer, BATCH_RESPONSE_BUDGET_BYTES,
};

impl KhiveMcpServer {
    /// Serve over stdio (blocks until the connection closes).
    ///
    /// #714: a resumed generation (produced by `crate::daemon`'s in-place
    /// re-exec self-heal on a stale-protocol mismatch) skips the normal MCP
    /// initialize handshake via `serve_directly` — by construction, its peer
    /// already completed a real handshake with the prior generation over this
    /// same, uninterrupted stdio pipe pair, so waiting for another one would
    /// hang forever (the client has no reason to send a second `initialize`).
    /// A cold start (the overwhelmingly common case) is unaffected: no
    /// `--resumed-generation` marker means the normal `.serve()` handshake
    /// runs exactly as before this change.
    ///
    /// Both branches keep `crate::daemon::SelfHealOnFlushTransport` directly
    /// around the raw stdio transport — the actual happens-after edge that
    /// fires an armed self-heal re-exec (or drain-and-exit) only once a message
    /// has genuinely finished flushing to the client. The outer EOF adapter
    /// shares rmcp's root cancellation token so disconnect cancels every
    /// per-request child before rmcp starts its graceful drain.
    #[cfg(unix)]
    pub async fn serve_stdio(mut self) -> anyhow::Result<()> {
        use rmcp::transport::{async_rw::AsyncRwTransport, stdio};

        let _ = bridge_instance_id();
        self.stdio_bridge = true;
        self.bridge_executable = crate::daemon::executable::BridgeExecutable::current()
            .map(|executable| Arc::new(std::sync::Mutex::new(executable)));
        let root = tokio_util::sync::CancellationToken::new();
        let idle_timeout = stdio_bridge_idle_timeout_from_env();
        let response_deadline = stdio_bridge_response_deadline_from_env()?;
        let max_outstanding_requests = stdio_bridge_max_outstanding_requests_from_env();
        let max_line_bytes = crate::stdio_line_limit::max_line_bytes_from_env()?;
        let build_transport = |root: tokio_util::sync::CancellationToken| {
            let (read, write) = stdio();
            let write =
                crate::transport::DeadlineWriter::new(write, response_deadline, root.clone());
            crate::transport::CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
                crate::daemon::SelfHealOnFlushTransport::new(AsyncRwTransport::new_server(
                    crate::stdio_line_limit::BoundedLineReader::new(read, max_line_bytes),
                    write,
                )),
                root,
                idle_timeout,
                Some(response_deadline),
                stdio_bridge_request_obligation_ttl_from_env(),
                max_outstanding_requests,
            )
        };

        let mode = stdio_serve_mode_for(crate::daemon::resumed_generation());
        let transport = mode.wrap_transport(build_transport(root.clone()));
        match mode {
            StdioServeMode::Resumed => {
                let service = rmcp::service::serve_directly_with_ct(self, transport, None, root);
                service.waiting().await?;
            }
            StdioServeMode::Handshake => {
                let service = self.serve_with_ct(transport, root).await?;
                service.waiting().await?;
            }
        }
        Ok(())
    }

    /// Non-Unix stdio serving. The #714 self-heal re-exec mechanism
    /// (`crate::daemon`'s `SelfHealOnFlushTransport`/resumed-generation
    /// machinery) requires `exec()` (POSIX-only) and is only ever armed by a
    /// Unix-domain-socket daemon-forwarding protocol mismatch — there is
    /// nothing to self-heal from on this target (`--daemon` mode itself is
    /// Unix-only, see `serve.rs::serve_server`), so this path always runs the
    /// normal MCP `initialize` handshake, with no resumed-generation skip and
    /// no flush-triggered hook. It still shares rmcp's root token with the EOF
    /// adapter so disconnect cancellation is platform-independent.
    #[cfg(not(unix))]
    pub async fn serve_stdio(mut self) -> anyhow::Result<()> {
        use rmcp::transport::{async_rw::AsyncRwTransport, stdio};

        let _ = bridge_instance_id();
        self.stdio_bridge = true;
        let root = tokio_util::sync::CancellationToken::new();
        let (read, write) = stdio();
        let max_line_bytes = crate::stdio_line_limit::max_line_bytes_from_env()?;
        let response_deadline = stdio_bridge_response_deadline_from_env()?;
        let write = crate::transport::DeadlineWriter::new(write, response_deadline, root.clone());
        let transport =
            crate::transport::CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
                AsyncRwTransport::new_server(
                    crate::stdio_line_limit::BoundedLineReader::new(read, max_line_bytes),
                    write,
                ),
                root.clone(),
                stdio_bridge_idle_timeout_from_env(),
                Some(response_deadline),
                stdio_bridge_request_obligation_ttl_from_env(),
                stdio_bridge_max_outstanding_requests_from_env(),
            );
        let transport = crate::transport::PreInitializeRequestTransport::new(transport);
        let service = self.serve_with_ct(transport, root).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Build the textual verb catalog included in the request tool's description.
    ///
    /// The list is rebuilt from the runtime registry so it always reflects which
    /// packs are actually loaded.
    pub(super) fn verb_catalog(&self) -> String {
        let verbs = self
            .registry
            .all_verbs_with_names()
            .into_iter()
            .map(|(pack, v)| (pack.to_owned(), v.name.to_owned(), v.description.to_owned()));
        let mounted = self
            .registry
            .mounted_verb_snapshot()
            .into_iter()
            .map(|verb| {
                (
                    verb["pack"].as_str().unwrap_or_default().to_owned(),
                    verb["verb"].as_str().unwrap_or_default().to_owned(),
                    verb["description"].as_str().unwrap_or_default().to_owned(),
                )
            });
        build_verb_catalog(verbs.chain(mounted))
    }

    /// Dispatch a single [`ParsedOp`] by resolving its args (potentially
    /// substituting `$prev` references) and calling the [`VerbRegistry`].
    ///
    /// Returns a per-op result object: `{ok, tool, result}` on success or
    /// `{ok: false, tool, error, reason?}` on failure.
    async fn dispatch_op(
        &self,
        op: ParsedOp,
        prev_result: Option<&Value>,
        from_wire: bool,
        identity: Option<&khive_runtime::RequestIdentity>,
    ) -> Result<Value, DispatchFailure> {
        let ParsedOp { tool, args } = op;

        // Resolve args — substitute $prev references when prev_result is Some.
        // Handles flat PrevRef as well as Array/Object containing nested refs.
        let mut resolved: serde_json::Map<String, Value> = serde_json::Map::new();
        for (name, arg_val) in args {
            let needs_prev = !matches!(&arg_val, ArgValue::Value(_));
            let value = if needs_prev {
                // `dispatch_op` only ever runs inside a chain (see `run_parsed`'s
                // `ExecutionMode::Chain` arm); `prev_result` is `None` here
                // exactly when this is the chain's first op, so there is no
                // preceding result to substitute from at all.
                let prev = prev_result.ok_or_else(|| {
                    DispatchFailure::before_dispatch(
                        tool.clone(),
                        json!({
                            "kind": "substitution_error",
                            "reason": "no_preceding_op",
                            "message": format!(
                                "argument {name:?}: $prev has no preceding op to resolve \
                                 against — this is the first operation in the chain. $prev \
                                 always resolves against the immediately preceding op's result \
                                 only; it cannot reach further back or forward. Move this op \
                                 after the one that produces the value, or pass a literal value \
                                 here instead of $prev."
                            )
                        }),
                    )
                })?;
                let resolved_val = arg_val.resolve_all(prev).ok_or_else(|| {
                    DispatchFailure::before_dispatch(
                        tool.clone(),
                        substitution_error_payload(&name, &arg_val, prev),
                    )
                })?;
                // UE4-H1: bare `$prev` (no path) resolving to a map or array
                // will cause a confusing downstream type error. Detect it here and
                // surface a clear substitution error with available field names.
                if matches!(&arg_val, ArgValue::PrevRef { path } if path.is_empty()) {
                    match &resolved_val {
                        Value::Object(map) => {
                            let fields: Vec<&str> = map.keys().map(String::as_str).collect();
                            return Err(DispatchFailure::before_dispatch(
                                tool.clone(),
                                json!({
                                    "kind": "substitution_error",
                                    "reason": "bare_ref_ambiguous",
                                    "message": format!(
                                        "argument {name:?}: $prev requires a dotted path \
                                         (e.g. $prev.id) when the prior result is a map. \
                                         Available top-level fields: [{}]",
                                        fields.join(", ")
                                    ),
                                }),
                            ));
                        }
                        Value::Array(_) => {
                            return Err(DispatchFailure::before_dispatch(
                                tool.clone(),
                                json!({
                                    "kind": "substitution_error",
                                    "reason": "bare_ref_ambiguous",
                                    "message": format!(
                                        "argument {name:?}: $prev requires a dotted path \
                                         (e.g. $prev.0) when the prior result is an array. \
                                         Use $prev.N to select a specific element."
                                    ),
                                }),
                            ));
                        }
                        _ => {}
                    }
                }
                resolved_val
            } else {
                match arg_val {
                    ArgValue::Value(v) => v,
                    _ => unreachable!(),
                }
            };
            resolved.insert(name, value);
        }

        let args_value = Value::Object(resolved);

        // Subhandler verbs are operator-only — block them at the MCP wire
        // boundary (`from_wire`), never on the operator path (`kkernel exec`,
        // in-process callers). Exception: `help=true` is short-circuited in
        // VerbRegistry::dispatch before reaching the pack, so introspection works.
        let is_help = args_value
            .get("help")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if from_wire && !is_help && self.registry.is_subhandler_verb(&tool) {
            return Err(DispatchFailure::before_dispatch(
                tool.clone(),
                json!(format!(
                    "permission denied for verb {tool:?}: verb '{tool}' is an internal \
                     subhandler and cannot be invoked via the MCP request surface"
                )),
            ));
        }

        // Multi-backend interception: route link/search through the coordinator (ADR-029 D3/D4).
        // Single-backend and non-link/search verbs fall through to the registry unchanged.
        if let Some(coord_result) = self
            .dispatch_via_coordinator(&tool, &args_value, identity)
            .await
        {
            return coord_result.and_then(|result| chain_ok_envelope_or_depth_error(tool, result));
        }

        let search_args = (tool == "search" && !is_help).then(|| args_value.clone());
        match self
            .registry
            .dispatch_with_disposition(&tool, args_value, identity.cloned())
            .await
        {
            Ok(result) => {
                let text_mode = match search_args.as_ref() {
                    Some(args) => match validated_search_text_mode(args, &self.registry) {
                        Ok(mode) => mode,
                        Err(error) => {
                            return Err(DispatchFailure::before_dispatch(
                                &tool,
                                json!(error.to_string()),
                            ));
                        }
                    },
                    None => "all_terms",
                };
                let result = decorate_schedule_agenda_with_ticker_health(
                    &tool,
                    is_help,
                    result,
                    self.schedule_ticker_last_tick_micros.as_ref(),
                );
                let vector_selected = self
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.vector_arm_selected());
                let success = op_success_from_registry_result(
                    &tool,
                    is_help,
                    result,
                    vector_selected,
                    text_mode,
                );
                chain_ok_envelope_or_depth_error(tool, success)
            }
            Err(error) => Err(DispatchFailure::from_dispatch(&tool, error)),
        }
    }

    /// Execute a parsed request, dispatching according to its [`ExecutionMode`].
    ///
    /// - `Single` / `Parallel`: at most [`MAX_BATCH_CONCURRENCY`] ops run at
    ///   once; per-op failure does not abort siblings. `aborted` count is 0.
    /// - `Chain`: ops run sequentially; `$prev` from each op's result is
    ///   substituted into the next op's args. If any op fails (or a `$prev`
    ///   substitution fails), remaining ops appear as `aborted: true`.
    ///
    /// Presentation transforms are applied per-op AFTER dispatch,
    /// using `mode_for_op` to determine the mode per position. Chain `$prev`
    /// substitution uses canonical (verbose) handler output; the transform runs
    /// only at the final response-envelope boundary.
    ///
    /// Aggregate `status` describes failed or aborted operations — distinct
    /// from a `search` op's own per-operation `status` field ("complete" /
    /// "partial", ADR-130 §1), which lives inside that op's `results` entry,
    /// never at this top level. A successful but incomplete coordinator
    /// search remains a success (`status="partial"` on that entry) and
    /// carries bounded `missing_backends` and `backend_errors` diagnostics
    /// plus the deprecated `partial` alias; a search where no hit survives a
    /// backend failure is instead a failed op (`ok: false`,
    /// `error.kind: "search_incomplete"`).
    ///
    /// Response envelope:
    /// ```json
    /// {
    ///   "results": [...],
    ///   "summary": { "total": N, "succeeded": K, "failed": M, "aborted": A },
    ///   "status": "success" | "partial"
    /// }
    /// ```
    ///
    /// `status` is a structural signal for a partially-failed batch (#1220):
    /// per-op `results` entries and `summary.failed`/`summary.aborted` counts
    /// already carry this information, but a caller that checks only for the
    /// absence of a top-level RPC error has nothing to branch on. `"partial"`
    /// means at least one op in this response failed or was aborted;
    /// `"success"` means every op in `results` reports `ok: true`.
    pub(super) async fn run_parsed(
        &self,
        ops: Vec<ParsedOp>,
        mode: ExecutionMode,
        ranges: Vec<Range<usize>>,
        presentation: PresentationMode,
        presentation_per_op: Option<Vec<Option<PresentationMode>>>,
        context: RunParsedContext<'_>,
    ) -> (Value, Vec<NoteContentScope>) {
        let RunParsedContext {
            enforce_response_budget,
            max_batch_concurrency,
            from_wire,
            identity,
        } = context;
        debug_assert!(max_batch_concurrency > 0);
        let response_budget = if mode == ExecutionMode::Parallel && enforce_response_budget {
            BATCH_RESPONSE_BUDGET_BYTES
        } else {
            usize::MAX
        };
        let now_unix = PresentationNow::from(chrono::Utc::now());

        // Resolve per-op presentation mode: per-op entry overrides batch default.
        let mode_for_op = |i: usize| -> PresentationMode {
            presentation_per_op
                .as_ref()
                .and_then(|v| v.get(i))
                .and_then(|o| *o)
                .unwrap_or(presentation)
        };

        let mut parse_content: Vec<bool> = ops
            .iter()
            .map(|op| parse_content_requested(op, None))
            .collect();
        let response = match mode {
            // ADR-016 Amendment 2: a bracketed batch whose ranges partition
            // `ops` into units longer than one leaf is a parallel batch of
            // linear chains, not an ordinary flat batch; every ordinary
            // batch keeps `ranges.len() == ops.len()` (one leaf per range)
            // and falls through to the unchanged arm below.
            ExecutionMode::Parallel if ranges.iter().any(|range| range.len() > 1) => {
                let (resp, content_updates) = self
                    .run_parallel_units(
                        ops,
                        ranges,
                        presentation,
                        presentation_per_op.clone(),
                        context,
                        now_unix,
                    )
                    .await;
                for (index, requested) in content_updates {
                    parse_content[index] = requested;
                }
                resp
            }
            ExecutionMode::Single | ExecutionMode::Parallel => {
                // Write-key conflict preflight.
                //
                // Detect ops that target the same write key in the same parallel/single
                // batch. Conflicting ops receive per-op error entries; non-conflicting ops
                // execute normally. `results.length == summary.total` is preserved.
                let conflict_ops = khive_request::write_key_conflict_ops(&ops);

                // Clone coordinator and namespace for use in the per-op closures (ADR-029 D3/D4).
                let coordinator: Option<Arc<dyn CoordinatorService>> = self.coordinator.clone();
                let schedule_ticker_last_tick_micros =
                    self.schedule_ticker_last_tick_micros.clone();
                let vector_selected = self
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.vector_arm_selected());
                // ADR-096 Fork 1: a per-request identity overrides the default
                // namespace for both the coordinator intercept and the registry
                // dispatch below, so the two can't drift out of sync per op.
                let identity_owned: Option<khive_runtime::RequestIdentity> = identity.cloned();

                // Independent dispatch — bounded concurrency, results restored to input order.
                let futures = ops.into_iter().enumerate().map(|(i, op)| {
                    let conflict_with: Option<String> = if !conflict_ops[i].is_empty() {
                        Some(format!(
                            "conflict: writes overlap with another op in this batch (op #{})",
                            i
                        ))
                    } else {
                        None
                    };

                    let participants = conflict_ops[i].clone();
                    let registry = self.registry.clone();
                    let coord = coordinator.clone();
                    let schedule_ticker_last_tick_micros =
                        schedule_ticker_last_tick_micros.clone();
                    let op_identity = identity_owned.clone();
                    let op_vector_selected = vector_selected;
                    let op_mode = mode_for_op(i);
                    let task_tool = op.tool.clone();
                    let parse_content = parse_content[i];
                    BatchTask {
                        index: i,
                        tool: task_tool,
                        future: async move {
                        // ADR-103 Amendment 2: one dispatch-accounting context
                        // per op; the entry is stamped with the frozen usage
                        // snapshot after dispatch resolves.
                        let usage_ctx = khive_runtime::usage::UsageContext::new();
                        let operation = khive_types::OperationAttribution {
                            op_index: u32::try_from(i).expect("parser bounds operation count"),
                            ref_resolution: khive_types::RefResolution::Literal,
                        };
                        let mut entry = khive_storage::operation_context::scope_operation_attribution(
                            operation,
                            khive_runtime::usage::scope(usage_ctx.clone(), async {
                        let tool = op.tool.clone();
                        // Conflicting ops get a per-op error; skip dispatch.
                        if let Some(msg) = conflict_with {
                            let mut entry = failure_entry(tool, json!(msg), DomainDisposition::NotCommitted);
                            entry["conflict_ops"] = json!(participants);
                            return entry;
                        }
                        // AlwaysVerbose verbs override the caller's presentation mode.
                        let presentation_policy = registry.presentation_policy_for(&tool);
                        let effective_mode =
                            if presentation_policy == VerbPresentationPolicy::AlwaysVerbose
                            {
                                PresentationMode::Verbose
                            } else {
                                op_mode
                            };
                        // No $prev in parallel/single mode — PrevRef, Array(PrevRef),
                        // and Object(PrevRef) are all errors here.
                        let mut resolved: serde_json::Map<String, Value> =
                            serde_json::Map::new();
                        let mut prev_error: Option<Value> = None;
                        for (name, arg_val) in &op.args {
                            if matches!(arg_val, ArgValue::Value(_)) {
                                if let ArgValue::Value(v) = arg_val {
                                    resolved.insert(name.clone(), v.clone());
                                }
                            } else {
                                prev_error = Some(failure_entry(&tool, json!(format!(
                                    "argument {name:?}: $prev reference is only valid in chain (|) mode"
                                )), DomainDisposition::NotCommitted));
                                break;
                            }
                        }
                        if let Some(err) = prev_error {
                            return err;
                        }
                        let args_value = Value::Object(resolved);

                        // Block subhandler verbs at the MCP wire boundary
                        // (`from_wire`) only — operator paths pass through.
                        // Exception: help=true is short-circuited in
                        // VerbRegistry::dispatch before the pack, so
                        // introspection passes through.
                        let is_help = args_value
                            .get("help")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if from_wire && !is_help && registry.is_subhandler_verb(&tool) {
                            return failure_entry(tool.clone(), json!(format!(
                                "permission denied for verb {tool:?}: verb '{tool}' is an \
                                 internal subhandler and cannot be invoked via the MCP request surface"
                            )), DomainDisposition::NotCommitted);
                        }

                        // Multi-backend interception: route link/search through the coordinator
                        // (ADR-029 D3/D4). Falls through to registry for single-backend and
                        // non-link/search verbs.
                        if let Some(active_coord) = coord.as_ref() {
                            if !active_coord.is_single_backend() {
                                if let Some(coord_result) = dispatch_via_coordinator_inner(
                                    active_coord.as_ref(),
                                    &registry,
                                    &tool,
                                    &args_value,
                                    op_identity.as_ref(),
                                )
                                .await
                                {
                                    return match coord_result {
                                        Ok(result) => present_ok_envelope_or_depth_error(
                                            tool,
                                            result,
                                            effective_mode,
                                            now_unix,
                                            presentation_policy,
                                            NoteContentScope::None,
                                        ),
                                        Err(failure) => failure.into_entry(),
                                    };
                                }
                            }
                        }

                        let search_args =
                            (tool == "search" && !is_help).then(|| args_value.clone());
                        match registry
                            .dispatch_with_disposition(&tool, args_value, op_identity)
                            .await
                        {
                            Ok(result) => {
                                let text_mode = match search_args.as_ref() {
                                    Some(args) => {
                                        match validated_search_text_mode(args, &registry) {
                                            Ok(mode) => mode,
                                            Err(error) => {
                                                return DispatchFailure::before_dispatch(
                                                    &tool,
                                                    json!(error.to_string()),
                                                )
                                                .into_entry();
                                            }
                                        }
                                    }
                                    None => "all_terms",
                                };
                                let result = decorate_schedule_agenda_with_ticker_health(
                                    &tool,
                                    is_help,
                                    result,
                                    schedule_ticker_last_tick_micros.as_ref(),
                                );
                                let success =
                                    op_success_from_registry_result(
                                        &tool,
                                        is_help,
                                        result,
                                        op_vector_selected,
                                        text_mode,
                                    );
                                let content_scope = note_content_scope(
                                    parse_content && !is_help, &tool, &success.result, &registry,
                                );
                                present_ok_envelope_or_depth_error(
                                    tool,
                                    success,
                                    effective_mode,
                                    now_unix,
                                    presentation_policy,
                                    content_scope,
                                )
                            }
                            Err(error) => {
                                DispatchFailure::from_dispatch(&tool, error).into_entry()
                            }
                        }
                        }))
                        .await;
                        stamp_usage(&mut entry, &usage_ctx);
                        entry
                        },
                    }
                });
                let results =
                    execute_bounded_batch(futures, response_budget, max_batch_concurrency).await;
                parallel_batch_envelope(results)
            }
            ExecutionMode::Chain => {
                // Sequential execution with $prev substitution and abort-on-failure.
                // $prev uses canonical (verbose) handler output — presentation runs
                // only at the final response-envelope boundary.
                let total = ops.len();
                let mut results: Vec<Value> = Vec::with_capacity(total);
                // prev_result holds the CANONICAL result (pre-presentation) for $prev.
                let mut prev_result: Option<Value> = None;
                let mut aborted_from: Option<usize> = None;

                for (i, op) in ops.into_iter().enumerate() {
                    if let Some(failed_at) = aborted_from {
                        // A prior op failed — mark remaining as aborted, and say so
                        // plainly: this op was never dispatched (its own $prev, if any,
                        // was never attempted), so the failure to debug lives at the
                        // earlier op, not here.
                        let failed_index = failed_at - 1;
                        let failed_tool = results
                            .get(failed_index)
                            .and_then(|r| r.get("tool"))
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>");
                        results.push(aborted_entry(op.tool, Some(format!(
                            "not executed: op #{failed_index} ({failed_tool:?}) failed earlier in this chain, \
                             so the chain aborted before reaching this op. Fix op #{failed_index} — this \
                             op's own arguments, including any $prev reference, were never evaluated."
                        ))));
                        continue;
                    }
                    parse_content[i] = parse_content_requested(&op, prev_result.as_ref());
                    let op_mode = mode_for_op(i);
                    // AlwaysVerbose verbs override the caller's presentation mode.
                    let presentation_policy = self.registry.presentation_policy_for(&op.tool);
                    let effective_mode =
                        if presentation_policy == VerbPresentationPolicy::AlwaysVerbose {
                            PresentationMode::Verbose
                        } else {
                            op_mode
                        };
                    let usage_ctx = khive_runtime::usage::UsageContext::new();
                    let operation = khive_types::OperationAttribution {
                        op_index: u32::try_from(i).expect("parser bounds operation count"),
                        ref_resolution: if op
                            .args
                            .values()
                            .any(|arg| !matches!(arg, ArgValue::Value(_)))
                        {
                            khive_types::RefResolution::Resolved
                        } else {
                            khive_types::RefResolution::Literal
                        },
                    };
                    match khive_storage::operation_context::scope_operation_attribution(
                        operation,
                        khive_runtime::usage::scope(
                            usage_ctx.clone(),
                            self.dispatch_op(op, prev_result.as_ref(), from_wire, identity),
                        ),
                    )
                    .await
                    {
                        Ok(mut result_obj) => {
                            stamp_usage(&mut result_obj, &usage_ctx);
                            // Guard against a pathologically deep handler result
                            // (e.g. `traverse`/`context`) before it is ever cloned
                            // into `$prev` context or handed to presentation/
                            // serialization, both of which recurse natively over
                            // `Value` and would otherwise be exposed to the same
                            // unbounded-nesting stack-overflow risk (CWE-674) the
                            // DSL parser guard already closes for syntax input.
                            match chain_aggregation_depth_reject(result_obj) {
                                Err(error_entry) => {
                                    results.push(error_entry);
                                    prev_result = None;
                                    aborted_from = Some(i + 1);
                                    continue;
                                }
                                Ok(result_obj) => {
                                    // Extract canonical result for $prev (pre-presentation).
                                    prev_result = result_obj.get("result").cloned();
                                    // Apply presentation to the result field only,
                                    // using the effective mode (AlwaysVerbose override honored).
                                    let content_scope = note_content_scope(
                                        parse_content[i],
                                        result_obj["tool"].as_str().unwrap_or_default(),
                                        &result_obj["result"],
                                        &self.registry,
                                    );
                                    let presented_obj = apply_presentation_to_result(
                                        result_obj,
                                        effective_mode,
                                        now_unix,
                                        presentation_policy,
                                        content_scope,
                                    );
                                    results.push(presented_obj);
                                }
                            }
                        }
                        Err(failure) => {
                            let mut entry = failure.into_entry();
                            stamp_usage(&mut entry, &usage_ctx);
                            results.push(entry);
                            aborted_from = Some(i + 1);
                        }
                    }
                }

                let succeeded = results
                    .iter()
                    .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
                    .count();
                let aborted = results
                    .iter()
                    .filter(|r| r.get("aborted").and_then(Value::as_bool) == Some(true))
                    .count();
                let failed = total - succeeded - aborted;
                json!({
                    "results": results,
                    "summary": { "total": total, "succeeded": succeeded, "failed": failed, "aborted": aborted },
                    "status": batch_status(failed, aborted),
                })
            }
        };
        let content_scopes = response["results"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, entry)| {
                note_content_scope(
                    parse_content[i] && entry["ok"] == true,
                    entry["tool"].as_str().unwrap_or_default(),
                    &entry["result"],
                    &self.registry,
                )
            })
            .collect();
        (response, content_scopes)
    }

    /// ADR-016 Amendment 2 executor for a bracketed batch of linear chains.
    ///
    /// Each `ranges` entry is a unit: its own leaves dispatch sequentially
    /// with their own `$prev`, exactly like plain chain mode (only a unit's
    /// first leaf is barred from `$prev`). Units themselves run
    /// concurrently, bounded by `context.max_batch_concurrency`, and share
    /// one aggregate response budget; once it is exhausted, no leaf
    /// anywhere in the request starts; an already-admitted leaf keeps its
    /// real disposition, and the next never-started leaf of every affected
    /// unit is refused with `response_budget_exceeded`.
    ///
    /// A static write-key conflict between two different units refuses both
    /// units before either dispatches a single leaf; a key repeated inside
    /// one unit is left alone, since that unit's own leaves are already
    /// ordered.
    ///
    /// Returns the response envelope plus the `(global leaf index,
    /// parse_content)` pairs for every leaf this call actually dispatched,
    /// so the caller can fold them into the request-wide `parse_content`
    /// vector before its own trailing `content_scopes` pass.
    async fn run_parallel_units(
        &self,
        ops: Vec<ParsedOp>,
        ranges: Vec<Range<usize>>,
        presentation: PresentationMode,
        presentation_per_op: Option<Vec<Option<PresentationMode>>>,
        context: RunParsedContext<'_>,
        now_unix: PresentationNow,
    ) -> (Value, Vec<(usize, bool)>) {
        let RunParsedContext {
            enforce_response_budget,
            max_batch_concurrency,
            from_wire,
            identity,
        } = context;
        let total = ops.len();
        let response_budget = if enforce_response_budget {
            BATCH_RESPONSE_BUDGET_BYTES
        } else {
            usize::MAX
        };
        let budget = Arc::new(UnitBudget::new(response_budget));
        let unit_conflicts = unit_write_key_conflicts(&ops, &ranges);
        let conflict_ops = khive_request::unit_write_key_conflict_ops(&ops, &ranges);
        let presentation_per_op: Arc<Vec<Option<PresentationMode>>> =
            Arc::new(presentation_per_op.unwrap_or_default());

        let mut leaf_slots: Vec<Option<ParsedOp>> = ops.into_iter().map(Some).collect();
        let unit_tasks: Vec<UnitTask<_>> = ranges
            .iter()
            .cloned()
            .enumerate()
            .map(|(unit_index, range)| {
                let leaves: Vec<ParsedOp> = range
                    .clone()
                    .map(|i| {
                        leaf_slots[i]
                            .take()
                            .expect("each leaf belongs to exactly one range")
                    })
                    .collect();
                let conflicts = unit_conflicts.get(&unit_index).cloned();
                let participants = conflict_ops[range.clone()].to_vec();
                let budget = budget.clone();
                let presentation_per_op = presentation_per_op.clone();
                UnitTask {
                    future: async move {
                        let mut entries: Vec<Value> = Vec::with_capacity(leaves.len());
                        let mut content_updates: Vec<(usize, bool)> =
                            Vec::with_capacity(leaves.len());

                        if let Some(conflicts) = conflicts {
                            let positions: Vec<String> = conflicts
                                .iter()
                                .map(|c| format!("op #{} in unit #{}", c.other_leaf, c.other_unit))
                                .collect();
                            let message = format!(
                                "write-key conflict: this unit shares a write key with another \
                                 unit in the same request ({}). Both units are refused before \
                                 dispatch; split them into separate requests.",
                                positions.join(", ")
                            );
                            let mut leaves = leaves.into_iter();
                            let first = leaves.next().expect("a unit always has at least one leaf");
                            entries.push(failure_entry(
                                first.tool,
                                json!(message),
                                DomainDisposition::NotCommitted,
                            ));
                            for leaf in leaves {
                                entries.push(aborted_entry(
                                    leaf.tool,
                                    Some(
                                        "not executed: this unit was refused before dispatch \
                                         because it shares a write key with another unit in the \
                                         same request."
                                            .to_string(),
                                    ),
                                ));
                            }
                            for (entry, indices) in entries.iter_mut().zip(participants) {
                                entry["conflict_ops"] = json!(indices);
                            }
                            return UnitOutcome {
                                unit_index,
                                entries,
                                content_updates,
                            };
                        }

                        let mut prev_result: Option<Value> = None;
                        let mut aborted_from: Option<usize> = None;
                        for (step_index, op) in leaves.into_iter().enumerate() {
                            let global_index = range.start + step_index;
                            if let Some(failed_at) = aborted_from {
                                let failed_step = failed_at - 1;
                                let failed_tool = entries
                                    .get(failed_step)
                                    .and_then(|r| r.get("tool"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("<unknown>");
                                entries.push(aborted_entry(
                                    op.tool,
                                    Some(format!(
                                        "not executed: leaf #{failed_step} of this unit \
                                         ({failed_tool:?}) failed earlier in the same unit, so \
                                         this unit's tail aborted before reaching this leaf."
                                    )),
                                ));
                                continue;
                            }
                            if budget.is_breached() {
                                entries.push(batch_budget_error(&op.tool, response_budget));
                                aborted_from = Some(step_index + 1);
                                continue;
                            }

                            let content_requested =
                                parse_content_requested(&op, prev_result.as_ref());
                            content_updates.push((global_index, content_requested));
                            let op_mode = presentation_per_op
                                .get(global_index)
                                .and_then(|mode| *mode)
                                .unwrap_or(presentation);
                            let presentation_policy =
                                self.registry.presentation_policy_for(&op.tool);
                            let effective_mode =
                                if presentation_policy == VerbPresentationPolicy::AlwaysVerbose {
                                    PresentationMode::Verbose
                                } else {
                                    op_mode
                                };
                            let usage_ctx = khive_runtime::usage::UsageContext::new();
                            let operation = khive_types::OperationAttribution {
                                op_index: u32::try_from(global_index)
                                    .expect("parser bounds operation count"),
                                ref_resolution: if op
                                    .args
                                    .values()
                                    .any(|arg| !matches!(arg, ArgValue::Value(_)))
                                {
                                    khive_types::RefResolution::Resolved
                                } else {
                                    khive_types::RefResolution::Literal
                                },
                            };
                            match khive_storage::operation_context::scope_operation_attribution(
                                operation,
                                khive_runtime::usage::scope(
                                    usage_ctx.clone(),
                                    self.dispatch_op(op, prev_result.as_ref(), from_wire, identity),
                                ),
                            )
                            .await
                            {
                                Ok(mut result_obj) => {
                                    stamp_usage(&mut result_obj, &usage_ctx);
                                    match chain_aggregation_depth_reject(result_obj) {
                                        Err(error_entry) => {
                                            budget.record(&error_entry);
                                            entries.push(error_entry);
                                            prev_result = None;
                                            aborted_from = Some(step_index + 1);
                                        }
                                        Ok(result_obj) => {
                                            prev_result = result_obj.get("result").cloned();
                                            let content_scope = note_content_scope(
                                                content_requested,
                                                result_obj["tool"].as_str().unwrap_or_default(),
                                                &result_obj["result"],
                                                &self.registry,
                                            );
                                            let presented = apply_presentation_to_result(
                                                result_obj,
                                                effective_mode,
                                                now_unix,
                                                presentation_policy,
                                                content_scope,
                                            );
                                            budget.record(&presented);
                                            entries.push(presented);
                                        }
                                    }
                                }
                                Err(failure) => {
                                    let mut entry = failure.into_entry();
                                    stamp_usage(&mut entry, &usage_ctx);
                                    budget.record(&entry);
                                    entries.push(entry);
                                    aborted_from = Some(step_index + 1);
                                }
                            }
                        }

                        UnitOutcome {
                            unit_index,
                            entries,
                            content_updates,
                        }
                    },
                }
            })
            .collect();

        let outcomes = execute_bounded_units(unit_tasks, max_batch_concurrency).await;

        let mut results: Vec<Option<Value>> = (0..total).map(|_| None).collect();
        let mut content_updates: Vec<(usize, bool)> = Vec::new();
        for outcome in outcomes {
            let range = ranges[outcome.unit_index].clone();
            for (offset, mut entry) in outcome.entries.into_iter().enumerate() {
                // Additive on this bracketed shape only: an ordinary single,
                // flat batch, or top-level chain response carries none of
                // these fields, unchanged by this amendment.
                if let Some(object) = entry.as_object_mut() {
                    object.insert("op_index".to_string(), json!(range.start + offset));
                    object.insert("unit_index".to_string(), json!(outcome.unit_index));
                    object.insert("step_index".to_string(), json!(offset));
                }
                results[range.start + offset] = Some(entry);
            }
            content_updates.extend(outcome.content_updates);
        }
        let results: Vec<Value> = results
            .into_iter()
            .map(|entry| entry.expect("every leaf in every unit produces exactly one entry"))
            .collect();

        let succeeded = results
            .iter()
            .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
            .count();
        let aborted = results
            .iter()
            .filter(|r| r.get("aborted").and_then(Value::as_bool) == Some(true))
            .count();
        let failed = total - succeeded - aborted;
        let response = json!({
            "results": results,
            "summary": { "total": total, "succeeded": succeeded, "failed": failed, "aborted": aborted },
            "status": batch_status(failed, aborted),
        });
        (response, content_updates)
    }
}
