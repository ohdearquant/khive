//! KhiveMcpServer — rmcp-based MCP server exposing a single `request` tool.
//!
//! Accepts the function-call DSL or JSON form and dispatches each parsed operation
//! through the [`VerbRegistry`] built from the configured packs.
//!
// FILE SIZE JUSTIFICATION: `run_parsed` is long because it encodes the
// execution-mode contract (Single/Parallel/Chain) as a single match
// expression. Splitting the three branches into separate functions would
// scatter the contract invariants (summary shape, aborted semantics,
// $prev substitution ordering) across files, making them harder to review
// as a unit. The module is the authoritative implementation of request
// dispatch and is intentionally co-located.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};

use khive_request::{parse_request, ArgValue, DslError, ExecutionMode, ParsedOp};
use khive_runtime::{
    present, KhiveRuntime, PackLoadError, PackRegistry, PresentationMode, RuntimeError,
    VerbPresentationPolicy, VerbRegistry, VerbRegistryBuilder,
};

use crate::tools::request::RequestParams;

/// Build a sorted, human-readable verb catalog from `(pack_name, verb_name, description)` triples.
///
/// When multiple packs register the same verb name, each pack's description is
/// emitted on its own continuation line with a `[pack]` prefix so the caller can
/// see every contributing pack. A `tracing::warn!` is emitted once per duplicate.
fn build_verb_catalog(verbs: impl IntoIterator<Item = (String, String, String)>) -> String {
    let mut by_verb: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for (pack_name, verb_name, description) in verbs {
        by_verb
            .entry(verb_name)
            .or_default()
            .push((pack_name, description));
    }
    let mut out = String::new();
    for (name, pack_descs) in &by_verb {
        if pack_descs.len() > 1 {
            let packs: Vec<&str> = pack_descs.iter().map(|(p, _)| p.as_str()).collect();
            tracing::warn!(
                verb = %name,
                packs = ?packs,
                "verb registered by multiple packs; all descriptions included in catalog"
            );
        }
        out.push_str("  ");
        out.push_str(name);
        out.push_str(" — ");
        if pack_descs.len() == 1 {
            out.push_str(&pack_descs[0].1);
        } else {
            for (i, (pack, desc)) in pack_descs.iter().enumerate() {
                if i > 0 {
                    out.push_str("\n    ");
                }
                out.push('[');
                out.push_str(pack);
                out.push_str("] ");
                out.push_str(desc);
            }
        }
        out.push('\n');
    }
    out
}

/// MCP server that dispatches all verbs through a [`VerbRegistry`].
#[derive(Clone)]
pub struct KhiveMcpServer {
    registry: VerbRegistry,
    /// Namespace this registry was built for. The stdio client passes it to the
    /// daemon; a namespace mismatch triggers local-dispatch fallback.
    default_namespace: String,
}

/// Failure reason inside a [`PackRegError`].
pub enum PackRegFailure {
    UnknownPack(String),
    MissingDependency { pack: String, dep: String },
    Registry(khive_runtime::RuntimeError),
}

/// Returned by [`KhiveMcpServer::with_packs`] when pack registration fails.
/// The original runtime is returned so the caller can recover.
pub struct PackRegError {
    pub failure: PackRegFailure,
    pub runtime: KhiveRuntime,
}

impl std::fmt::Debug for PackRegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut dbg = f.debug_struct("PackRegError");
        match &self.failure {
            PackRegFailure::UnknownPack(unknown) => dbg.field("unknown", unknown),
            PackRegFailure::MissingDependency { pack, dep } => {
                dbg.field("pack", pack).field("missing_dep", dep)
            }
            PackRegFailure::Registry(source) => dbg.field("source", source),
        }
        .finish_non_exhaustive()
    }
}

impl std::fmt::Display for PackRegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.failure {
            PackRegFailure::UnknownPack(unknown) => write!(
                f,
                "unknown pack name {:?} — built-in packs: {}",
                unknown,
                builtin_pack_names().join(", ")
            ),
            PackRegFailure::MissingDependency { pack, dep } => write!(
                f,
                "pack {pack:?} requires {dep:?}, which is not in the requested pack list; \
                 add --pack {dep} before --pack {pack}"
            ),
            PackRegFailure::Registry(source) => write!(f, "pack registry build failed: {source}"),
        }
    }
}

impl std::error::Error for PackRegError {}

