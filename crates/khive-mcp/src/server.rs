//! KhiveMcpServer — rmcp-based MCP server exposing a single `request` tool.
//!
//! The MCP surface is intentionally minimal: one tool (`request`) that accepts
//! a function-call DSL or JSON form (ADR-020) and dispatches each parsed
//! operation through the [`VerbRegistry`] built from the packs declared in
//! [`khive_runtime::RuntimeConfig::packs`].
//!
//! ## Why a single tool
//!
//! As of v0.2 the verb-flat surface (ADR-023) is folded behind `request`. The
//! verb catalog lives in:
//!
//! - the `request` tool's description (terse list, per-pack);
//! - each pack's marketplace plugin SKILL.md files (rich usage guides).
//!
//! Tool discovery happens once per session anyway, so collapsing 16+ flat tools
//! into one keeps tool-list latency low and frees agent context budget while
//! preserving expressiveness through the DSL.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};

use khive_request::{parse_request, ArgValue, DslError, ExecutionMode, ParsedOp};
use khive_runtime::{
    present, KhiveRuntime, PackRegistry, PresentationMode, RuntimeError, VerbPresentationPolicy,
    VerbRegistry, VerbRegistryBuilder,
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
}

/// Failure reason inside a [`PackRegError`].
pub enum PackRegFailure {
    UnknownPack(String),
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
            PackRegFailure::Registry(source) => write!(f, "pack registry build failed: {source}"),
        }
    }
}

impl std::error::Error for PackRegError {}

/// Built-in pack names known to this binary.
///
/// Sourced from `PackRegistry::discovered_names()` so the list always reflects
/// whatever pack crates are linked into the binary (ADR-027).
pub fn builtin_pack_names() -> Vec<&'static str> {
    PackRegistry::discovered_names()
}

impl KhiveMcpServer {
    /// Build a server using the pack list from `runtime.config().packs`.
    ///
    /// The authorization gate from `runtime.config().gate` is threaded into the
    /// registry. Gate decisions are **hard-enforcing** in v0.3 — a `Deny`
    /// result blocks pack dispatch and returns `PermissionDenied` (ADR-035).
    ///
    /// Fails fast if any requested pack is unknown or has an unsatisfied
    /// dependency (ADR-027). A misconfigured `KHIVE_PACKS` is a boot error —
    /// callers must list all required packs explicitly. Use [`Self::with_packs`]
    /// for the same strict path with an explicit pack list.
    ///
    /// # Errors
    ///
    /// Returns [`PackRegError`] if any pack in `runtime.config().packs` is
    /// unknown or if a declared dependency is absent from the list.
    // The error variant intentionally carries the runtime so callers can recover.
    #[allow(clippy::result_large_err)]
    pub fn new(runtime: KhiveRuntime) -> Result<Self, PackRegError> {
        let packs: Vec<String> = runtime.config().packs.clone();
        // ADR-014 (c14 hardening): fail-fast on bad packs so callers can decide
        // recovery. The c12 schema_plan application happens inside with_packs.
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
        // ADR-035: wire the EventStore into the registry for audit persistence.
        if let Ok(event_store) =
            runtime.events(&runtime.authorize(khive_runtime::Namespace::local()))
        {
            builder.with_event_store(event_store);
        }
        if let Err(unknown) = PackRegistry::register_packs(packs, runtime.clone(), &mut builder) {
            return Err(PackRegError {
                failure: PackRegFailure::UnknownPack(unknown),
                runtime,
            });
        }
        let registry = builder.build().map_err(|source| PackRegError {
            failure: PackRegFailure::Registry(source),
            runtime: runtime.clone(),
        })?;
        // ADR-031: aggregate pack-declared edge endpoint rules into the runtime
        // so `validate_edge_relation_endpoints` can consult them.
        runtime.install_edge_rules(registry.all_edge_rules());
        // ADR-031 extension: invoke `PackRuntime::register_embedders` on every
        // pack so custom embedding providers are available before the first verb
        // dispatch. Must happen after the registry is built (packs are ordered)
        // and before any `remember`/`recall` calls that would resolve embedders.
        registry.call_register_embedders(&runtime);
        // ADR-017 §c12: apply pack-auxiliary schema plans at startup so pack
        // tables are present before any handler runs. Errors are logged but
        // not propagated so a single pack's schema failure cannot abort startup.
        registry.apply_schema_plans(runtime.backend());
        Ok(Self { registry })
    }

    /// Build a server directly from a pre-configured registry.
    ///
    /// Intended for tests that need to inject mock packs (e.g. packs that
    /// return `RuntimeError::Khive` to exercise structured error serialization).
    /// Production code should use [`Self::new`] or [`Self::with_packs`].
    #[doc(hidden)]
    pub fn from_registry(registry: VerbRegistry) -> Self {
        Self { registry }
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
                arg_val.resolve_all(prev).ok_or_else(|| {
                    (
                        tool.clone(),
                        json!({
                            "kind": "substitution_error",
                            "message": format!(
                                "argument {name:?}: one or more $prev paths not found in prior result"
                            ),
                        }),
                    )
                })?
            } else {
                match arg_val {
                    ArgValue::Value(v) => v,
                    _ => unreachable!(),
                }
            };
            resolved.insert(name, value);
        }

        let args_value = Value::Object(resolved);

        // ADR-017 §Visibility: Subhandler verbs are operator-only.
        // Block them at the MCP wire boundary; internal callers that call
        // VerbRegistry::dispatch directly are not affected.
        // Exception: `help=true` is short-circuited in VerbRegistry::dispatch
        // before reaching the pack — pass it through so introspection works.
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
    /// Presentation transforms (ADR-045) are applied per-op AFTER dispatch,
    /// using `mode_for_op` to determine the mode per position. Chain `$prev`
    /// substitution uses canonical (verbose) handler output; the transform runs
    /// only at the final response-envelope boundary.
    ///
    /// Response envelope (ADR-016):
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
            .map(|d| d.as_secs() as i64)
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
                // Independent dispatch — run all concurrently, results in input order.
                let futures = ops.into_iter().enumerate().map(|(i, op)| {
                    let registry = self.registry.clone();
                    let op_mode = mode_for_op(i);
                    async move {
                        let tool = op.tool.clone();
                        // ADR-045 §6: AlwaysVerbose verbs override the caller's mode.
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

                        // ADR-017 §Visibility: block Subhandler verbs at the
                        // MCP wire boundary.  Exception: help=true is
                        // short-circuited in VerbRegistry::dispatch before the
                        // pack — pass it through so introspection works.
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
                // only at the final response-envelope boundary (ADR-045 §4).
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
                    // ADR-045 §6: AlwaysVerbose verbs override the caller's mode.
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
/// Per ADR-045 §3.5: "Error envelopes are NEVER transformed."
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

ops syntax (ADR-016):

  Single op   : verb(name=value, name=value)
  Batch       : [verb(...), verb(...)]                 — parallel, max 100
  Chain       : verb1(...) | verb2(id=$prev.id)        — sequential, $prev
  JSON form   : [{"tool":"verb","args":{...}}, ...]    — equivalent

Argument values are JSON literals: strings (double-quoted), numbers, booleans,
null, arrays, objects. Strings may contain commas / parens; escape with \".
Chain-only: $prev resolves to the prior op's result; $prev.field.path extracts
a nested field.

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
        let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;

        // Parse presentation strings → PresentationMode (ADR-045).
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
            "khive — request-only MCP surface (ADR-020 + ADR-027). One tool, `request`, \
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
    /// (not server instructions) — ADR-027 requires discovery to work there.
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
