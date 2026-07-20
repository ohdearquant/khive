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
use sha2::{Digest, Sha256};

use khive_db::ConnectionPool;
use khive_request::{parse_request, ArgValue, DslError, ExecutionMode, ParsedOp};
use khive_runtime::{
    present, render_format, resolve_explicit_namespace, KhiveRuntime, OutputFormat, PackLoadError,
    PackRegistry, PresentationMode, RuntimeConfig, RuntimeError, VerbPresentationPolicy,
    VerbRegistry, VerbRegistryBuilder,
};

use khive_storage::EdgeRelation;

use crate::coordinator::CoordinatorService;
use crate::tools::request::RequestParams;

/// Fingerprint the engine-coherence parts of a resolved [`RuntimeConfig`].
///
/// Two servers produce the same id iff they can safely share one warm engine:
/// same pack set (order-independent), same storage target, same embedders, same
/// backend topology/routing, and same construction-baked outbound and git-write
/// policies.
/// Identity fields (`namespace`, `actor_id`, `visible_namespaces`) are carried
/// per request in the daemon frame and must never enter this key. The daemon
/// compares this against each forwarded request's `config_id` and rejects
/// mismatches so a restricted client (e.g. `--pack kg`, `--db :memory:`) cannot
/// execute through the broader default daemon.
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
    let mut outbound: Vec<String> = config
        .allowed_outbound_namespaces
        .iter()
        .map(|ns| ns.as_str().to_owned())
        .collect();
    outbound.sort();
    outbound.dedup();
    let mut git_write_hasher = Sha256::new();
    git_write_hasher.update(b"khive.git-write-policy.v1");
    git_write_hasher.update((config.git_write.allowed.len() as u64).to_be_bytes());
    for entry in &config.git_write.allowed {
        git_write_hasher.update((entry.repo.len() as u64).to_be_bytes());
        git_write_hasher.update(entry.repo.as_bytes());
        git_write_hasher.update((entry.branches.len() as u64).to_be_bytes());
        for branch in &entry.branches {
            git_write_hasher.update((branch.len() as u64).to_be_bytes());
            git_write_hasher.update(branch.as_bytes());
        }
    }
    let git_write = format!("{:x}", git_write_hasher.finalize());

    let base = format!(
        "packs=[{}];db={};embed={};extra=[{}];backend={:?};outbound=[{}];git_write={}",
        packs.join(","),
        db,
        primary,
        extra.join(","),
        config.backend_id,
        outbound.join(","),
        git_write,
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
    /// File-backed backend pools beyond `pool` (ADR-091 Amendment 3
    /// fan-out): every additional backend a multi-backend boot wired, so the
    /// session sweep and the daemon's checkpoint ownership can cover them
    /// too. Always empty for a single-backend server — `pool` alone is that
    /// server's one backend.
    secondary_pools: Vec<Arc<ConnectionPool>>,
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

/// Which MCP handshake mode [`KhiveMcpServer::serve_stdio`] should use for
/// this process instance (#714). Unix-only: the resumed-generation self-heal
/// re-exec this decides between requires `crate::daemon`'s Unix-only
/// mismatch-recovery machinery (in turn only ever armed by a Unix-domain-socket
/// daemon-forwarding protocol mismatch); non-Unix `serve_stdio` always takes
/// the plain handshake path (see its `#[cfg(not(unix))]` variant below).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdioServeMode {
    /// Normal MCP `initialize` handshake — the overwhelmingly common case.
    Handshake,
    /// Skip the handshake (`serve_directly`): this process is a resumed
    /// generation of a prior self-heal re-exec (`crate::daemon`, #714 §2.3).
    Resumed,
}

