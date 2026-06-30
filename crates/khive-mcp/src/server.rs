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

use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};

use khive_db::ConnectionPool;
use khive_request::{parse_request, ArgValue, DslError, ExecutionMode, ParsedOp};
use khive_runtime::{
    present, render_format, KhiveRuntime, Namespace, OutputFormat, PackLoadError, PackRegistry,
    PresentationMode, RuntimeConfig, RuntimeError, VerbPresentationPolicy, VerbRegistry,
    VerbRegistryBuilder,
};

use khive_storage::EdgeRelation;

use crate::coordinator::CoordinatorService;
use crate::tools::request::RequestParams;

/// Fingerprint the dispatch-affecting parts of a resolved [`RuntimeConfig`].
///
/// Two servers produce the same id iff they would dispatch identically: same
/// pack set (order-independent), same storage target, and same embedders. The
/// daemon compares this against each forwarded request's `config_id` and rejects
/// mismatches so a restricted client (e.g. `--pack kg`, `--db :memory:`) cannot
/// execute through the broader default daemon. Namespace is carried separately.
///
/// When `khive_cfg` is supplied and contains a non-empty `[[backends]]`
/// declaration, the backend topology (sorted backend list and pack→backend
/// assignments) is folded into the fingerprint so that two configs differing
/// only in pack routing produce different ids (ADR-049 / B-SHOULD-FIX-4).
///
/// When `khive_cfg` is `None` or its `backends` list is empty, the fingerprint
/// is byte-identical to what it would have been before this parameter was added.
pub fn compute_config_id(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
) -> String {
    let mut packs = config.packs.clone();
    packs.sort();
    let db = config
        .db_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ":memory:".to_string());
    let primary = config
        .embedding_model
        .as_ref()
        .map(|m| format!("{m:?}"))
        .unwrap_or_else(|| "none".to_string());
    let mut extra: Vec<String> = config
        .additional_embedding_models
        .iter()
        .map(|m| format!("{m:?}"))
        .collect();
    extra.sort();
    let mut visible: Vec<String> = config
        .visible_namespaces
        .iter()
        .map(|ns| ns.as_str().to_owned())
        .collect();
    visible.sort();
    visible.dedup();
    let mut outbound: Vec<String> = config
        .allowed_outbound_namespaces
        .iter()
        .map(|ns| ns.as_str().to_owned())
        .collect();
    outbound.sort();
    outbound.dedup();

    let base = format!(
        "packs=[{}];db={};embed={};extra=[{}];backend={:?};visible=[{}];outbound=[{}]",
        packs.join(","),
        db,
        primary,
        extra.join(","),
        config.backend_id,
        visible.join(","),
        outbound.join(","),
    );

    // Fold backend topology when non-empty so two configs differing only in
    // pack→backend routing produce different config_ids (ADR-049).
    // When backends is empty this branch is skipped, preserving byte-identity
    // with the pre-change fingerprint.
    let topology = khive_cfg
        .filter(|cfg| !cfg.backends.is_empty())
        .map(|cfg| {
            let mut backend_entries: Vec<String> = cfg
                .backends
                .iter()
                .map(|b| {
                    let path = b
                        .path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| ":memory:".to_string());
                    format!("{}:{:?}:{}", b.name, b.kind, path)
                })
                .collect();
            backend_entries.sort();

            let mut pack_entries: Vec<String> = cfg
                .packs
                .iter()
                .map(|(pack, pc)| format!("{}={}", pack, pc.backend))
                .collect();
            pack_entries.sort();

            format!(
                ";backends=[{}];pack_backends=[{}]",
                backend_entries.join(","),
                pack_entries.join(","),
            )
        })
        .unwrap_or_default();

    format!("{base}{topology}")
}

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
    /// Fingerprint of the resolved runtime config (packs, db target, embedders).
    /// The stdio client passes it to the daemon; a config mismatch triggers
    /// local-dispatch fallback so a restricted client never runs through the
    /// broader default daemon.
    config_id: String,
    /// Cross-backend coordinator (ADR-029 Phase 2). Present only in multi-backend
    /// deployments. `None` in single-backend mode — all dispatch goes through the
    /// `VerbRegistry` unchanged (zero-change invariant).
    coordinator: Option<Arc<dyn CoordinatorService>>,
    /// Pool arc for the WAL checkpoint background task. `None` for in-memory
    /// or registry-only servers that have no persistent database.
    pool: Option<Arc<ConnectionPool>>,
    /// Server-level default output format (ADR-078). Resolved from TOML →
    /// `KHIVE_OUTPUT_FORMAT` → builtin `json`. Per-request `format` fields
    /// override this at dispatch time.
    default_output_format: OutputFormat,
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
        let config_id = compute_config_id(runtime.config(), None);
        let visible_namespaces = runtime.config().visible_namespaces.clone();
        let actor_id = runtime.config().actor_id.clone();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(gate);
        builder.with_default_namespace(default_namespace.as_str());
        builder.with_visible_namespaces(visible_namespaces);
        builder.with_actor_id(actor_id);
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
        // Invoke `PackRuntime::register_entity_type_validator` on every pack so
        // entity-type validation is active at the runtime layer for all write
        // paths, including direct `create_many` callers that bypass the handler.
        registry.call_register_entity_type_validators(&runtime);
        // Apply pack-auxiliary schema plans at startup so pack tables are
        // present before any handler runs. Errors are logged but not propagated
        // so a single pack's schema failure cannot abort startup.
        registry.apply_schema_plans(runtime.backend());
        // Capture the pool arc for the WAL checkpoint task. Only available for
        // file-backed databases; in-memory backends return None here.
        let pool = if runtime.backend().is_file_backed() {
            Some(runtime.backend().pool_arc())
        } else {
            None
        };
        Ok(Self {
            registry,
            default_namespace: default_namespace.as_str().to_string(),
            config_id,
            coordinator: None,
            pool,
            default_output_format: OutputFormat::Json,
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
            // A registry injected directly has no resolved RuntimeConfig; use a
            // sentinel that matches no real daemon so such servers always
            // dispatch locally rather than forward.
            config_id: "registry-only".to_string(),
            coordinator: None,
            pool: None,
            default_output_format: OutputFormat::Json,
        }
    }

    /// Build a server from a pre-built registry with explicit namespace and config_id.
    ///
    /// Used by the multi-backend boot path in `serve.rs` where the registry is
    /// assembled externally before constructing the server.
    pub fn from_registry_with_meta(
        registry: VerbRegistry,
        default_namespace: &str,
        config_id: &str,
    ) -> Self {
        Self {
            registry,
            default_namespace: default_namespace.to_string(),
            config_id: config_id.to_string(),
            coordinator: None,
            pool: None,
            default_output_format: OutputFormat::Json,
        }
    }

    /// Override the server-level default output format (ADR-078).
    ///
    /// Called after construction to wire in the format resolved from
    /// `KHIVE_OUTPUT_FORMAT` or `[runtime] default_output_format` in
    /// `config.toml`. Per-request `format` fields override this at dispatch time.
    pub fn with_default_output_format(mut self, fmt: OutputFormat) -> Self {
        self.default_output_format = fmt;
        self
    }

    /// Attach a cross-backend coordinator (ADR-029 Phase 2).
    ///
    /// Only multi-backend servers need a coordinator. Single-backend servers
    /// leave `coordinator` as `None` (zero-change invariant: all dispatch goes
    /// through `VerbRegistry` unchanged).
    pub fn with_coordinator(mut self, coordinator: Arc<dyn CoordinatorService>) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    /// Attach a connection pool for the WAL checkpoint background task.
    ///
    /// Used by the multi-backend boot path to wire the main backend's pool into a
    /// server built via `from_registry_with_meta` (which cannot carry a pool itself
    /// because registry-only construction has no access to the backend layer).
    pub fn with_pool(mut self, pool: Arc<ConnectionPool>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Clone the verb registry for use by background tasks (e.g. channel polling loops).
    ///
    /// `VerbRegistry` is internally `Arc`-wrapped so this clone is cheap. The returned
    /// registry shares the same packs and dispatch state as the server.
    #[cfg(feature = "channel-email")]
    pub(crate) fn verb_registry_clone(&self) -> VerbRegistry {
        self.registry.clone()
    }

    /// Route a `link` or `search` verb through the coordinator when in multi-backend mode.
    ///
    /// Returns `Some(result)` when the coordinator handled the op (caller should skip
    /// `registry.dispatch`). Returns `None` (fall-through) when:
    /// - no coordinator is attached (`coordinator == None`)
    /// - the coordinator reports a single backend (`is_single_backend()`)
    /// - the verb is not `link` or `search`
    /// - args cannot be extracted for coordinator dispatch (e.g. non-UUID source/target)
    ///
    /// Result semantics mirror the per-op envelope from the registry:
    /// `Ok(Value)` → success payload (caller wraps in `{ok:true, tool, result}`).
    /// `Err((tool, error_value))` → error payload (caller wraps in `{ok:false, tool, error}`).
    async fn dispatch_via_coordinator(
        &self,
        tool: &str,
        args_value: &Value,
    ) -> Option<Result<Value, (String, Value)>> {
        let coord = self.coordinator.as_ref()?;
        if coord.is_single_backend() {
            return None;
        }
        dispatch_via_coordinator_inner(coord.as_ref(), tool, args_value, &self.default_namespace)
            .await
    }

    /// Namespace this server's registry was built for.
    pub fn default_namespace(&self) -> &str {
        &self.default_namespace
    }

    /// Fingerprint of the runtime config this server's registry was built for.
    pub fn config_id(&self) -> &str {
        &self.config_id
    }

    /// The connection pool to use for background WAL checkpointing, if any.
    ///
    /// Returns `None` for in-memory or registry-only servers.
    pub fn pool(&self) -> Option<Arc<ConnectionPool>> {
        self.pool.clone()
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
        from_wire: bool,
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
        // boundary (`from_wire`), never on the operator path (`kkernel exec`,
        // in-process callers). Exception: `help=true` is short-circuited in
        // VerbRegistry::dispatch before reaching the pack, so introspection works.
        let is_help = args_value
            .get("help")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if from_wire && !is_help && self.registry.is_subhandler_verb(&tool) {
            return Err((
                tool.clone(),
                json!(format!(
                    "permission denied for verb {tool:?}: verb '{tool}' is an internal \
                     subhandler and cannot be invoked via the MCP request surface"
                )),
            ));
        }

        // Multi-backend interception: route link/search through the coordinator (ADR-029 D3/D4).
        // Single-backend and non-link/search verbs fall through to the registry unchanged.
        if let Some(coord_result) = self.dispatch_via_coordinator(&tool, &args_value).await {
            return coord_result
                .map(|result| json!({ "ok": true, "tool": tool, "result": result }));
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
        from_wire: bool,
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

                // Clone coordinator and namespace for use in the per-op closures (ADR-029 D3/D4).
                let coordinator: Option<Arc<dyn CoordinatorService>> = self.coordinator.clone();
                let default_namespace = self.default_namespace.clone();

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
                    let coord = coordinator.clone();
                    let ns_str = default_namespace.clone();
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

                        // Multi-backend interception: route link/search through the coordinator
                        // (ADR-029 D3/D4). Falls through to registry for single-backend and
                        // non-link/search verbs.
                        if let Some(active_coord) = coord.as_ref() {
                            if !active_coord.is_single_backend() {
                                if let Some(coord_result) = dispatch_via_coordinator_inner(
                                    active_coord.as_ref(),
                                    &tool,
                                    &args_value,
                                    &ns_str,
                                )
                                .await
                                {
                                    return match coord_result {
                                        Ok(result) => {
                                            let presented =
                                                present(result, effective_mode, now_unix);
                                            json!({ "ok": true, "tool": tool, "result": presented })
                                        }
                                        Err((_, error_payload)) => {
                                            json!({ "ok": false, "tool": tool, "error": error_payload })
                                        }
                                    };
                                }
                            }
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
                    match self.dispatch_op(op, prev_result.as_ref(), from_wire).await {
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

/// Route a `link` or `search` verb through `coord` when in multi-backend mode.
///
/// This is the shared logic behind both dispatch sites (`dispatch_op` chain mode
/// and the parallel/single closure in `run_parsed`). Extracted as a free async
/// function so closures that don't have access to `&self` can call it.
///
/// Returns `Some(Ok(Value))` when the coordinator handled the op successfully.
/// Returns `Some(Err((tool, error_value)))` when the coordinator returned an error.
/// Returns `None` to indicate fall-through (caller should dispatch through the registry).
async fn dispatch_via_coordinator_inner(
    coord: &dyn CoordinatorService,
    tool: &str,
    args_value: &Value,
    namespace_str: &str,
) -> Option<Result<Value, (String, Value)>> {
    let namespace = Namespace::parse(namespace_str).unwrap_or_else(|_| Namespace::local());

    match tool {
        "link" => {
            // Only intercept single-link form (not bulk `links` array).
            // Bulk link falls through to the registry for now.
            if args_value.get("links").is_some() {
                return None;
            }
            let source_str = args_value.get("source_id")?.as_str()?;
            let target_str = args_value.get("target_id")?.as_str()?;
            let relation_str = args_value.get("relation")?.as_str()?;

            // Only intercept when both endpoints are parseable UUIDs.
            // Name/prefix resolution requires single-backend context — fall through.
            let source_id: uuid::Uuid = source_str.parse().ok()?;
            let target_id: uuid::Uuid = target_str.parse().ok()?;
            let relation: EdgeRelation = relation_str.parse().ok()?;

            let weight = args_value
                .get("weight")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let metadata = args_value.get("metadata").cloned();

            let result = coord
                .link(&namespace, source_id, target_id, relation, weight, metadata)
                .await;

            let tool_name = tool.to_string();
            Some(match result {
                Ok(coord_result) => {
                    // Serialize the edge using serde_json — matches `to_json(&edge)` in the kg
                    // handler, which is what `format_edge_output` receives (identity fn).
                    let edge_val = serde_json::to_value(&coord_result.edge)
                        .unwrap_or_else(|e| json!({"error": format!("serialize edge: {e}")}));
                    // Preserve symmetric-relation source/target override that the kg handler
                    // applies: if the edge was written with swapped endpoints, inject the
                    // canonical source/target so callers get what they requested.
                    let mut raw = edge_val;
                    if relation.is_symmetric() {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("source_id".to_string(), json!(source_id.to_string()));
                            obj.insert("target_id".to_string(), json!(target_id.to_string()));
                        }
                    }
                    Ok(raw)
                }
                Err(e) => {
                    let re: RuntimeError = e.into();
                    match re {
                        RuntimeError::Khive(k) => {
                            let error_payload = serde_json::to_value(&k).unwrap_or_else(
                                |_| json!({"kind": "internal", "message": k.to_string()}),
                            );
                            Err((tool_name, error_payload))
                        }
                        other => Err((tool_name, json!(other.to_string()))),
                    }
                }
            })
        }
        "search" => {
            let kind = args_value.get("kind")?.as_str()?;
            let query = args_value.get("query")?.as_str()?;
            let limit = args_value
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v as u32)
                .unwrap_or(10)
                .min(100);
            let score_floor = args_value
                .get("min_score")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0);

            // For substrate-level kinds ("entity" / "note"), pass None so the search
            // is unrestricted. For granular kinds ("concept", "observation", etc.) pass
            // the kind string so each backend filters at the storage layer — matching
            // the behaviour of the single-backend handler (search.rs).
            let kind_filter: Option<&str> = match kind {
                "entity" | "note" => None,
                other => Some(other),
            };

            // Extract entity-substrate filters and forward them to each backend.
            // When either is active the coordinator widens the per-backend candidate
            // window so that sparse matches ranked below the bare limit are not cut
            // off before filtering (before-truncation parity with the single-backend
            // handler in search.rs).
            let props_filter: Option<&serde_json::Value> =
                args_value.get("properties").and_then(|v| {
                    if v.as_object().is_some_and(|m| !m.is_empty()) {
                        Some(v)
                    } else {
                        None
                    }
                });
            // Parse tags strictly: absent/null → no filter (empty Vec); present and
            // valid Vec<String> → use as-is (including empty array → no filter);
            // present but not a Vec<String> → reject with a per-op error so the
            // multi-backend path matches single-backend behaviour, which rejects
            // malformed tags via SearchParams deserialisation (RuntimeError::InvalidInput).
            // filter_map(as_str) would silently drop non-string entries and produce
            // an empty Vec, bypassing the filter and returning unfiltered results.
            let tags_owned: Vec<String> = match args_value.get("tags") {
                None | Some(Value::Null) => vec![],
                Some(v) => match serde_json::from_value::<Vec<String>>(v.clone()) {
                    Ok(t) => t,
                    Err(_) => {
                        return Some(Err((
                            "search".to_string(),
                            json!("tags must be an array of strings"),
                        )));
                    }
                },
            };

            let coord_result = coord
                .fan_out_search(
                    kind,
                    query,
                    &namespace,
                    limit,
                    kind_filter,
                    props_filter,
                    &tags_owned,
                )
                .await;

            // Shape result to match the kg search handler's output fields exactly.
            // Entity hits: [{id, entity_kind, score, title, snippet}]
            //   - entity_kind: real kind string fetched from the owning backend
            //   - score: RRF-merged, subject to min_score floor
            // Note hits:   [{id, note_kind, score, title, snippet}]
            //   - note_kind: real kind string fetched from the owning backend
            let result_val = if !coord_result.note_hits.is_empty()
                || (coord_result.entity_hits.is_empty() && coord_result.note_hits.is_empty())
            {
                // Note substrate or empty — return note-shaped result.
                let items: Vec<Value> = coord_result
                    .note_hits
                    .iter()
                    .filter(|h| h.score.to_f64() >= score_floor)
                    .map(|h| {
                        let note_kind = coord_result.note_kinds.get(&h.note_id);
                        json!({
                            "id": h.note_id.to_string(),
                            "note_kind": note_kind,
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                serde_json::to_value(items).unwrap_or_else(|_| json!([]))
            } else {
                // Entity substrate — return entity-shaped result.
                let items: Vec<Value> = coord_result
                    .entity_hits
                    .iter()
                    .filter(|h| h.score.to_f64() >= score_floor)
                    .map(|h| {
                        let entity_kind = coord_result.entity_kinds.get(&h.entity_id);
                        json!({
                            "id": h.entity_id.to_string(),
                            "entity_kind": entity_kind,
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                serde_json::to_value(items).unwrap_or_else(|_| json!([]))
            };

            Some(Ok(result_val))
        }
        _ => None,
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
            let frame = self.wire_daemon_frame(&p);
            if let Some(res) = crate::daemon::forward_or_spawn(&frame).await {
                return res;
            }
        }
        self.dispatch_request_wire(p).await
    }
}

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
            ops: p.ops.clone(),
            presentation: p.presentation.clone(),
            presentation_per_op: p.presentation_per_op.clone(),
            namespace: self.default_namespace.clone(),
            config_id: self.config_id.clone(),
            protocol_version: khive_runtime::daemon::PROTOCOL_VERSION,
            probe_only: false,
            format: p.format.clone(),
            format_per_op: p.format_per_op.clone(),
            from_wire: true,
        }
    }

    /// Parse and dispatch a request against this server's own registry.
    ///
    /// This is the canonical **operator** dispatch path: subhandler verbs are
    /// allowed. `kkernel exec`, in-process callers, and tests use this. The
    /// agent-facing MCP wire surface goes through `dispatch_request_wire`
    /// (or sets `from_wire` on the daemon frame), which enforces verb visibility.
    pub async fn dispatch_request_local(&self, p: RequestParams) -> Result<String, McpError> {
        self.dispatch_request_inner(p, false).await
    }

    /// Wire-surface dispatch: same as [`Self::dispatch_request_local`] but
    /// enforces verb visibility (`Visibility::Subhandler` verbs are rejected).
    /// Used by the stdio `request` tool's local-fallback path.
    pub(crate) async fn dispatch_request_wire(&self, p: RequestParams) -> Result<String, McpError> {
        self.dispatch_request_inner(p, true).await
    }

    /// Shared body for both dispatch surfaces. `from_wire` decides whether the
    /// subhandler-visibility gate fires (see [`run_parsed`](Self::run_parsed)).
    pub(crate) async fn dispatch_request_inner(
        &self,
        p: RequestParams,
        from_wire: bool,
    ) -> Result<String, McpError> {
        let save_to = p.save_to.clone();
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

        // Resolve the output format for this request (ADR-078 §2 precedence):
        // per-request `format` field → server default (already resolved from
        // env + toml + builtin by `serve.rs`).
        let batch_format = parse_output_format(p.format.as_deref())
            .map_err(|e| McpError::invalid_params(e, None))?
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
                                .map_err(|e| McpError::invalid_params(e, None))?
                                .unwrap_or(batch_format),
                        ),
                    };
                    fmts.push(fmt);
                }
                Some(fmts)
            } else {
                None
            };

        let result = self
            .run_parsed(
                parsed.ops,
                parsed.mode,
                presentation,
                presentation_per_op.clone(),
                from_wire,
            )
            .await;

        if let Some(path_str) = save_to {
            let path = std::path::Path::new(&path_str);
            let manifest = crate::save_sink::write_and_manifest(&result, path)
                .map_err(|e| McpError::internal_error(format!("save_to: {e}"), None))?;
            // Manifests are always compact JSON regardless of format (lossless metadata).
            return serde_json::to_string(&manifest)
                .map_err(|e| McpError::internal_error(format!("serialize manifest: {e}"), None));
        }

        // Apply per-op format rendering (ADR-078 §8.4 and §9).
        Ok(render_result(
            result,
            batch_format,
            &format_per_op,
            presentation,
            &presentation_per_op,
            &self.registry,
        ))
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

/// Parse an optional output format string from the request envelope (ADR-078).
///
/// `None` → `None` (caller uses server default). Known values: `"json"`, `"auto"`, `"table"`.
fn parse_output_format(s: Option<&str>) -> Result<Option<OutputFormat>, String> {
    match s {
        None => Ok(None),
        Some("json") => Ok(Some(OutputFormat::Json)),
        Some("auto") => Ok(Some(OutputFormat::Auto)),
        Some("table") => Ok(Some(OutputFormat::Table)),
        Some(other) => Err(format!(
            "unknown output format {other:?}; valid values: \"json\", \"auto\", \"table\""
        )),
    }
}

/// Render the `run_parsed` result envelope using per-op format dispatch (ADR-078 §8.4).
///
/// For each op entry in `results`:
/// - If `ok=false` (error entry): always compact JSON, never reformatted (§8.2).
/// - If `ok=true`: resolve per-op format (per_op_formats[i] → batch_format) and
///   per-op presentation (presentation_per_op[i] → batch presentation, then the
///   verb's AlwaysVerbose policy forces Verbose), apply `render_format` to the
///   `result` payload with the effective presentation so that both
///   `presentation_per_op=["verbose"]` and AlwaysVerbose verbs (get/link/query/
///   traverse/neighbors/brain.feedback) correctly skip the redundancy-drop
///   pre-pass (ADR-078 §7 + §8.4; mirrors `run_parsed`).
///
/// The outer envelope (`{results:[...], summary:{...}}`) is always compact JSON (§8.4).
/// When the result value is not a compound batch envelope (single-op fast path),
/// the whole value is rendered with `batch_format` and `presentation`.
fn render_result(
    value: serde_json::Value,
    batch_format: OutputFormat,
    format_per_op: &Option<Vec<Option<OutputFormat>>>,
    presentation: PresentationMode,
    presentation_per_op: &Option<Vec<Option<PresentationMode>>>,
    registry: &VerbRegistry,
) -> String {
    // Fast path: if format is json and no per-op overrides, compact-serialize and return.
    if batch_format == OutputFormat::Json && format_per_op.is_none() {
        return serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
    }

    // Try to detect the compound batch envelope shape: { results: [...], summary: {...} }
    if let serde_json::Value::Object(ref map) = value {
        if let Some(serde_json::Value::Array(results)) = map.get("results") {
            let mut out_results = Vec::with_capacity(results.len());
            for (i, entry) in results.iter().enumerate() {
                let per_op_fmt = format_per_op
                    .as_ref()
                    .and_then(|v| v.get(i))
                    .and_then(|x| *x)
                    .unwrap_or(batch_format);

                // Resolve per-op presentation: per-op entry overrides batch default,
                // then the AlwaysVerbose verb policy forces Verbose — mirroring the
                // resolution `run_parsed` applies before presentation. Without this,
                // a policy-verbose verb (get/link/query/traverse/neighbors/
                // brain.feedback) dispatched under format=auto/table with the default
                // Agent presentation would be redundancy-dropped at the format seam,
                // stripping the namespace/properties it is declared AlwaysVerbose
                // precisely to preserve.
                let base_presentation = presentation_per_op
                    .as_ref()
                    .and_then(|v| v.get(i))
                    .and_then(|o| *o)
                    .unwrap_or(presentation);
                let effective_presentation =
                    match entry.get("tool").and_then(serde_json::Value::as_str) {
                        Some(tool)
                            if registry.presentation_policy_for(tool)
                                == VerbPresentationPolicy::AlwaysVerbose =>
                        {
                            PresentationMode::Verbose
                        }
                        _ => base_presentation,
                    };

                // Error entries are never reformatted (§8.2).
                let is_ok = entry
                    .get("ok")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !is_ok || per_op_fmt == OutputFormat::Json {
                    out_results.push(entry.clone());
                    continue;
                }

                // For successful entries, render the `result` sub-value with the
                // effective presentation so verbose ops skip the redundancy drop.
                if let Some(result_val) = entry.get("result") {
                    let rendered =
                        render_format(result_val.clone(), per_op_fmt, effective_presentation);
                    // Replace `result` with the rendered string.
                    let mut new_entry = entry.clone();
                    if let serde_json::Value::Object(ref mut emap) = new_entry {
                        emap.insert("result".to_string(), serde_json::Value::String(rendered));
                    }
                    out_results.push(new_entry);
                } else {
                    out_results.push(entry.clone());
                }
            }

            // Rebuild envelope: results rendered per-op, summary always compact.
            let mut out_map = map.clone();
            out_map.insert("results".to_string(), serde_json::Value::Array(out_results));
            return serde_json::to_string(&serde_json::Value::Object(out_map))
                .unwrap_or_else(|_| "null".to_string());
        }
    }

    // Single-op or unknown shape: render the whole value with batch_format and presentation.
    render_format(value, batch_format, presentation)
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