/// Built-in pack names known to this binary.
///
/// Sourced from `PackRegistry::discovered_names()` so the list always reflects
/// whatever pack crates are linked into the binary.
pub fn builtin_pack_names() -> Vec<&'static str> {
    PackRegistry::discovered_names()
}

impl KhiveMcpServer {
    /// Build a server from `runtime.config().packs`. Errors if any pack is unknown or missing deps.
    // The error variant intentionally carries the runtime so callers can recover.
    #[allow(clippy::result_large_err)]
    pub fn new(runtime: KhiveRuntime) -> Result<Self, PackRegError> {
        let packs: Vec<String> = runtime.config().packs.clone();
        // Fail-fast on bad packs so callers can decide recovery.
        // Schema plan application happens inside with_packs.
        Self::with_packs(runtime, &packs)
    }

    /// Build a server with an explicit pack list (strict — fails on unknown names).
    // The error variant intentionally carries the runtime by value so callers
    // can recover and retry. Boxing would force every recovery path through a
    // deref for no real benefit.
    #[allow(clippy::result_large_err)]
    pub fn with_packs(runtime: KhiveRuntime, packs: &[String]) -> Result<Self, PackRegError> {
        let gate = runtime.config().gate.clone();
        let default_namespace = runtime.config().default_namespace.clone();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(gate);
        builder.with_default_namespace(default_namespace.as_str());
        // Wire the EventStore into the registry for audit persistence.
        if let Ok(tok) = runtime.authorize(khive_runtime::Namespace::local()) {
            if let Ok(event_store) = runtime.events(&tok) {
                builder.with_event_store(event_store);
            }
        }
        if let Err(load_err) = PackRegistry::register_packs(packs, runtime.clone(), &mut builder) {
            let failure = match load_err {
                PackLoadError::UnknownPack(name) => PackRegFailure::UnknownPack(name),
                PackLoadError::MissingDependency { pack, dep } => {
                    PackRegFailure::MissingDependency { pack, dep }
                }
            };
            return Err(PackRegError { failure, runtime });
        }
        let registry = builder.build().map_err(|source| PackRegError {
            failure: PackRegFailure::Registry(source),
            runtime: runtime.clone(),
        })?;
        // Aggregate pack-declared edge endpoint rules into the runtime
        // so `validate_edge_relation_endpoints` can consult them.
        runtime.install_edge_rules(registry.all_edge_rules());
        // Invoke `PackRuntime::register_embedders` on every pack so custom
        // embedding providers are available before the first verb dispatch.
        // Must happen after the registry is built (packs are ordered)
        // and before any `remember`/`recall` calls that would resolve embedders.
        registry.call_register_embedders(&runtime);
        // Apply pack-auxiliary schema plans at startup so pack tables are
        // present before any handler runs. Errors are logged but not propagated
        // so a single pack's schema failure cannot abort startup.
        registry.apply_schema_plans(runtime.backend());
        Ok(Self {
            registry,
            default_namespace: default_namespace.as_str().to_string(),
        })
    }

    /// Build a server directly from a pre-configured registry.
    ///
    /// Intended for tests that need to inject mock packs (e.g. packs that
    /// return `RuntimeError::Khive` to exercise structured error serialization).
    /// Production code should use [`Self::new`] or [`Self::with_packs`].
    #[doc(hidden)]
    pub fn from_registry(registry: VerbRegistry) -> Self {
        Self {
            registry,
            default_namespace: "local".to_string(),
        }
    }

    /// Namespace this server's registry was built for.
    pub fn default_namespace(&self) -> &str {
        &self.default_namespace
    }

    /// Warm every pack's in-memory state. Called by the daemon in a background
    /// task after the socket is bound.
    pub async fn warm_all(&self) {
        self.registry.call_warm_all().await;
    }

    /// Serve over stdio (blocks until the connection closes).
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        use rmcp::transport::stdio;
        let service = self.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Build the textual verb catalog included in the request tool's description.
    ///
    /// The list is rebuilt from the runtime registry so it always reflects which
    /// packs are actually loaded.
    fn verb_catalog(&self) -> String {
        let verbs = self
            .registry
            .all_verbs_with_names()
            .into_iter()
            .map(|(pack, v)| (pack.to_owned(), v.name.to_owned(), v.description.to_owned()));
        build_verb_catalog(verbs)
    }