/// Pure decision behind [`StdioServeMode`], factored out so it is
/// unit-testable without driving real stdio I/O. `resumed_generation` is
/// [`crate::daemon::resumed_generation`]'s return value.
#[cfg(unix)]
fn stdio_serve_mode_for(resumed_generation: Option<u32>) -> StdioServeMode {
    match resumed_generation {
        Some(_) => StdioServeMode::Resumed,
        None => StdioServeMode::Handshake,
    }
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
        // #750: install pack-owned note-mutation hooks (currently
        // only khive-pack-memory's warm-ANN-cache invalidation) so KG's
        // update/delete verbs notify caching packs even though there is no
        // crate-level dependency between them.
        registry.call_register_note_mutation_hooks(&runtime);
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
            secondary_pools: Vec::new(),
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
            secondary_pools: Vec::new(),
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
            secondary_pools: Vec::new(),
            default_output_format: OutputFormat::Json,
        }
    }

    /// Override the server-level default output format (ADR-078).
    ///
    /// Called after construction to wire in the format resolved from
    /// `KHIVE_OUTPUT_FORMAT` or `[runtime] default_output_format` in
    /// `khive.toml`. Per-request `format` fields override this at dispatch time.
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

    /// Attach every file-backed backend pool beyond the main one (ADR-091
    /// Amendment 3 fan-out), so the session sweep and the daemon's
    /// checkpoint task can cover the full multi-backend deployment instead
    /// of only `pool`.
    pub fn with_secondary_pools(mut self, pools: Vec<Arc<ConnectionPool>>) -> Self {
        self.secondary_pools = pools;
        self
    }

    /// Clone the verb registry for use by background tasks (e.g. channel polling loops).
    ///
    /// `VerbRegistry` is internally `Arc`-wrapped so this clone is cheap. The returned
    /// registry shares the same packs and dispatch state as the server.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
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
    ///
    /// `identity` mirrors the override [`Self::dispatch_op`] applies to the
    /// registry path (ADR-096 Fork 1): when present, its namespace is used
    /// instead of `self.default_namespace` so a per-request identity can't
    /// diverge between the coordinator intercept and the registry dispatch
    /// it falls through to.
    async fn dispatch_via_coordinator(
        &self,
        tool: &str,
        args_value: &Value,
        identity: Option<&khive_runtime::RequestIdentity>,
    ) -> Option<Result<Value, (String, Value)>> {
        let coord = self.coordinator.as_ref()?;
        if coord.is_single_backend() {
            return None;
        }
        let default_namespace = identity
            .map(|id| id.namespace.as_str())
            .unwrap_or(self.default_namespace.as_str());
        dispatch_via_coordinator_inner(coord.as_ref(), tool, args_value, default_namespace).await
    }

    /// Namespace this server's registry was built for.
    pub fn default_namespace(&self) -> &str {
        &self.default_namespace
    }

    /// Fingerprint of the runtime config this server's registry was built for.
    pub fn config_id(&self) -> &str {
        &self.config_id
    }

    /// This server's resolved actor identity label, if configured (ADR-057).
    ///
    /// Read when building the daemon request frame (ADR-096 Fork 1) to carry
    /// this server's own identity on the wire, so a warm daemon with a
    /// different baked identity serves the request under this caller's
    /// actor instead of the daemon's.
    pub fn actor_id(&self) -> Option<&str> {
        self.registry.actor_id()
    }

    /// This server's resolved extra read-visibility namespaces (ADR-007
    /// Rev 4 Rule 3b). See [`Self::actor_id`] for why this is exposed
    /// (ADR-096 Fork 1).
    pub fn visible_namespaces(&self) -> &[khive_runtime::Namespace] {
        self.registry.visible_namespaces()
    }

    /// The connection pool to use for background WAL checkpointing, if any.
    ///
    /// Returns `None` for in-memory or registry-only servers.
    pub fn pool(&self) -> Option<Arc<ConnectionPool>> {
        self.pool.clone()
    }

    /// File-backed backend pools beyond [`Self::pool`] (ADR-091 Amendment 3
    /// fan-out). Empty for a single-backend server.
    pub fn secondary_pools(&self) -> Vec<Arc<ConnectionPool>> {
        self.secondary_pools.clone()
    }

    /// This server's configured audit `EventStore`, if any (ADR-094).
    ///
    /// Exposed so the `DaemonDispatch::event_store_for_checkpoint` impl and
    /// the email channel poll loop can append best-effort lifecycle events
    /// to the same sink gate-check audit rows already use, without a second
    /// constructor argument threaded everywhere a registry is built.
    pub fn event_store(&self) -> Option<Arc<dyn khive_storage::EventStore>> {
        self.registry.event_store()
    }

    /// The server-level default output format (ADR-078), as resolved at
    /// construction by [`crate::serve::apply_env_output_format`].
    pub fn default_output_format(&self) -> OutputFormat {
        self.default_output_format
    }

    /// Warm every pack's in-memory state. Called by the daemon in a background
    /// task after the socket is bound.
    pub async fn warm_all(&self) {
        self.registry.call_warm_all().await;
    }

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
    /// Both branches wrap the raw stdio transport in
    /// `crate::daemon::SelfHealOnFlushTransport` — the actual happens-after
    /// edge that fires an armed self-heal re-exec (or drain-and-exit) only
    /// once a message has genuinely finished flushing to the client, never
    /// on a fixed timer that could race a slow or backpressured stdout.
    #[cfg(unix)]
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        use rmcp::transport::{async_rw::AsyncRwTransport, stdio};

        let build_transport = || {
            let (read, write) = stdio();
            crate::daemon::SelfHealOnFlushTransport::new(AsyncRwTransport::new_server(read, write))
        };

        match stdio_serve_mode_for(crate::daemon::resumed_generation()) {
            StdioServeMode::Resumed => {
                let service = rmcp::service::serve_directly(self, build_transport(), None);
                service.waiting().await?;
            }
            StdioServeMode::Handshake => {
                let service = self.serve(build_transport()).await?;
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
    /// normal MCP `initialize` handshake directly over the raw stdio
    /// transport, with no resumed-generation skip and no flush-triggered hook.
    #[cfg(not(unix))]
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
        identity: Option<&khive_runtime::RequestIdentity>,
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
        if let Some(coord_result) = self
            .dispatch_via_coordinator(&tool, &args_value, identity)
            .await
        {
            return coord_result.and_then(|result| chain_ok_envelope_or_depth_error(tool, result));
        }

        match self
            .registry
            .dispatch_with_identity(&tool, args_value, identity.cloned())
            .await
        {
            Ok(result) => chain_ok_envelope_or_depth_error(tool, result),
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
        identity: Option<&khive_runtime::RequestIdentity>,
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
                // ADR-096 Fork 1: a per-request identity overrides the default
                // namespace for both the coordinator intercept and the registry
                // dispatch below, so the two can't drift out of sync per op.
                let identity_owned: Option<khive_runtime::RequestIdentity> = identity.cloned();

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
                    let ns_str = identity_owned
                        .as_ref()
                        .map(|id| id.namespace.clone())
                        .unwrap_or_else(|| default_namespace.clone());
                    let op_identity = identity_owned.clone();
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
                                        Ok(result) => present_ok_envelope_or_depth_error(
                                            tool,
                                            result,
                                            effective_mode,
                                            now_unix,
                                        ),
                                        Err((_, error_payload)) => {
                                            json!({ "ok": false, "tool": tool, "error": error_payload })
                                        }
                                    };
                                }
                            }
                        }

                        match registry
                            .dispatch_with_identity(&tool, args_value, op_identity)
                            .await
                        {
                            Ok(result) => present_ok_envelope_or_depth_error(
                                tool,
                                result,
                                effective_mode,
                                now_unix,
                            ),
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
                    match self
                        .dispatch_op(op, prev_result.as_ref(), from_wire, identity)
                        .await
                    {
                        Ok(result_obj) => {
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
                                    let presented_obj = apply_presentation_to_result(
                                        result_obj,
                                        effective_mode,
                                        now_unix,
                                    );
                                    results.push(presented_obj);
                                }
                            }
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
/// Shared logic behind both dispatch sites (`dispatch_op` chain mode and the
/// parallel/single closure in `run_parsed`). Returns `Some(Ok(Value))` when
/// the coordinator handled the op, `Some(Err((tool, error_value)))` on a
/// coordinator error (including fail-closed namespace rejection), `None` to
/// fall through to the registry. Must apply the exact same fail-closed
/// namespace rule as `VerbRegistry::dispatch` (RUNTIME-AUD-002, #433) — see
/// `crates/khive-mcp/docs/api/coordinator.md`.
async fn dispatch_via_coordinator_inner(
    coord: &dyn CoordinatorService,
    tool: &str,
    args_value: &Value,
    default_namespace_str: &str,
) -> Option<Result<Value, (String, Value)>> {
    // Only link/search are ever intercepted here — resolve/validate the
    // namespace only for those verbs so unrelated verbs (which always
    // fall through to `None` below) don't pay for a parse they don't need.
    if !matches!(tool, "link" | "search") {
        return None;
    }

    let namespace = match resolve_explicit_namespace(args_value, default_namespace_str) {
        Ok(ns) => ns,
        Err(e) => {
            return Some(Err(match e {
                RuntimeError::Khive(k) => {
                    let error_payload = serde_json::to_value(&k)
                        .unwrap_or_else(|_| json!({"kind": "internal", "message": k.to_string()}));
                    (tool.to_string(), error_payload)
                }
                other => (tool.to_string(), json!(other.to_string())),
            }));
        }
    };

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
            // Parse strictly as u32 (matching the single-backend `SearchParams { limit:
            // Option<u32> }` contract) instead of parsing as u64 and casting — `as u32`
            // wraps values above `u32::MAX` (e.g. 4294967297 as u32 == 1) before the
            // `.min(100)` cap ever runs, silently truncating a huge limit to a near-empty
            // result set rather than rejecting it (MCP-AUD-003).
            let limit = match args_value.get("limit") {
                None | Some(Value::Null) => 10,
                Some(v) => match serde_json::from_value::<u32>(v.clone()) {
                    Ok(limit) => limit.min(100),
                    Err(_) => {
                        return Some(Err((
                            "search".to_string(),
                            json!("limit must be an unsigned 32-bit integer"),
                        )));
                    }
                },
            };
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

/// Returns `true` when a raw handler `result` value's container nesting is
/// within [`khive_request::NESTING_DEPTH_LIMIT`]. Callers MUST call this on
/// the raw value straight out of coordinator/registry dispatch, before any
/// recursive `Value` operation (clone, serialize, presentation transform)
/// touches it — see `crates/khive-mcp/docs/design.md` (Result depth guard).
fn result_within_depth_limit(result: &Value) -> bool {
    khive_request::value_nesting_within_limit(result, khive_request::NESTING_DEPTH_LIMIT)
}

/// Per-op error payload for a handler result that failed
/// [`result_within_depth_limit`]. Carries only the configured depth limit,
/// never the oversized value itself.
fn depth_error_payload(context: &str) -> Value {
    json!({
        "kind": "result_too_deep",
        "message": format!(
            "op result nesting depth exceeds max {}{context}",
            khive_request::NESTING_DEPTH_LIMIT
        ),
    })
}

/// Build the `{ok: true, tool, result}` envelope for a successful op,
/// without re-serializing an already-owned `Value` through `json!` (which
/// would call `serde_json::to_value` and recurse over the whole tree
/// again). The depth check must already have passed before this is called.
fn ok_envelope(tool: String, result: Value) -> Value {
    let mut map = serde_json::Map::with_capacity(3);
    map.insert("ok".to_string(), Value::Bool(true));
    map.insert("tool".to_string(), Value::String(tool));
    map.insert("result".to_string(), result);
    Value::Object(map)
}

/// Discard a rejected over-limit `Value` without native recursion.
///
/// `Value`'s derived `Drop` walks nested containers the same way `Clone`
/// and `Serialize` do, so simply letting a pathologically deep `result`
/// fall out of scope after the depth guard rejects it would trade a stack
/// overflow during serialization for one during drop. Draining containers
/// onto an explicit heap-allocated worklist keeps each removal O(1) on the
/// call stack regardless of nesting depth.
fn drop_value_iteratively(value: Value) {
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.into_values()),
            _ => {}
        }
    }
}

