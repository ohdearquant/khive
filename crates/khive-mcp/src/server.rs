//! KhiveMcpServer — rmcp-based MCP server exposing a single `request` tool.
//!
//! Accepts the function-call DSL or JSON form and dispatches each parsed operation
//! through the [`VerbRegistry`] built from the configured packs.
//!
// Request execution, bridge forwarding, dispatch helpers and rendering live in
// private concern modules below; constructors and the MCP tool surface stay here.

use crate::coordinator::CoordinatorService;
use crate::tools::request::RequestParams;
use khive_db::ConnectionPool;
#[cfg(test)]
use khive_runtime::daemon::DAEMON_LEXICAL_TIMEOUT_MARKER;
#[cfg(test)]
use khive_runtime::presentation::NoteContentScope;
use khive_runtime::{
    DomainDisposition, KhiveRuntime, OutputFormat, PackLoadError, PackRegistry, RuntimeConfig,
    RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
#[cfg(test)]
use khive_runtime::{PresentationMode, VerbPresentationPolicy};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

const _: () = assert!(
    khive_runtime::daemon::ERROR_DETAIL_NESTING_DEPTH_LIMIT == khive_request::NESTING_DEPTH_LIMIT
);

static BRIDGE_INSTANCE_ID: std::sync::OnceLock<uuid::Uuid> = std::sync::OnceLock::new();

pub(crate) fn bridge_instance_id() -> uuid::Uuid {
    *BRIDGE_INSTANCE_ID.get_or_init(uuid::Uuid::new_v4)
}

mod disk_policy;
mod pack_setup;
mod search_diagnostics;

use disk_policy::disk_guard_policy_fingerprint;

use search_diagnostics::OpSuccess;
#[cfg(test)]
use search_diagnostics::{
    backend_errors_value, search_arm_participation_value, SearchDegradation, SearchStatus,
};
#[cfg(test)]
use search_diagnostics::{
    bounded_backend_error_key, bounded_backend_error_message, search_diagnostic_value,
    search_diagnostic_wire_len, search_retry_after_ms, BackendErrorDiagnostic, SearchArmEvidence,
    SearchArmParticipation, SearchArmStatus, MAX_BACKEND_ERROR_ENTRIES,
    MAX_BACKEND_ERROR_KEY_CHARS, MAX_BACKEND_ERROR_MESSAGE_CHARS,
    MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP, MISSING_BACKEND_ERROR_MESSAGE,
};

/// Per-request parallelism stays bounded even when the parser accepts 100 ops; must be nonzero.
const MAX_BATCH_CONCURRENCY: usize = 8;

/// Half the frame remains for the daemon's outer serialization and budget-error entries.
const BATCH_RESPONSE_BUDGET_BYTES: usize = khive_runtime::daemon::MAX_FRAME_BYTES / 2;

mod batch_types;
pub(crate) use batch_types::DispatchOrigin;
#[cfg(any(unix, test))]
use batch_types::{aborted_entry, failure_entry};
use batch_types::{error_with_disposition, DispatchFailure};
#[cfg(test)]
use batch_types::{BatchTask, RunParsedContext};

mod config_id;
#[cfg(test)]
use config_id::build_verb_catalog;
#[cfg(test)]
pub(crate) use config_id::compute_config_id_with_ann_fresh_tail;
pub(crate) use config_id::compute_config_id_with_runtime_policies;
#[cfg(test)]
use config_id::{canonical_fingerprint_path, encode_backend_topology, format_served_kinds_suffix};
pub use config_id::{compute_config_id, compute_config_id_with_storage_mode};

mod server_core;
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(crate) use server_core::ChannelLoopAdmission;
pub use server_core::{builtin_pack_names, KhiveMcpServer, PackRegError, PackRegFailure};
#[cfg(test)]
use server_core::{
    stdio_bridge_idle_timeout_from_env, stdio_bridge_max_outstanding_requests_from_env,
    stdio_bridge_request_obligation_ttl_from_env, stdio_bridge_response_deadline_from_env,
};
#[cfg(all(unix, test))]
use server_core::{stdio_serve_mode_for, StdioServeMode};

mod batch_execution;
mod bridge;
mod dispatch_helpers;
mod execution;
mod rendering;
mod request_dispatch;

use batch_execution::{
    attach_strict_refusal_reasons, batch_budget_error, batch_status, execute_bounded_batch,
    execute_bounded_units, parallel_batch_envelope,
};
#[cfg(test)]
use bridge::{ensure_bridge_request_id, scope_mcp_request_read_cancellation};
use bridge::{
    parsed_contains_bridge_diagnostics, request_read_timeout,
    scope_mcp_request_read_cancellation_with_timeout,
};
#[cfg(all(unix, test))]
use bridge::{strict_fallback_envelope_response, ForwardFuture};
use dispatch_helpers::{
    apply_presentation_to_result, chain_aggregation_depth_reject, chain_ok_envelope_or_depth_error,
    decorate_schedule_agenda_with_ticker_health, dispatch_via_coordinator_inner,
    drop_value_iteratively, note_content_scope, parse_content_requested,
    present_ok_envelope_or_depth_error, result_within_depth_limit, stamp_usage,
    substitution_error_payload,
};
#[cfg(test)]
use dispatch_helpers::{
    coordinator_search_visibility, ok_envelope, result_exceeds_depth_limit, runtime_error_value,
};
#[cfg(unix)]
use rendering::cancelled_forward_error;
use rendering::{
    attach_audit_persistence_advisories, build_instructions, dsl_err_to_mcp, invalid_request_error,
    mark_all_failed_request_result, mark_daemon_lexical_timeout, parse_output_format,
    parse_presentation_mode, render_result, request_internal_error, save_to_write_error,
    serialize_response_value, validate_request_overrides, RenderContext,
};
#[cfg(test)]
use rendering::{
    empty_rendered_daemon_frame_len, entry_escaped_len, envelope_escaped_len, envelope_metadata,
    envelope_metadata_escaped_len, fit_rendered_batch_envelope, frame_budget_omission,
    rendered_response_daemon_frame_len, rendered_response_fits_daemon_frame,
    serialized_response_len,
};

impl KhiveMcpServer {
    /// Build a server from `runtime.config().packs`. Errors if any pack is unknown or missing deps.
    ///
    /// This constructor assumes the supplied runtime's database is already at a
    /// complete V21 attachment cutover. It intentionally performs no migration
    /// or blob-evidence verification. Production hosts opening a database should
    /// use the async builders in [`crate::serve`] and reserve this constructor for
    /// already-prepared runtimes and tests.
    // The error variant intentionally carries the runtime so callers can recover.
    #[allow(clippy::result_large_err)]
    pub fn new(runtime: KhiveRuntime) -> Result<Self, PackRegError> {
        let packs: Vec<String> = runtime.config().packs.clone();
        // Fail-fast on bad packs so callers can decide recovery.
        // Schema plan application happens inside with_packs.
        Self::with_packs(runtime, &packs)
    }

    /// Build a server with an explicit pack list (strict — fails on unknown names).
    ///
    /// The same already-prepared-runtime precondition as [`Self::new`] applies.
    // The error variant intentionally carries the runtime by value so callers
    // can recover and retry. Boxing would force every recovery path through a
    // deref for no real benefit.
    #[allow(clippy::result_large_err)]
    pub fn with_packs(runtime: KhiveRuntime, packs: &[String]) -> Result<Self, PackRegError> {
        if !runtime.config().mounts.is_empty() {
            return Err(PackRegError {
                failure: PackRegFailure::Registry(RuntimeError::InvalidInput(
                    "configured mounts require the async server constructor".into(),
                )),
                runtime,
            });
        }
        Self::with_mounted_packs(runtime, packs, Vec::new(), None)
    }

    /// Build a prepared runtime's native registry and start its configured sources.
    #[allow(clippy::result_large_err)]
    pub async fn new_with_mounts(runtime: KhiveRuntime) -> Result<Self, PackRegError> {
        Self::new_with_mounts_and_config(runtime, None).await
    }

    /// Build a prepared runtime's registry with resolved operator configuration.
    /// Applies disabled-handler policy and includes it in daemon identity. As with
    /// [`Self::new_with_mounts`], storage preparation remains the caller's job.
    #[allow(clippy::result_large_err)]
    pub async fn new_with_mounts_and_config(
        runtime: KhiveRuntime,
        config: Option<&khive_runtime::KhiveConfig>,
    ) -> Result<Self, PackRegError> {
        let packs = runtime.config().packs.clone();
        let mounted = khive_mounts::start_mounts(&runtime).await;
        Self::with_mounted_packs(runtime, &packs, mounted, config)
    }

    /// Build a server directly from a pre-configured registry.
    ///
    /// Intended for tests that need to inject mock packs (e.g. packs that
    /// return `RuntimeError::Khive` to exercise structured error serialization).
    /// Production code should use [`Self::new`] or [`Self::with_packs`].
    #[doc(hidden)]
    pub fn from_registry(registry: VerbRegistry) -> Self {
        // A registry injected directly has no resolved RuntimeConfig; use a
        // sentinel that matches no real daemon so such servers always
        // dispatch locally rather than forward.
        Self::from_registry_with_meta(registry, "local", "registry-only")
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
            schedule_ticker_last_tick_micros: Arc::new(AtomicI64::new(0)),
            #[cfg(unix)]
            events_readiness: Arc::new(std::sync::OnceLock::new()),
            #[cfg(unix)]
            bridge_executable: None,
            stdio_bridge: false,
            runtime: None,
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_outbox_runtime: None,
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_loop_admission: ChannelLoopAdmission::default(),
        }
    }

    /// Attach the default-backend `KhiveRuntime` (see the `runtime` field docs
    /// on [`KhiveMcpServer`]). Used by the multi-backend boot path to wire in
    /// the same `default_runtime` it already resolved while building the
    /// registry.
    pub fn with_runtime(mut self, runtime: KhiveRuntime) -> Self {
        #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
        if self.channel_outbox_runtime.is_none() {
            self.channel_outbox_runtime = Some(runtime.clone());
        }
        self.runtime = Some(runtime);
        self
    }

    /// Attach the exact comm-routed runtime that owns outbox note properties.
    /// Multi-backend boot overrides the default-runtime fallback installed by
    /// [`Self::with_runtime`]; single-backend construction already points both
    /// handles at the same runtime.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn with_channel_outbox_runtime(mut self, runtime: Option<KhiveRuntime>) -> Self {
        self.channel_outbox_runtime = runtime;
        self
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

    /// Attach the runtime-derived channel admission computed by the
    /// multi-backend builder after pack routing has been resolved.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn with_channel_loop_admission(mut self, admission: ChannelLoopAdmission) -> Self {
        self.channel_loop_admission = admission;
        self
    }

    /// Return the fixed boot-time admission for channel background tasks.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn channel_loop_admission(&self) -> ChannelLoopAdmission {
        self.channel_loop_admission
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

    pub(crate) fn blob_upload_manager(
        &self,
    ) -> Option<Arc<khive_pack_blob::uploads::UploadManager>> {
        self.registry.pack_host_state("blob")
    }

    /// Clone the verb registry for use by background tasks (e.g. channel polling loops).
    ///
    /// `VerbRegistry` is internally `Arc`-wrapped so this clone is cheap. The returned
    /// registry shares the same packs and dispatch state as the server.
    #[cfg(any(test, feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn verb_registry_clone(&self) -> VerbRegistry {
        self.registry.clone()
    }

    /// Clone the KG-routed `KhiveRuntime` retained for background tasks that
    /// need a non-wire owner API (the email outbox loop's `external_id`
    /// claim). `KhiveRuntime` is internally `Arc`-wrapped so this is cheap.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn channel_outbox_runtime_clone(&self) -> Option<KhiveRuntime> {
        self.channel_outbox_runtime.clone()
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
    /// `Ok(OpSuccess)` → success payload plus any coordinator degradation
    /// metadata (caller wraps it in the per-op envelope).
    /// `Err(DispatchFailure)` → error payload plus any stable refusal reason.
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
    ) -> Option<Result<OpSuccess, DispatchFailure>> {
        let coord = self.coordinator.as_ref()?;
        if coord.is_single_backend() {
            return None;
        }
        dispatch_via_coordinator_inner(coord.as_ref(), &self.registry, tool, args_value, identity)
            .await
    }

    /// Namespace this server's registry was built for.
    pub fn default_namespace(&self) -> &str {
        &self.default_namespace
    }

    /// Fingerprint of the runtime config this server's registry was built for.
    /// The resolved events-split config of the default-backend runtime, if
    /// any (ADR-170). Daemon supervision derives the events daemon's
    /// db/socket from this exact value so the supervised daemon and the
    /// forwarding clients cannot anchor at diverging paths.
    pub(crate) fn events_split_config(
        &self,
    ) -> Option<&khive_runtime::events_split::EventsSplitConfig> {
        self.runtime
            .as_ref()
            .and_then(|rt| rt.config().events_split.as_ref())
    }

    /// Events storage inherits the resolved main-backend policy, including its
    /// source. The server's checkpoint pool may be overridden independently.
    pub(crate) fn events_wal_ceiling_policy(&self) -> Option<khive_db::WalCeilingPolicy> {
        self.runtime
            .as_ref()
            .map(|rt| rt.core().backend().pool().config().wal_ceiling)
    }

    /// Whether the default-backend runtime is read-only. Daemon supervision
    /// asks this before spawning an events daemon: the supervised daemon
    /// opens the events sidecar writable, which a read-only deployment must
    /// never cause (ADR-170).
    pub(crate) fn default_runtime_is_read_only(&self) -> bool {
        self.runtime.as_ref().is_some_and(|rt| rt.is_read_only())
    }

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

    /// Quiesce the ADR-133 audit-batch supervisor before a short-lived
    /// process drops this server.
    ///
    /// Stops admission and waits for every already-accepted audit row to
    /// reach a terminal state, then joins the supervisor's own task so its
    /// separate clone of the runtime's connection pool is released before
    /// this call returns (a no-op returning `Ok(())` when no audit
    /// `EventStore` is configured, see [`VerbRegistry::shutdown_audit_batch`]).
    ///
    /// Exists so a caller that owns this server for the lifetime of one
    /// process invocation, not a long-running daemon, can make the pool's
    /// own `Drop` run deterministically on return instead of racing a
    /// detached supervisor task that may still hold a pool reference. Every
    /// serving path (the daemon, `serve_stdio`) keeps the registry and its
    /// audit batch alive for the process lifetime and must not call this.
    pub async fn shutdown_audit_batch(
        &self,
    ) -> Result<(), khive_runtime::audit_batch::AuditTerminalReason> {
        self.registry.shutdown_audit_batch().await
    }

    /// The server-level default output format (ADR-078), as resolved at
    /// construction by [`crate::serve::apply_env_output_format`].
    pub fn default_output_format(&self) -> OutputFormat {
        self.default_output_format
    }

    /// Record that this process's daemon schedule loop began a tick.
    ///
    /// The loop calls this before starting its drain pass, including passes
    /// that find no due rows or return an error. A pass that wedges after this
    /// point leaves a frozen timestamp for callers to classify as stale.
    pub(crate) fn record_schedule_ticker_tick(&self) {
        self.schedule_ticker_last_tick_micros
            .store(chrono::Utc::now().timestamp_micros(), Ordering::Release);
    }

    /// Install the startup gate for the events daemon this server's daemon
    /// supervises. Returns `false`, leaving the first gate in place, when one
    /// is already installed.
    #[cfg(unix)]
    pub(crate) fn install_events_readiness(
        &self,
        readiness: khive_runtime::events_split::EventsReadiness,
    ) -> bool {
        self.events_readiness.set(readiness).is_ok()
    }

    /// The installed events-daemon startup gate, shared by every clone.
    #[cfg(unix)]
    pub(crate) fn events_readiness_gate(
        &self,
    ) -> Option<khive_runtime::events_split::EventsReadiness> {
        self.events_readiness.get().cloned()
    }

    /// Warm every pack's in-memory state. Called by the daemon in a background
    /// task after the socket is bound.
    pub async fn warm_all(&self) {
        self.registry.call_warm_all().await;
    }
}