    /// Dispatch a single [`ParsedOp`] by resolving its args (potentially
    /// substituting `$prev` references) and calling the [`VerbRegistry`].
    ///
    /// Returns a per-op result object: `{ok, tool, result}` on success or
    /// `{ok: false, tool, error}` on failure.
    async fn dispatch_op(
        &self,
        op: ParsedOp,
        prev_result: Option<&Value>,
    ) -> Result<Value, (String, Value)> {
        let ParsedOp { tool, args } = op;

        // Resolve args — substitute $prev references when prev_result is Some.
        // Handles flat PrevRef as well as Array/Object containing nested refs.
        let mut resolved: serde_json::Map<String, Value> = serde_json::Map::new();
        for (name, arg_val) in args {
            let needs_prev = !matches!(&arg_val, ArgValue::Value(_));
            let value = if needs_prev {
                let prev = prev_result.ok_or_else(|| {
                    (
                        tool.clone(),
                        json!({
                            "kind": "substitution_error",
                            "message": format!(
                                "argument {name:?}: $prev reference in non-chain context"
                            )
                        }),
                    )
                })?;
                let resolved_val = arg_val.resolve_all(prev).ok_or_else(|| {
                    // Include available top-level fields in the error message,
                    // matching the UX of the bare-$prev guard.
                    let fields_hint = if let Value::Object(map) = prev {
                        let mut fields: Vec<&str> =
                            map.keys().map(String::as_str).collect();
                        fields.sort_unstable();
                        format!(
                            " Available top-level fields: [{}]",
                            fields.join(", ")
                        )
                    } else {
                        String::new()
                    };
                    (
                        tool.clone(),
                        json!({
                            "kind": "substitution_error",
                            "message": format!(
                                "argument {name:?}: one or more $prev paths not found in prior result.{fields_hint}"
                            ),
                        }),
                    )
                })?;
                // UE4-H1: bare `$prev` (no path) resolving to a map or array
                // will cause a confusing downstream type error. Detect it here and
                // surface a clear substitution error with available field names.
                if matches!(&arg_val, ArgValue::PrevRef { path } if path.is_empty()) {
                    match &resolved_val {
                        Value::Object(map) => {
                            let fields: Vec<&str> = map.keys().map(String::as_str).collect();
                            return Err((
                                tool.clone(),
                                json!({
                                    "kind": "substitution_error",
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
                            return Err((
                                tool.clone(),
                                json!({
                                    "kind": "substitution_error",
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
        // boundary. Internal callers that call VerbRegistry::dispatch directly
        // are not affected. Exception: `help=true` is short-circuited in
        // VerbRegistry::dispatch before reaching the pack, so introspection works.
        let is_help = args_value
            .get("help")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_help && self.registry.is_subhandler_verb(&tool) {
            return Err((
                tool.clone(),
                json!(format!(
                    "permission denied for verb {tool:?}: verb '{tool}' is an internal \
                     subhandler and cannot be invoked via the MCP request surface"
                )),
            ));
        }

        match self.registry.dispatch(&tool, args_value).await {
            Ok(result) => Ok(json!({ "ok": true, "tool": tool, "result": result })),
            Err(RuntimeError::Khive(k)) => {
                let error_payload = serde_json::to_value(&k)
                    .unwrap_or_else(|_| json!({ "kind": "internal", "message": k.to_string() }));
                Err((tool, error_payload))
            }
            Err(e) => Err((tool, json!(e.to_string()))),
        }
    }

    /// Execute a parsed request, dispatching according to its [`ExecutionMode`].
    ///
    /// - `Single` / `Parallel`: all ops run concurrently; per-op failure does
    ///   not abort siblings. `aborted` count is always 0.
    /// - `Chain`: ops run sequentially; `$prev` from each op's result is
    ///   substituted into the next op's args. If any op fails (or a `$prev`
    ///   substitution fails), remaining ops appear as `aborted: true`.
    ///
    /// Presentation transforms are applied per-op AFTER dispatch,
    /// using `mode_for_op` to determine the mode per position. Chain `$prev`
    /// substitution uses canonical (verbose) handler output; the transform runs
    /// only at the final response-envelope boundary.
    ///
    /// Response envelope:
    /// ```json
    /// {
    ///   "results": [...],
    ///   "summary": { "total": N, "succeeded": K, "failed": M, "aborted": A }
    /// }
    /// ```
    async fn run_parsed(
        &self,
        ops: Vec<ParsedOp>,
        mode: ExecutionMode,
        presentation: PresentationMode,
        presentation_per_op: Option<Vec<Option<PresentationMode>>>,
    ) -> Value {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .unwrap_or(0);

        // Resolve per-op presentation mode: per-op entry overrides batch default.
        let mode_for_op = |i: usize| -> PresentationMode {
            presentation_per_op
                .as_ref()
                .and_then(|v| v.get(i))
                .and_then(|o| *o)
                .unwrap_or(presentation)
        };

        match mode {
            ExecutionMode::Single | ExecutionMode::Parallel => {
                // Write-key conflict preflight.
                //
                // Detect ops that target the same write key in the same parallel/single
                // batch. Conflicting ops receive per-op error entries; non-conflicting ops
                // execute normally. `results.length == summary.total` is preserved.
                let conflict_indices: std::collections::HashSet<usize> = {
                    let mut seen: std::collections::HashMap<String, usize> =
                        std::collections::HashMap::new();
                    let mut bad: std::collections::HashSet<usize> =
                        std::collections::HashSet::new();
                    for (i, op) in ops.iter().enumerate() {
                        for key in khive_request::write_keys_for_op_pub(op) {
                            if let Some(&prior) = seen.get(&key) {
                                bad.insert(prior);
                                bad.insert(i);
                            } else {
                                seen.insert(key, i);
                            }
                        }
                    }
                    bad
                };

                // Independent dispatch — run all concurrently, results in input order.
                let futures = ops.into_iter().enumerate().map(|(i, op)| {
                    let conflict_with: Option<String> = if conflict_indices.contains(&i) {
                        Some(format!(
                            "conflict: writes overlap with another op in this batch (op #{})",
                            i
                        ))
                    } else {
                        None
                    };

                    let registry = self.registry.clone();
                    let op_mode = mode_for_op(i);
                    async move {
                        let tool = op.tool.clone();
                        // Conflicting ops get a per-op error; skip dispatch.
                        if let Some(msg) = conflict_with {
                            return json!({ "ok": false, "tool": tool, "error": msg });
                        }
                        // AlwaysVerbose verbs override the caller's presentation mode.
                        let effective_mode =
                            if registry.presentation_policy_for(&tool)
                                == VerbPresentationPolicy::AlwaysVerbose
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
                                prev_error = Some(json!({
                                    "ok": false,
                                    "tool": tool,
                                    "error": format!(
                                        "argument {name:?}: $prev reference is only valid in chain (|) mode"
                                    )
                                }));
                                break;
                            }
                        }
                        if let Some(err) = prev_error {
                            return err;
                        }
                        let args_value = Value::Object(resolved);

                        // Block subhandler verbs at the MCP wire boundary.
                        // Exception: help=true is short-circuited in
                        // VerbRegistry::dispatch before the pack, so
                        // introspection passes through.
                        let is_help = args_value
                            .get("help")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if !is_help && registry.is_subhandler_verb(&tool) {
                            return json!({
                                "ok": false,
                                "tool": tool,
                                "error": format!(
                                    "permission denied for verb {tool:?}: verb '{tool}' is an \
                                     internal subhandler and cannot be invoked via the MCP \
                                     request surface"
                                )
                            });
                        }

                        match registry.dispatch(&tool, args_value).await {
                            Ok(result) => {
                                let presented = present(result, effective_mode, now_unix);
                                json!({ "ok": true, "tool": tool, "result": presented })
                            }
                            Err(RuntimeError::Khive(k)) => {
                                let error_payload = serde_json::to_value(&k).unwrap_or_else(
                                    |_| json!({ "kind": "internal", "message": k.to_string() }),
                                );
                                json!({ "ok": false, "tool": tool, "error": error_payload })
                            }
                            Err(e) => json!({ "ok": false, "tool": tool, "error": e.to_string() }),
                        }
                    }
                });
                let results: Vec<Value> = futures::future::join_all(futures).await;
                let total = results.len();
                let succeeded = results
                    .iter()
                    .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
                    .count();
                let failed = total - succeeded;
                json!({
                    "results": results,
                    "summary": { "total": total, "succeeded": succeeded, "failed": failed, "aborted": 0 },
                })
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
                    if aborted_from.is_some() {
                        // A prior op failed — mark remaining as aborted.
                        results.push(json!({ "ok": false, "tool": op.tool, "aborted": true }));
                        continue;
                    }
                    let op_mode = mode_for_op(i);
                    // AlwaysVerbose verbs override the caller's presentation mode.
                    let effective_mode = if self.registry.presentation_policy_for(&op.tool)
                        == VerbPresentationPolicy::AlwaysVerbose
                    {
                        PresentationMode::Verbose
                    } else {
                        op_mode
                    };
                    match self.dispatch_op(op, prev_result.as_ref()).await {
                        Ok(result_obj) => {
                            // Extract canonical result for $prev (pre-presentation).
                            prev_result = result_obj.get("result").cloned();
                            // Apply presentation to the result field only,
                            // using the effective mode (AlwaysVerbose override honored).
                            let presented_obj =
                                apply_presentation_to_result(result_obj, effective_mode, now_unix);
                            results.push(presented_obj);
                        }
                        Err((tool, error_payload)) => {
                            results
                                .push(json!({ "ok": false, "tool": tool, "error": error_payload }));
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
                })
            }
        }
    }
}

/// Apply the presentation transform to the `result` field of a successful
/// per-op envelope, leaving error envelopes unchanged.
///
/// Error envelopes are never transformed — only successful `result` fields.
fn apply_presentation_to_result(
    mut result_obj: Value,
    mode: PresentationMode,
    now_unix: i64,
) -> Value {
    if result_obj.get("ok").and_then(Value::as_bool) == Some(true) {
        if let Some(result_field) = result_obj.get("result").cloned() {
            let presented = present(result_field, mode, now_unix);
            if let Some(obj) = result_obj.as_object_mut() {
                obj.insert("result".to_string(), presented);
            }
        }
    }
    result_obj
}

// ── single MCP tool ─────────────────────────────────────────────────────────

#[tool_router]
impl KhiveMcpServer {
    #[tool(description = r#"Run one or more khive verbs in a single MCP call.

ops syntax:

  Single op   : verb(name=value, name=value)
  Batch       : [verb(...), verb(...)]                 — parallel, max 100
  Chain       : verb1(...) | verb2(id=$prev.id)        — sequential, $prev
  JSON form   : [{"tool":"verb","args":{...}}, ...]    — INDEPENDENT ops only

Argument values are JSON literals: strings (double-quoted), numbers, booleans,
null, arrays, objects. Strings may contain commas / parens; escape with \".

Chain-only: $prev resolves to the prior op's result. Path extraction syntax:
  $prev               — full result
  $prev.field         — nested object field
  $prev.items[0].id   — array index
  $prev[2]            — top-level array index
Quoted strings that contain $prev are promoted to substitutions (e.g. id="$prev.id"
is the same as id=$prev.id). To pass a literal "$prev", escape with backslash:
\"\\$prev\". JSON form is for independent ops only — any $prev string in JSON
form is rejected.

Response shape:

  {
    "results": [ {"ok": true, "tool": "verb", "result": {...}}, ... ],
    "summary": { "total": N, "succeeded": N, "failed": N, "aborted": N }
  }

Parallel: a failed op does NOT abort siblings. Chain: failure aborts remaining
ops (reported as {"ok": false, "aborted": true}). Committed ops are not rolled back.

Verb discovery: install the `kg` / `gtd` plugins for usage skills. The verbs
currently registered on this server (pack-derived) are listed below. Argument
schemas live in each pack's docs and SKILL.md files.

Tip: for one-shot calls, the single-op form is the densest. Use batch when
several independent ops can run together; use chain when each op needs the prior
result (e.g. create then link with the new entity's id)."#)]
    async fn request(&self, Parameters(p): Parameters<RequestParams>) -> Result<String, McpError> {
        // Forward to the warm daemon when reachable, auto-spawning it
        // on first use. Any failure (no socket, spawn failure, namespace
        // mismatch, KHIVE_NO_DAEMON) falls through to local dispatch.
        #[cfg(unix)]
        {
            let frame = khive_runtime::DaemonRequestFrame {
                ops: p.ops.clone(),
                presentation: p.presentation.clone(),
                presentation_per_op: p.presentation_per_op.clone(),
                namespace: self.default_namespace.clone(),
            };
            if let Some(res) = crate::daemon::forward_or_spawn(&frame).await {
                return res;
            }
        }
        self.dispatch_request_local(p).await
    }
}

impl KhiveMcpServer {
    /// Parse and dispatch a request against this server's own registry.
    ///
    /// This is the canonical dispatch path. The stdio `request` tool calls it
    /// only as a fallback; the daemon calls it directly (never through the tool
    /// wrapper), so there is no risk of a daemon forwarding to itself.
    pub async fn dispatch_request_local(&self, p: RequestParams) -> Result<String, McpError> {
        let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;

        // Parse presentation strings → PresentationMode.
        let presentation = parse_presentation_mode(p.presentation.as_deref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let presentation_per_op: Option<Vec<Option<PresentationMode>>> =
            if let Some(per_op_strs) = p.presentation_per_op {
                let mut modes = Vec::with_capacity(per_op_strs.len());
                for s in per_op_strs {
                    let mode = match s.as_deref() {
                        None => None,
                        Some(v) => Some(
                            parse_presentation_mode(Some(v))
                                .map_err(|e| McpError::invalid_params(e, None))?,
                        ),
                    };
                    modes.push(mode);
                }
                Some(modes)
            } else {
                None
            };

        let result = self
            .run_parsed(parsed.ops, parsed.mode, presentation, presentation_per_op)
            .await;
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))
    }
}

fn dsl_err_to_mcp(e: DslError) -> McpError {
    McpError::invalid_params(e.to_string(), None)
}

/// Parse an optional presentation mode string from the request envelope.
///
/// `None` → default (`Agent`). Known values: `"agent"`, `"verbose"`, `"human"`.
fn parse_presentation_mode(s: Option<&str>) -> Result<PresentationMode, String> {
    match s {
        None | Some("agent") => Ok(PresentationMode::Agent),
        Some("verbose") => Ok(PresentationMode::Verbose),
        Some("human") => Ok(PresentationMode::Human),
        Some(other) => Err(format!(
            "unknown presentation mode {other:?}; valid values: \"agent\", \"verbose\", \"human\""
        )),
    }
}

#[tool_handler]
impl ServerHandler for KhiveMcpServer {
    fn get_info(&self) -> ServerInfo {
        let catalog = self.verb_catalog();
        let builtins = builtin_pack_names().join(", ");
        let instructions = format!(
            "khive — request-only MCP surface. One tool, `request`, \
             dispatches verbs through the loaded pack registry. Configure packs via \
             KHIVE_PACKS or --pack (built-ins: {builtins}). Verbs registered on this \
             server:\n{catalog}\nFor detailed usage of each verb, see the corresponding \
             plugin's SKILL.md files."
        );
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions)
    }

    /// Override the macro-generated `list_tools` so the `request` tool's
    /// description carries the dynamic verb catalog built from the loaded
    /// pack registry. Many MCP clients only surface `tools/list` descriptions
    /// (not server instructions) — discovery must work via tool listing.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        let mut tools = Self::tool_router().list_all();
        let catalog = self.verb_catalog();
        for t in &mut tools {
            if t.name == "request" {
                let base = t.description.as_deref().unwrap_or("");
                t.description = Some(std::borrow::Cow::Owned(format!(
                    "{base}\n\nVerbs registered on this server:\n{catalog}"
                )));
            }
        }
        Ok(rmcp::model::ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::build_verb_catalog;

    fn t(pack: &str, verb: &str, desc: &str) -> (String, String, String) {
        (pack.to_owned(), verb.to_owned(), desc.to_owned())
    }

    #[test]
    fn single_pack_verbs_unchanged() {
        let catalog = build_verb_catalog([
            t("kg", "create", "Create an entity or note."),
            t("kg", "list", "List entities."),
        ]);
        assert_eq!(
            catalog,
            "  create — Create an entity or note.\n  list — List entities.\n"
        );
    }

    #[test]
    fn duplicate_verb_concatenates_descriptions_with_pack_attribution() {
        let catalog = build_verb_catalog([
            t("kg", "create", "Create an entity or note."),
            t("gtd", "create", "Create a task."),
        ]);
        // Both pack descriptions must appear with attribution.
        assert!(catalog.contains("[kg] Create an entity or note."));
        assert!(catalog.contains("[gtd] Create a task."));
        // The verb name must appear exactly once in the catalog header.
        assert_eq!(catalog.matches("  create — ").count(), 1);
    }

    #[test]
    fn catalog_is_sorted_alphabetically() {
        let catalog = build_verb_catalog([
            t("kg", "search", "Search."),
            t("kg", "assign", "Assign."),
            t("kg", "list", "List."),
        ]);
        let names: Vec<&str> = catalog
            .lines()
            .filter(|l| l.starts_with("  "))
            .map(|l| l.trim_start().split(' ').next().unwrap())
            .collect();
        assert_eq!(names, vec!["assign", "list", "search"]);
    }
}