/// Chain-mode (`dispatch_op`) success path: check the raw handler `result`
/// against the depth guard before it is ever cloned into `$prev` context or
/// wrapped in the response envelope. On violation returns a `result_too_deep`
/// error that does not embed the oversized value, and discards the rejected
/// value iteratively so its own drop can't overflow the stack either.
fn chain_ok_envelope_or_depth_error(tool: String, result: Value) -> Result<Value, (String, Value)> {
    if !result_within_depth_limit(&result) {
        drop_value_iteratively(result);
        return Err((
            tool,
            depth_error_payload("; cannot be used as $prev chain context"),
        ));
    }
    Ok(ok_envelope(tool, result))
}

/// Parallel/single-mode success path: check the raw handler `result` against
/// the depth guard *before* it is handed to `present` (which recurses
/// natively over `Value` in agent mode) or wrapped in the response envelope.
/// On violation returns a `result_too_deep` per-op error entry that does not
/// embed the oversized value, and discards the rejected value iteratively
/// (see [`drop_value_iteratively`]).
fn present_ok_envelope_or_depth_error(
    tool: String,
    result: Value,
    mode: PresentationMode,
    now_unix: i64,
) -> Value {
    if !result_within_depth_limit(&result) {
        drop_value_iteratively(result);
        return json!({ "ok": false, "tool": tool, "error": depth_error_payload("") });
    }
    let presented = present(result, mode, now_unix);
    ok_envelope(tool, presented)
}

/// Returns `true` if a dispatched op's canonical `result` field nests
/// container values (`[`/`{`) deeper than [`khive_request::NESTING_DEPTH_LIMIT`].
///
/// This is a second, defense-in-depth check retained on the chain-mode
/// aggregation path in [`KhiveMcpServer::run_parsed`]: by the time it runs,
/// [`chain_ok_envelope_or_depth_error`] has already screened the same
/// `result` field inside `dispatch_op`, so this should never trip in
/// practice. It stays cheap (iterative, not recursive) so keeping it costs
/// nothing and catches a future refactor that bypasses the earlier guard.
fn result_exceeds_depth_limit(result_obj: &Value) -> bool {
    result_obj
        .get("result")
        .is_some_and(|v| !result_within_depth_limit(v))
}