#[tool_router]
impl KhiveMcpServer {
    #[tool(description = r#"Run one or more khive verbs in a single MCP call.

Set plan=true with ops alone to check syntax without execution. The result has
parsed, mode, stage_count, stages (verb, pack, known, args, prev_refs), and parser
limits. A syntax error returns parsed=false and error, with no stages. Planning
does not check permission or resolve references. presentation, presentation_per_op,
format, format_per_op, save_to, and request_id cannot accompany plan=true.

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
    "summary": { "total": N, "succeeded": N, "failed": N, "aborted": N },
    "status": "success" | "partial"
  }

Parallel: a failed op does NOT abort siblings. Chain: failure aborts remaining
ops (reported as {"ok": false, "aborted": true}). Committed ops are not rolled back.
`status` is "partial" whenever summary.failed or summary.aborted is non-zero.
The MCP tool result sets isError=true only when no op succeeded and at least
one failed or aborted. For mixed batches inspect each result and the summary.

A parallel write-heavy batch is best-effort, not atomic: `results` ordering is
not a commit prefix (an earlier entry succeeding implies nothing about a later
one, or vice versa), and one entry's safe-retry failure (e.g. `retryable:
true`, `code: "writer_pool_checkout_timeout"`, `"writer_queue_saturated"`,
or `"writer_task_begin_busy"`)
never rolls back a sibling that already committed. Inspect each result
entry's own `ok` field rather than assuming batch-level atomicity.

`comm.read` and `comm.mark_read` mutate delivery state. In a parallel batch,
either acknowledgement does not wait for or depend on comm.send/comm.reply, so
a read mark can commit even when the sibling send fails. When the mark must
depend on a send, use a chain so a failed send aborts the mark. For the common
reply-and-read flow, prefer `comm.reply`: comm.reply delivers first, then attempts
the original message's best-effort read mark.

`search` carries its own per-op `status` ("complete" | "partial") inside that
op's `result` entry, separate from the top-level batch `status` above. A
degraded-but-answered search stays ok:true with status="partial" plus a
missing_backends list, bounded backend_errors causes, and the deprecated
partial:true alias. Truncation is explicit through backend_errors_truncated and
backend_errors_omitted. When a backend failure leaves no hit standing after
filtering, the op instead fails outright with ok:false and
error.kind="search_incomplete" while retaining the same diagnostics — that case
must not be read as "no results found." Per-backend causes use kind="timeout"
for typed deadline failures and kind="backend_error" otherwise. The incomplete
error is retryable only when every failed backend leg timed out. Every such
error names retry_after_ms: 2000ms plus 250ms per additional failed backend,
capped at 10000ms and computed from the full pre-truncation failure set.
Conforming clients use at most three total attempts per logical request,
exponential backoff with nonnegative jitter (never shorter than the named
pace), and a 30s circuit breaker after three consecutive all-timeout outcomes
for the same backend set; the breaker suppresses first attempts too and admits
one half-open probe after the open interval.

Verb discovery: install the `kg` / `gtd` plugins for usage skills. The verbs
currently registered on this server (pack-derived) are listed below. Argument
schemas live in each pack's docs and SKILL.md files.

Bridge control: `bridge.diagnostics()` reads this stdio bridge's in-memory
fallback counters without contacting a daemon or store. It must be the only
operation in the request; `bridge.diagnostics(help=true)` describes its schema.

Tip: for one-shot calls, the single-op form is the densest. Use batch when
several independent ops can run together; use chain when each op needs the prior
result (e.g. create then link with the new entity's id)."#)]
    async fn request(
        &self,
        Parameters(p): Parameters<RequestParams>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<String, McpError> {
        let timeout = crate::request_policy::read_timeout(&p.ops, request_read_timeout());
        scope_mcp_request_read_cancellation_with_timeout(
            cancellation,
            timeout,
            self.request_with_cancellation(p),
        )
        .await
    }
}

#[cfg(test)]
#[path = "server/request_result_error_tests.rs"]
mod request_result_error_tests;

#[tool_handler]
impl ServerHandler for KhiveMcpServer {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let is_request_tool = request.name == "request";
        // The router turns parameter-deserialization failures into tool errors.
        // Plan isolation requires the JSON-RPC invalid_params response instead.
        if request.name == "request" {
            if let Some(args) = request.arguments.as_ref() {
                if args.get("plan") == Some(&Value::Bool(true)) {
                    for field in crate::tools::request::PLAN_COMPANIONS {
                        if args.contains_key(field) {
                            return Err(invalid_request_error(format!(
                                "plan=true cannot be combined with {field}"
                            )));
                        }
                    }
                }
            }
        }
        let context = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let mut result = Self::tool_router().call(context).await?;
        if is_request_tool {
            mark_all_failed_request_result(&mut result);
        }
        Ok(result)
    }

    fn get_info(&self) -> ServerInfo {
        let loaded_names = self.registry.pack_names();
        let loaded = loaded_names.join(", ");
        // Linked minus loaded: the packs `--pack`/`KHIVE_PACKS` could still
        // select. `builtin_pack_names` is the link-time inventory, so it never
        // lists a configured mount; a mount is only ever in `loaded_names`.
        let unloaded = builtin_pack_names()
            .into_iter()
            .filter(|name| !loaded_names.contains(name))
            .collect::<Vec<_>>()
            .join(", ");
        let instructions = build_instructions(&loaded, &unloaded);
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
        self.registry.mounted_verb_catalog().await.map_err(|_| {
            McpError::internal_error(
                "mounted catalog unavailable",
                Some(error_with_disposition(
                    json!({
                        "kind": "unavailable",
                        "message": "mounted catalog unavailable",
                        "details": {"class": "tool_error", "reason": "catalog_drift"},
                    }),
                    DomainDisposition::NotCommitted,
                )),
            )
        })?;
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
#[path = "server/events_wal_policy_tests.rs"]
mod events_wal_policy_tests;

#[cfg(test)]
mod request_read_cancellation_tests;
#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

#[cfg(test)]
mod bridge_diagnostics_tests;

#[cfg(test)]
mod disposition_tests;

#[cfg(test)]
mod refusal_event_tests;

#[cfg(test)]
mod issue_2537_tests;

#[cfg(test)]
#[path = "server_operation_attribution_tests.rs"]
mod operation_attribution_tests;

#[cfg(test)]
#[path = "event_row_usage_tests.rs"]
mod event_row_usage_tests;

#[cfg(test)]
#[path = "server/numeric_env_tests.rs"]
mod numeric_env_tests;

#[cfg(test)]
#[path = "audit_success_rows_usage_tests.rs"]
mod audit_success_rows_usage_tests;