/// Chain-mode aggregation-loop seam in [`KhiveMcpServer::run_parsed`]:
/// defense-in-depth depth check on a dispatched op's full `result_obj`
/// envelope (should never trip — `dispatch_op` already screened `result`).
/// Returns the unchanged envelope on success, or an already-built error
/// entry on rejection. See `crates/khive-mcp/docs/design.md` (Result depth
/// guard) for why the rejected envelope is drained iteratively.
fn chain_aggregation_depth_reject(result_obj: Value) -> Result<Value, Value> {
    if result_exceeds_depth_limit(&result_obj) {
        let tool_name = result_obj
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let error_entry = json!({
            "ok": false,
            "tool": tool_name,
            "error": {
                "kind": "result_too_deep",
                "message": format!(
                    "op result nesting depth exceeds max {}; \
                     cannot be used as $prev chain context",
                    khive_request::NESTING_DEPTH_LIMIT
                ),
            },
        });
        drop_value_iteratively(result_obj);
        return Err(error_entry);
    }
    Ok(result_obj)
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
        #[cfg(unix)]
        if p.save_to.is_none() {
            let frame = self.wire_daemon_frame(&p);
            if let Some(res) = crate::daemon::forward_or_spawn(&frame).await {
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
        }
        self.dispatch_request_wire(p).await
    }
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
fn strict_fallback_envelope_response(
    p: &RequestParams,
    reason: String,
) -> Result<String, McpError> {
    let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;
    let total = parsed.ops.len();
    let error_msg = format!(
        "daemon fallback rejected under KHIVE_DAEMON_STRICT=1: reason={reason}; \
         refusing to complete the request via local dispatch; \
         rebuild with `make local` and retry"
    );

    let results: Vec<Value> = match parsed.mode {
        ExecutionMode::Chain => parsed
            .ops
            .iter()
            .enumerate()
            .map(|(i, op)| {
                if i == 0 {
                    json!({ "ok": false, "tool": op.tool, "error": error_msg })
                } else {
                    json!({ "ok": false, "tool": op.tool, "aborted": true })
                }
            })
            .collect(),
        ExecutionMode::Single | ExecutionMode::Parallel => parsed
            .ops
            .iter()
            .map(|op| json!({ "ok": false, "tool": op.tool, "error": error_msg }))
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
    }))
    .expect("envelope of string/bool JSON values always serializes"))
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
            // ADR-096 Fork 1: carry this server's OWN resolved actor/visibility
            // identity on the frame so a warm daemon with a *different* baked
            // identity serves the request under this caller's identity instead
            // of rejecting it or silently stamping writes under its own actor.
            actor_id: self.actor_id().map(str::to_string),
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
        self.dispatch_request_inner(p, false, None).await
    }

    /// Wire-surface dispatch: same as [`Self::dispatch_request_local`] but
    /// enforces verb visibility (`Visibility::Subhandler` verbs are rejected).
    /// Used by the stdio `request` tool's local-fallback path.
    pub(crate) async fn dispatch_request_wire(&self, p: RequestParams) -> Result<String, McpError> {
        self.dispatch_request_inner(p, true, None).await
    }

    /// Shared body for both dispatch surfaces. `from_wire` decides whether the
    /// subhandler-visibility gate fires (see [`run_parsed`](Self::run_parsed)).
    ///
    /// `identity` is the per-request identity context threaded from a daemon
    /// frame (ADR-096 Fork 1, see `crate::daemon`'s `DaemonDispatch` impl).
    /// `None` for every local (non-daemon-served) call — this server's own
    /// baked identity applies, exactly as before this parameter existed.
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
    ) -> Result<String, McpError> {
        let save_to = p.save_to.clone();
        let identity = identity.or_else(|| {
            p.request_id
                .map(|request_id| khive_runtime::RequestIdentity {
                    namespace: self.default_namespace.clone(),
                    actor_id: self.actor_id().map(str::to_string),
                    visible_namespaces: self
                        .visible_namespaces()
                        .iter()
                        .map(|ns| ns.as_str().to_string())
                        .collect(),
                    request_id: Some(request_id),
                })
        });
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
                identity.as_ref(),
            )
            .await;

        if let Some(path_str) = save_to {
            let path = std::path::Path::new(&path_str);
            // `from_wire` gates the destination policy: the agent-facing MCP
            // `request` tool (`from_wire = true`) restricts `save_to` to the
            // allowed export root; the trusted operator CLI path
            // (`kkernel exec --save-file`, `from_wire = false`) is unrestricted,
            // matching its documented "write anywhere" behavior.
            let manifest = crate::save_sink::write_and_manifest(&result, path, from_wire)
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

/// Build the `initialize` instructions string from the verb catalog and the
/// loaded builtin pack names. Extracted from [`ServerHandler::get_info`] so
/// the docs-pointer section (#594) is unit-testable without standing up a
/// full server.
fn build_instructions(catalog: &str, builtins: &str) -> String {
    format!(
        "khive — request-only MCP surface. One tool, `request`, \
         dispatches verbs through the loaded pack registry. Configure packs via \
         KHIVE_PACKS or --pack (built-ins: {builtins}). Verbs registered on this \
         server:\n{catalog}\nFor detailed usage of each verb, see the corresponding \
         plugin's SKILL.md files.\n\
         Docs: https://ohdearquant.github.io/khive/ (hosted) or docs/*.md in the repo \
         checkout. Treat the live verb catalog above and help=true as authoritative over \
         cached/training knowledge. Config/backend issues: docs/configuration.md. Usage \
         patterns: docs/guide/tips-and-tricks.md."
    )
}

#[tool_handler]
impl ServerHandler for KhiveMcpServer {
    fn get_info(&self) -> ServerInfo {
        let catalog = self.verb_catalog();
        let builtins = builtin_pack_names().join(", ");
        let instructions = build_instructions(&catalog, &builtins);
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
    use super::*;
    use khive_runtime::Namespace;
    use khive_storage::{EventFilter, PageRequest};
    use serial_test::serial;

    fn t(pack: &str, verb: &str, desc: &str) -> (String, String, String) {
        (pack.to_owned(), verb.to_owned(), desc.to_owned())
    }

    // ── serve_stdio handshake-mode decision (#714) ────────────────────────────

    #[cfg(unix)]
    #[test]
    fn stdio_serve_mode_cold_start_uses_handshake() {
        assert_eq!(stdio_serve_mode_for(None), StdioServeMode::Handshake);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_serve_mode_resumed_generation_skips_handshake() {
        assert_eq!(stdio_serve_mode_for(Some(1)), StdioServeMode::Resumed);
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
    fn instructions_carry_docs_address_and_guidance_pointers() {
        let instructions = build_instructions("  create — Create an entity or note.\n", "kg, gtd");
        assert!(instructions.contains("https://ohdearquant.github.io/khive/"));
        assert!(instructions.contains("docs/configuration.md"));
        assert!(instructions.contains("docs/guide/tips-and-tricks.md"));
        // help=true / live-catalog-over-training-knowledge guidance present.
        assert!(instructions.contains("help=true"));
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

    // ── #658 regression: brain dispatch hook wired into production builder ──

    /// The hook (registered via `PackInstall::dispatch_hook`) and the pack
    /// runtime the registry dispatches `brain.*` verbs to must be the same
    /// `BrainPack` instance — otherwise the hook's posterior updates would be
    /// invisible to `brain.state` reads. `brain.state` loads the default
    /// namespace into the shared active slot as a side effect, so a
    /// subsequent non-brain dispatch in the same namespace lands on
    /// `ApplyTarget::ActiveSlot` and is immediately observable.
    ///
    /// Uses the `local` namespace (rather than an arbitrary one) because
    /// ADR-007 Rule 3b always pins the implicit write token to `local`
    /// regardless of the registry's configured default namespace; using
    /// `local` for both keeps the dispatched event's namespace and the
    /// token's namespace identical, so the signal lands on the active slot
    /// instead of the cold-namespace queue.
    #[tokio::test]
    async fn brain_dispatch_hook_updates_state_visible_through_same_instance() {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "brain".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let server = KhiveMcpServer::with_packs(runtime, &["kg".to_string(), "brain".to_string()])
            .expect("server builds with kg + brain");

        server
            .registry
            .dispatch("brain.state", serde_json::Value::Null)
            .await
            .expect("brain.state loads the default namespace into the active slot");

        server
            .registry
            .dispatch("stats", serde_json::json!({}))
            .await
            .expect("kg.stats dispatch succeeds");

        let state = server
            .registry
            .dispatch("brain.state", serde_json::Value::Null)
            .await
            .expect("brain.state dispatch");
        let total_events = state["balanced_recall"]["total_events"]
            .as_u64()
            .unwrap_or(0);
        assert!(
            total_events > 0,
            "dispatch hook must update the same BrainPack instance the registry \
             dispatches brain.* verbs to; got snapshot {state:?}"
        );
    }

    // ── #823: runtime `$prev` result depth guard ────────────────────────────

    /// Iteratively (no native recursion) wrap `leaf` in `depth` nested
    /// single-key objects, a synthetic stand-in for a pathologically deep
    /// handler result (e.g. from `traverse`/`context`) that would otherwise
    /// overflow the stack when cloned into `$prev` chain context.
    ///
    /// Builds each level via a direct `Map` insert rather than `json!` — the
    /// `json!` object-literal arm calls `serde_json::to_value(&v)` on the
    /// accumulated value, which would walk the whole tree built so far on
    /// every iteration (recursing to the current depth each time) and
    /// overflow the stack itself well before reaching `depth` large enough
    /// to exercise the guard under test.
    fn nest_object(depth: usize, leaf: Value) -> Value {
        let mut v = leaf;
        for _ in 0..depth {
            let mut map = serde_json::Map::with_capacity(1);
            map.insert("nested".to_string(), v);
            v = Value::Object(map);
        }
        v
    }

    #[test]
    fn deep_nested_result_over_limit_is_flagged() {
        let deep = nest_object(
            khive_request::NESTING_DEPTH_LIMIT + 5,
            json!({"leaf": true}),
        );
        let result_obj = json!({ "ok": true, "tool": "traverse", "result": deep });
        assert!(
            result_exceeds_depth_limit(&result_obj),
            "result nested past NESTING_DEPTH_LIMIT must be flagged"
        );
    }

    #[test]
    fn result_at_exactly_the_depth_limit_is_not_flagged() {
        // A scalar leaf (not a container) so the wrapping objects alone land
        // exactly at NESTING_DEPTH_LIMIT containers deep.
        let at_limit = nest_object(khive_request::NESTING_DEPTH_LIMIT, json!(true));
        let result_obj = json!({ "ok": true, "tool": "traverse", "result": at_limit });
        assert!(
            !result_exceeds_depth_limit(&result_obj),
            "result nested exactly at the limit must still be usable as $prev context"
        );
    }

    #[test]
    fn shallow_result_is_not_flagged() {
        let shallow = json!({"a": {"b": {"c": 1}}});
        let result_obj = json!({ "ok": true, "tool": "get", "result": shallow });
        assert!(!result_exceeds_depth_limit(&result_obj));
    }

    #[test]
    fn result_missing_field_is_not_flagged() {
        let result_obj = json!({ "ok": false, "tool": "get", "error": "not found" });
        assert!(!result_exceeds_depth_limit(&result_obj));
    }

    #[test]
    fn chain_aggregation_seam_rejects_over_limit_result_via_iterative_drop() {
        // Directly exercises the post-hoc aggregation-loop guard in
        // `run_parsed`'s `Chain` arm (isolated as
        // `chain_aggregation_depth_reject`) with a value nested well past
        // NESTING_DEPTH_LIMIT. If this branch let the rejected `result_obj`
        // fall out of scope instead of routing it through
        // `drop_value_iteratively`, `Value`'s derived recursive `Drop` would
        // overflow the stack on a value this deep — so this test failing to
        // complete (rather than merely asserting wrong) is itself the
        // regression signal for #823's post-hoc-rejection finding.
        let deep = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
        // Built via direct `Map` inserts, not `json!({..., "result": deep})`:
        // the object-literal macro arm calls `serde_json::to_value(&deep)` on
        // the already-deep value, which would recurse over the whole tree
        // and overflow the stack while constructing the fixture itself,
        // before the guard under test ever runs (see `nest_object` above).
        let mut envelope = serde_json::Map::with_capacity(3);
        envelope.insert("ok".to_string(), Value::Bool(true));
        envelope.insert("tool".to_string(), Value::String("traverse".to_string()));
        envelope.insert("result".to_string(), deep);
        let result_obj = Value::Object(envelope);

        let err = chain_aggregation_depth_reject(result_obj)
            .expect_err("result nested past NESTING_DEPTH_LIMIT must be rejected");

        assert_eq!(err["ok"], json!(false));
        assert_eq!(err["tool"], json!("traverse"));
        assert_eq!(err["error"]["kind"], json!("result_too_deep"));
        // The error entry must never embed the oversized value itself.
        assert!(err.get("result").is_none());
    }

    #[test]
    fn chain_aggregation_seam_accepts_result_within_limit_unchanged() {
        let shallow = json!({ "ok": true, "tool": "get", "result": {"a": {"b": 1}} });
        let accepted = chain_aggregation_depth_reject(shallow.clone())
            .expect("result within the limit must be passed through unchanged");
        assert_eq!(accepted, shallow);
    }

    // ── earliest-seam guard: raw handler `Value` before json!/present/clone ──
    //
    // These exercise `chain_ok_envelope_or_depth_error` and
    // `present_ok_envelope_or_depth_error` directly with a synthetic
    // over-limit `Value` — no DSL parsing involved, standing in for a mock
    // handler whose result is pathologically deep regardless of how shallow
    // the caller's own op args were. This is the earliest point in
    // `dispatch_op` / `run_parsed`'s parallel closure where the raw value is
    // available, strictly before it is ever cloned, presented, or passed
    // through `json!`/`serde_json::to_value`.

    #[test]
    fn chain_seam_rejects_over_limit_result_before_envelope_build() {
        // Deep enough that native recursion (json!/to_value/present) over
        // this value would be a real stack risk; the guard must reject it
        // via the iterative checker without ever attempting that recursion.
        let pathological = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
        let err = chain_ok_envelope_or_depth_error("traverse".to_string(), pathological)
            .expect_err("over-limit result must be rejected, not enveloped");
        assert_eq!(err.0, "traverse");
        assert_eq!(err.1["kind"], json!("result_too_deep"));
        // The error payload must never embed the oversized value itself.
        assert!(err.1.get("result").is_none());
        assert!(err.1.get("nested").is_none());
    }

    #[test]
    fn chain_seam_accepts_at_limit_result_and_moves_value_without_reserializing() {
        let at_limit = nest_object(khive_request::NESTING_DEPTH_LIMIT, json!("leaf"));
        let envelope = chain_ok_envelope_or_depth_error("get".to_string(), at_limit.clone())
            .expect("result at exactly the limit must be accepted");
        assert_eq!(envelope["ok"], json!(true));
        assert_eq!(envelope["tool"], json!("get"));
        assert_eq!(envelope["result"], at_limit);
    }

    #[test]
    fn parallel_seam_rejects_over_limit_result_before_present() {
        let pathological = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
        let envelope = present_ok_envelope_or_depth_error(
            "context".to_string(),
            pathological,
            PresentationMode::Agent,
            0,
        );
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["tool"], json!("context"));
        assert_eq!(envelope["error"]["kind"], json!("result_too_deep"));
        assert!(envelope["error"].get("result").is_none());
    }

    #[test]
    fn parallel_seam_accepts_shallow_result_and_applies_presentation() {
        let shallow = json!({"id": "11111111-1111-1111-1111-111111111111"});
        let envelope = present_ok_envelope_or_depth_error(
            "get".to_string(),
            shallow,
            PresentationMode::Verbose,
            0,
        );
        assert_eq!(envelope["ok"], json!(true));
        assert_eq!(
            envelope["result"]["id"],
            json!("11111111-1111-1111-1111-111111111111")
        );
    }

    #[tokio::test]
    async fn chain_with_deep_accumulated_prev_result_errors_cleanly() {
        // Real end-to-end reproduction: chain N `create` ops where each step's
        // `properties.inner` embeds the previous op's full `properties` via
        // `$prev.properties`. Each op's own DSL args stay shallow (well under
        // NESTING_DEPTH_LIMIT), but the accumulated *runtime result* nests one
        // level deeper per chain step, the exact CWE-674 shape the parser's
        // syntax-tree guard cannot see. Past the limit this must surface a
        // clean per-op `result_too_deep` error and abort the remaining chain,
        // never attempting to clone/serialize the unbounded value.
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

        let steps = khive_request::NESTING_DEPTH_LIMIT + 6;
        let mut dsl = String::from(
            r#"create(kind="entity", entity_kind="concept", name="d0", properties={"n": 0})"#,
        );
        for i in 1..steps {
            dsl.push_str(&format!(
                r#" | create(kind="entity", entity_kind="concept", name="d{i}", properties={{"inner": $prev.properties}})"#
            ));
        }

        let parsed = parse_request(&dsl).expect("each op's own args stay shallow; DSL must parse");
        assert_eq!(parsed.mode, ExecutionMode::Chain);

        let response = server
            .run_parsed(
                parsed.ops,
                parsed.mode,
                PresentationMode::Verbose,
                None,
                false,
                None,
            )
            .await;

        let results = response["results"]
            .as_array()
            .expect("results must be an array");
        assert_eq!(results.len(), steps);

        let failure_idx = results
            .iter()
            .position(|r| r["ok"] == json!(false))
            .expect("accumulated nesting must trip the depth guard before the chain completes");
        assert_eq!(
            results[failure_idx]["error"]["kind"],
            json!("result_too_deep"),
            "unexpected failure shape at index {failure_idx}: {:?}",
            results[failure_idx]
        );

        // Every op after the failing one is marked aborted, not attempted,
        // proving the process kept running instead of crashing.
        for r in &results[failure_idx + 1..] {
            assert_eq!(
                r["aborted"],
                json!(true),
                "expected abort after the depth guard trips: {r:?}"
            );
        }
    }

    // ── request-boundary regression: raw controls survive wire decoding ─────

    #[tokio::test]
    async fn request_boundary_raw_control_bytes_reach_handler() {
        // Simulates the actual MCP wire: a JSON-RPC client sends the tool's
        // `ops` argument as a JSON string using the standard JSON `\n`
        // escape. Deserializing `RequestParams` decodes that escape into an
        // actual raw LF byte inside the DSL source — the exact shape
        // `normalize_quoted_string` (crates/khive-request/src/parser/scan.rs)
        // exists to accept. This confirms the decoded raw newline survives
        // parsing and dispatch all the way to the pack handler's result.
        let wire = "{\"ops\":\"create(kind=\\\"entity\\\", entity_kind=\\\"concept\\\", name=\\\"line1\\nline2\\\")\"}";
        let params: RequestParams = serde_json::from_str(wire).expect("wire JSON deserializes");
        assert!(
            params.ops.contains('\n'),
            "deserialized ops must carry a raw LF, not the two-char escape: {:?}",
            params.ops
        );

        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

        let parsed = parse_request(&params.ops).expect("literal newline inside quotes must parse");
        let response = server
            .run_parsed(
                parsed.ops,
                parsed.mode,
                PresentationMode::Verbose,
                None,
                false,
                None,
            )
            .await;

        let results = response["results"]
            .as_array()
            .expect("results must be an array");
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0]["ok"],
            json!(true),
            "unexpected result: {response:?}"
        );
        assert_eq!(results[0]["result"]["name"], json!("line1\nline2"));
    }

    // ── MCP-AUD-002 regression: save_to must bypass daemon forwarding ────────

    fn make_daemon_save_to_test_server() -> KhiveMcpServer {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        KhiveMcpServer::new(runtime).expect("server builds with kg")
    }

    fn clear_daemon_env() {
        std::env::remove_var("KHIVE_SOCKET");
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::remove_var("KHIVE_LOCK");
    }

    /// khive#948: `wire_daemon_frame` forwards `RequestParams::request_id`
    /// onto the `DaemonRequestFrame` unchanged, and defaults to `None` when
    /// the caller supplied none.
    #[cfg(unix)]
    #[test]
    fn wire_daemon_frame_forwards_request_id() {
        let server = make_daemon_save_to_test_server();

        let with_id = RequestParams {
            ops: "stats()".to_string(),
            request_id: Some(123),
            ..Default::default()
        };
        let frame = server.wire_daemon_frame(&with_id);
        assert_eq!(frame.request_id, Some(123));

        let without_id = RequestParams {
            ops: "stats()".to_string(),
            ..Default::default()
        };
        let frame = server.wire_daemon_frame(&without_id);
        assert_eq!(frame.request_id, None);
    }

    /// Query every persisted audit event and find the one whose
    /// `resource.request_id` matches `id`, if any.
    async fn find_audit_event_with_request_id(
        store: &Arc<dyn khive_storage::EventStore>,
        id: u64,
    ) -> Option<khive_storage::Event> {
        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query_events must succeed");
        page.items
            .into_iter()
            .find(|ev| ev.payload["resource"]["request_id"] == json!(id))
    }

    /// khive#948: `request_id` was previously dropped on the
    /// `KHIVE_NO_DAEMON`/soft-fallback local dispatch path because
    /// `dispatch_request_wire` always passed `identity = None`. This drives
    /// `request()` end-to-end under `KHIVE_NO_DAEMON=1` and inspects the
    /// persisted audit event, proving the id now survives to
    /// `resource.request_id` on the local-dispatch path too, not just the
    /// daemon-forward path.
    #[tokio::test]
    #[serial]
    async fn request_no_daemon_fallback_preserves_request_id_in_audit_event() {
        clear_daemon_env();
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let server = make_daemon_save_to_test_server();
        server
            .request(Parameters(RequestParams {
                // Explicit `namespace="local"` so the write lands in the
                // same namespace the server's audit `EventStore` handle is
                // scoped to at construction (`Namespace::local()`), matching
                // `find_audit_event_with_request_id`'s read scope.
                ops: "stats(namespace=\"local\")".to_string(),
                request_id: Some(9001),
                ..Default::default()
            }))
            .await
            .expect("request() must succeed via local dispatch under KHIVE_NO_DAEMON");

        let store = server
            .event_store()
            .expect("in-memory runtime must configure an EventStore");
        let matched = find_audit_event_with_request_id(&store, 9001).await;
        assert!(
            matched.is_some(),
            "KHIVE_NO_DAEMON local dispatch must stamp request_id onto the persisted \
             audit event"
        );

        clear_daemon_env();
    }

    /// khive#948: the `save_to` bypass (MCP-AUD-002) also routes through
    /// `dispatch_request_wire`'s local dispatch — this proves the id
    /// survives that path too.
    #[tokio::test]
    #[serial]
    async fn request_save_to_bypass_preserves_request_id_in_audit_event() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_SAVE_TO_ROOT", dir.path());

        let server = make_daemon_save_to_test_server();
        let sink_path = dir.path().join("out.jsonl");
        server
            .request(Parameters(RequestParams {
                ops: "stats(namespace=\"local\")".to_string(),
                save_to: Some(sink_path.to_string_lossy().to_string()),
                request_id: Some(9002),
                ..Default::default()
            }))
            .await
            .expect("request() with save_to must succeed");

        let store = server
            .event_store()
            .expect("in-memory runtime must configure an EventStore");
        let matched = find_audit_event_with_request_id(&store, 9002).await;
        assert!(
            matched.is_some(),
            "save_to local-dispatch bypass must stamp request_id onto the persisted \
             audit event"
        );

        clear_daemon_env();
        std::env::remove_var("KHIVE_SAVE_TO_ROOT");
    }

    #[cfg(unix)]
    async fn connect_when_daemon_ready(sock: &std::path::Path) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if tokio::net::UnixStream::connect(sock).await.is_ok() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon never bound {sock:?} within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Regression for MCP-AUD-002 / #440: `request()` must NOT forward a
    /// `save_to`-bearing call to a warm daemon (whose wire frame has no
    /// `save_to` field and would silently return the inline result instead of
    /// writing the sink). With a real daemon reachable at `KHIVE_SOCKET`, a
    /// `save_to` request must still take the local path and return the
    /// manifest with the file actually written — proving the daemon was
    /// bypassed rather than silently dropping the sink.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn request_save_to_bypasses_daemon_forwarding_and_writes_manifest() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");
        // save_to destinations must resolve inside the allowed export root
        // (crate::save_sink); scope it to this test's tempdir.
        std::env::set_var("KHIVE_SAVE_TO_ROOT", dir.path());

        let server = make_daemon_save_to_test_server();
        let daemon_server = server.clone();
        let handle = tokio::spawn(async move {
            let _ = khive_runtime::daemon::run_daemon(daemon_server).await;
        });
        connect_when_daemon_ready(&sock).await;

        let sink_path = dir.path().join("out.jsonl");
        let resp = server
            .request(Parameters(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: Some(sink_path.to_string_lossy().to_string()),
                format: None,
                format_per_op: None,
                request_id: None,
            }))
            .await
            .expect("request with save_to must succeed even with a warm daemon reachable");

        let manifest: serde_json::Value =
            serde_json::from_str(&resp).expect("response must be the save_to manifest JSON");
        assert!(
            manifest.get("rows").is_some() && manifest.get("path").is_some(),
            "response must be the save_to manifest, not an inline daemon result; got: {resp}"
        );
        assert!(
            sink_path.exists(),
            "save_to file must be written even when a daemon is reachable"
        );
        let contents = std::fs::read_to_string(&sink_path).expect("read sink file");
        assert!(
            !contents.trim().is_empty(),
            "sink file must contain JSONL content"
        );

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
        std::env::remove_var("KHIVE_SAVE_TO_ROOT");
    }

    // ── #644 regression: ambiguous post-write outcome must not double-dispatch ──
    //
    // `request()`'s daemon-forward call site (`if let Some(res) = forward_or_spawn(...)
    // .await { return res; }`) must return BOTH `Some(Ok(_))` and `Some(Err(_))`
    // directly, never falling through to `dispatch_request_wire` on the `Err`
    // arm. If a future edit narrowed that match to only short-circuit on
    // success (e.g. `if let Some(Ok(res)) = ...`), a mutating op whose real
    // frame was already written to a now-dead daemon would ALSO run through
    // local dispatch — a duplicate execution of the exact case #644 exists to
    // prevent. This forces that ambiguous outcome (a fake socket that reads
    // the request then closes without responding, exactly as a daemon crash
    // mid-dispatch would) and proves both that the caller sees the
    // ambiguous-forward error verbatim AND that the mutating op never actually
    // ran locally.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn request_returns_ambiguous_forward_error_without_local_double_dispatch() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "comm".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg + comm");

        // Fake "crashed daemon": accept exactly one connection, read the
        // request frame (the real write #644 cares about), then drop the
        // stream without writing a response.
        let listener =
            tokio::net::UnixListener::bind(&sock).expect("bind fake crash-daemon socket");
        let fake_handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = khive_runtime::daemon::read_frame(&mut stream).await;
            }
        });

        let baseline = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("baseline stats() must succeed");

        let resp = server
            .request(Parameters(RequestParams {
                ops: "comm.send(to=\"bob\", content=\"double-forward-probe\")".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            }))
            .await;

        match resp {
            Err(McpError { message, .. }) => {
                assert!(
                    message.contains(
                        "not retrying or locally dispatching to avoid duplicate execution"
                    ),
                    "request() must surface forward_or_spawn's ambiguous-forward error \
                     verbatim, not a local dispatch result; got: {message}"
                );
            }
            Ok(v) => panic!(
                "request() must return the ambiguous-forward error directly, not fall \
                 through to local dispatch; got Ok({v})"
            ),
        }

        let after = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("post-request stats() must succeed");
        assert_eq!(
            after, baseline,
            "the comm.send op must NEVER have run locally after the ambiguous \
             forward outcome — a double-dispatch would mutate local state here"
        );

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;
        clear_daemon_env();
    }

    // ── #947 Medium regression: strict fallback lands as a per-op envelope ──
    //
    // Before this fix, `request()` returned `forward_or_spawn`'s strict-mode
    // rejection as a raw `Err(McpError)`, bypassing the per-op `{ok, tool,
    // result/error}` / `summary` wire contract every other failure mode goes
    // through. This drives `request()` end to end with a genuinely
    // unreachable daemon under `KHIVE_DAEMON_STRICT=1` and asserts: (1) the
    // response is `Ok(envelope_json)`, never an RPC error; (2) each shape
    // (single op, parallel batch, chain) reports the fallback reason as a
    // normal failed-op `error`, with chain aborting the remaining ops exactly
    // like a real op failure would (`run_parsed`'s `Chain` arm); (3) summary
    // counts match `results`; and (4) none of the ops ever ran locally (a
    // `stats()` snapshot taken via the trusted `dispatch_request_local` path
    // is unchanged after all three calls).
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn request_strict_fallback_lands_as_failed_op_envelope_not_rpc_error() {
        clear_daemon_env();
        crate::daemon::reset_fallback_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        // Never bound by anything in this test. The spawned test harness exits
        // immediately on `mcp --daemon`, so #898 classifies this as a confirmed
        // respawn failure rather than the older generic `no_socket` fallback.
        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::set_var("KHIVE_DAEMON_STRICT", "1");

        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "comm".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg + comm");

        let baseline = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("baseline stats() must succeed");

        fn assert_fallback_error(entry: &Value, tool: &str) {
            assert_eq!(entry["ok"], json!(false), "entry: {entry}");
            assert_eq!(entry["tool"], json!(tool), "entry: {entry}");
            let msg = entry["error"].as_str().expect("error must be a string");
            assert!(
                msg.contains("KHIVE_DAEMON_STRICT"),
                "error must name the strict mode that rejected the fallback: {msg}"
            );
            assert!(
                msg.contains("respawn_failed"),
                "error must name the confirmed respawn failure: {msg}"
            );
            assert!(
                msg.contains("make local"),
                "error must include the safe respawn remediation: {msg}"
            );
        }

        // ── single op ──────────────────────────────────────────────────────
        let single_resp = server
            .request(Parameters(RequestParams {
                ops: "comm.send(to=\"bob\", content=\"strict-single-probe\")".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            }))
            .await
            .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
        let single: Value =
            serde_json::from_str(&single_resp).expect("response must be the request envelope");
        assert_eq!(
            single["results"].as_array().expect("results array").len(),
            1
        );
        assert_fallback_error(&single["results"][0], "comm.send");
        assert_eq!(
            single["summary"],
            json!({ "total": 1, "succeeded": 0, "failed": 1, "aborted": 0 })
        );

        // ── parallel batch ─────────────────────────────────────────────────
        let batch_resp = server
            .request(Parameters(RequestParams {
                ops: "[comm.send(to=\"bob\", content=\"strict-batch-1\"), \
                       comm.send(to=\"bob\", content=\"strict-batch-2\")]"
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            }))
            .await
            .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
        let batch: Value =
            serde_json::from_str(&batch_resp).expect("response must be the request envelope");
        let batch_results = batch["results"].as_array().expect("results array");
        assert_eq!(batch_results.len(), 2);
        for entry in batch_results {
            assert_fallback_error(entry, "comm.send");
        }
        assert_eq!(
            batch["summary"],
            json!({ "total": 2, "succeeded": 0, "failed": 2, "aborted": 0 })
        );

        // ── chain (must abort remaining ops per the wire contract) ─────────
        let chain_resp = server
            .request(Parameters(RequestParams {
                ops: "comm.send(to=\"bob\", content=\"strict-chain-1\") | \
                      comm.send(to=\"bob\", content=\"strict-chain-2\")"
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            }))
            .await
            .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
        let chain: Value =
            serde_json::from_str(&chain_resp).expect("response must be the request envelope");
        let chain_results = chain["results"].as_array().expect("results array");
        assert_eq!(chain_results.len(), 2);
        assert_fallback_error(&chain_results[0], "comm.send");
        assert_eq!(
            chain_results[1],
            json!({ "ok": false, "tool": "comm.send", "aborted": true })
        );
        assert_eq!(
            chain["summary"],
            json!({ "total": 2, "succeeded": 0, "failed": 1, "aborted": 1 })
        );

        // ── no local dispatch ever happened for any of the three calls ─────
        let after = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("post-request stats() must succeed");
        assert_eq!(
            after, baseline,
            "no comm.send op must ever have run locally under strict-mode fallback \
             rejection — a local dispatch would mutate local state here"
        );

        crate::daemon::reset_fallback_counters();
        clear_daemon_env();
    }
}
