//! MCP server state, registration errors, and stdio settings.

use std::sync::{atomic::AtomicI64, Arc};

use khive_db::ConnectionPool;
use khive_runtime::{KhiveRuntime, OutputFormat, PackRegistry, VerbRegistry};

use crate::coordinator::CoordinatorService;

/// Runtime-mode admission for transport background work.
///
/// The inbound tasks dispatch only `comm.*` verbs (`comm.ingest`, heartbeat,
/// and cursor operations), and the outbound tasks scan, claim, and mark
/// outbound `message` notes through the runtime's non-wire owner-side APIs —
/// both against the comm pack's actual assigned runtime, since under a
/// `[packs.comm]` backend assignment that is the backend holding comm's
/// rows. Neither loop may run unless that runtime can durably record its
/// writes. The two decisions stay separate so a future topology can admit
/// one direction without the other.
///
/// Inbound polling also publishes each quarantined message's original bytes
/// through `blob.put`, so it needs the blob pack's runtime to accept writes as
/// well. A blob runtime that cannot write would fail that publish on every
/// poll, hold the cursor, and retry the same message forever; admission
/// refuses it up front instead, and `inbound_blocked_by_read_only_blob`
/// records that this was the reason so the refusal can be logged by name.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelLoopAdmission {
    pub(crate) inbound_poll: bool,
    pub(crate) outbound_delivery: bool,
    pub(crate) inbound_blocked_by_read_only_blob: bool,
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
impl ChannelLoopAdmission {
    pub(super) fn for_single_runtime(runtime: &KhiveRuntime, packs: &[String]) -> Self {
        let comm_loaded = packs.iter().any(|pack| pack == "comm");
        let writable = !runtime.is_read_only();
        Self {
            inbound_poll: comm_loaded && writable,
            outbound_delivery: comm_loaded && writable,
            inbound_blocked_by_read_only_blob: false,
        }
    }

    /// `blob` is `None` when the blob pack is not loaded; the poll task's own
    /// storage readiness check reports that case.
    pub(crate) fn for_pack_runtimes(
        comm: Option<&KhiveRuntime>,
        blob: Option<&KhiveRuntime>,
    ) -> Self {
        let admitted = comm.is_some_and(|runtime| !runtime.is_read_only());
        let blob_read_only = blob.is_some_and(KhiveRuntime::is_read_only);
        Self {
            inbound_poll: admitted && !blob_read_only,
            outbound_delivery: admitted,
            inbound_blocked_by_read_only_blob: admitted && blob_read_only,
        }
    }
}

/// MCP server that dispatches all verbs through a [`VerbRegistry`].
#[derive(Clone)]
pub struct KhiveMcpServer {
    pub(super) registry: VerbRegistry,
    #[cfg(unix)]
    pub(super) bridge_executable:
        Option<Arc<std::sync::Mutex<crate::daemon::executable::BridgeExecutable>>>,
    pub(super) stdio_bridge: bool,
    /// Namespace this registry was built for. The stdio client passes it to the
    /// daemon; a namespace mismatch triggers local-dispatch fallback.
    pub(super) default_namespace: String,
    /// Fingerprint of the resolved runtime config (packs, db target, embedders).
    /// The stdio client passes it to the daemon; a config mismatch triggers
    /// local-dispatch fallback so a restricted client never runs through the
    /// broader default daemon.
    pub(super) config_id: String,
    /// Cross-backend coordinator (ADR-029 Phase 2). Present only in multi-backend
    /// deployments. `None` in single-backend mode — all dispatch goes through the
    /// `VerbRegistry` unchanged (zero-change invariant).
    pub(super) coordinator: Option<Arc<dyn CoordinatorService>>,
    /// The default-backend `KhiveRuntime` this server was built from, retained
    /// for non-wire background APIs that are genuinely default-backend scoped.
    /// Pack-routed owner operations must use their dedicated runtime handle
    /// below instead of assuming this one owns the row. `None` only for servers built via
    /// [`Self::from_registry`]/[`Self::from_registry_with_meta`] without an
    /// explicit [`Self::with_runtime`] call (test-only construction paths).
    pub(super) runtime: Option<KhiveRuntime>,
    /// Runtime that owns the outbound `message` notes the delivery loops
    /// scan, claim, and mark — the comm pack's assigned runtime. Every one of
    /// those touches is deliberately non-wire (the generic verbs run on the
    /// kg/main runtime, which under a `[packs.comm]` backend assignment does
    /// not hold comm's rows). In a multi-backend topology this may differ
    /// from `runtime` (the default backend); retaining the exact comm
    /// runtime keeps scan, owner claim, and delivered-at update on one store.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(super) channel_outbox_runtime: Option<KhiveRuntime>,
    /// Pool arc for the WAL checkpoint background task. `None` for in-memory
    /// or registry-only servers that have no persistent database.
    pub(super) pool: Option<Arc<ConnectionPool>>,
    /// File-backed backend pools beyond `pool` (ADR-091 Amendment 3
    /// fan-out): every additional backend a multi-backend boot wired, so the
    /// session sweep and the daemon's checkpoint ownership can cover them
    /// too. Always empty for a single-backend server — `pool` alone is that
    /// server's one backend.
    pub(super) secondary_pools: Vec<Arc<ConnectionPool>>,
    /// Server-level default output format (ADR-078). Resolved from TOML →
    /// `KHIVE_OUTPUT_FORMAT` → builtin `json`. Per-request `format` fields
    /// override this at dispatch time.
    pub(super) default_output_format: OutputFormat,
    /// Last instant at which this process's daemon schedule loop began a tick.
    /// Zero means this server instance has never observed the loop running.
    /// Shared by server clones but never persisted, so a replacement process
    /// cannot inherit a plausible-looking heartbeat from its predecessor.
    pub(super) schedule_ticker_last_tick_micros: Arc<AtomicI64>,
    /// Per-verb-runtime write admission for email and Telegram background
    /// tasks. CLI daemon role is necessary but not sufficient: snapshot
    /// runtimes must never poll into a failing ingest path or send externally
    /// when delivery state cannot be durably marked.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(super) channel_loop_admission: ChannelLoopAdmission,
}

/// Failure reason inside a [`PackRegError`].
pub enum PackRegFailure {
    UnknownPack(String),
    DuplicatePack(String),
    MissingDependency { pack: String, dep: String },
    NoPublicVerbs { pack: String },
    Registry(khive_runtime::RuntimeError),
    Schema(khive_runtime::PackSchemaCollisionError),
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
            PackRegFailure::DuplicatePack(pack) => dbg.field("duplicate_pack", pack),
            PackRegFailure::MissingDependency { pack, dep } => {
                dbg.field("pack", pack).field("missing_dep", dep)
            }
            PackRegFailure::NoPublicVerbs { pack } => dbg.field("pack", pack),
            PackRegFailure::Registry(source) => dbg.field("source", source),
            PackRegFailure::Schema(source) => dbg.field("schema", source),
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
            PackRegFailure::DuplicatePack(pack) => write!(f, "duplicate pack {pack:?}"),
            PackRegFailure::MissingDependency { pack, dep } => write!(
                f,
                "pack {pack:?} requires {dep:?}, which is not in the requested pack list; \
                 add --pack {dep} before --pack {pack}"
            ),
            PackRegFailure::NoPublicVerbs { pack } => write!(
                f,
                "declared pack {pack:?} registers no public verbs and is not marked as \
                 intentionally vocabulary- or ontology-only"
            ),
            PackRegFailure::Registry(source) => write!(f, "pack registry build failed: {source}"),
            PackRegFailure::Schema(source) => write!(f, "{source}"),
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
/// the plain handshake path (see its `#[cfg(not(unix))]` variant in `server.rs`).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StdioServeMode {
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
pub(super) fn stdio_serve_mode_for(resumed_generation: Option<u32>) -> StdioServeMode {
    match resumed_generation {
        Some(_) => StdioServeMode::Resumed,
        None => StdioServeMode::Handshake,
    }
}

/// Optional idle timeout for a stdio bridge session: when configured, no
/// request for this long and the session closes (see
/// [`crate::transport::CancelOnEofTransport`]), releasing its reader-pool
/// admission and DB connection — a client that comes back simply respawns the
/// bridge, seamlessly from its perspective.
/// This closes only a genuinely idle session: an admitted request with a
/// response still being written — running long, or delivered slowly to a
/// backpressured reader — defers the close rather than being cancelled out
/// from under it, up to the separate response-delivery bound documented on
/// [`stdio_bridge_response_deadline_from_env`].
///
/// **Off unless configured, and that is a deliberate reading of existing
/// repository law rather than caution.** ADR-091 enumerates "kill long-lived
/// reader sessions" among its rejected alternatives, on the ground that
/// long-lived stdio sessions are live Claude Code instances and closing them
/// by age is a worse user experience than bounding what they hold underneath
/// them. Closing a session after an hour of quiet is that rejected policy
/// whatever the mechanism, because this transport has no signal that
/// separates an abandoned pipe from a live client that simply has not been
/// asked anything. Defaulting it on would reverse an accepted decision from
/// inside an unrelated change, so the default is off and turning it on is an
/// operator's explicit act — a supervised deployment, a CI harness, or any
/// context where session churn is cheap and a pinned WAL connection is not.
/// Making it the default requires amending ADR-091, not a different number
/// here.
///
/// Set `KHIVE_BRIDGE_IDLE_TIMEOUT_SECS` to a positive number of seconds to
/// enable it. `0`, absent, and unparsable all leave it disabled; unparsable
/// falls back rather than panicking, matching this codebase's other
/// `_from_env` helpers, and it falls back to *disabled* because a typo must
/// never silently start closing live sessions. 3600 is the suggested value
/// where it is wanted: long enough that ordinary gaps in a live session never
/// trip it.
pub(super) fn stdio_bridge_idle_timeout_from_env() -> Option<std::time::Duration> {
    let secs = khive_storage::read_env_number::<u64>("KHIVE_BRIDGE_IDLE_TIMEOUT_SECS").unwrap_or(0);
    if secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(secs))
    }
}

/// How long an admitted request whose response has not been written keeps
/// deferring the idle close.
///
/// The idle check must not treat a session with work outstanding as idle, or
/// it cancels a running handler out from under itself. But rmcp spawns each
/// request handler and drops the join handle, so a handler that panics never
/// reaches the response construction that would clear its obligation. Deferring
/// on an outstanding obligation with no bound therefore hands any panicking
/// handler the power to disable idle reaping for the life of the session — the
/// exact unbounded lifetime the idle timeout exists to close, reintroduced
/// through the guard that protects it.
///
/// This bound is separate from the idle window on purpose. Reusing the idle
/// window would mean a handler that runs longer than one quiet window stops
/// protecting its own session, which is the guarantee the obligation exists to
/// provide. It is set far above any real handler and answers a different
/// question: not "has this session been quiet" but "has this request been
/// outstanding so long that its handler must be gone".
///
/// Overridable via `KHIVE_BRIDGE_REQUEST_OBLIGATION_SECS`; `0` disables the
/// bound, restoring the unbounded defer. An unparsable value falls back to the
/// default. Default: 3600s.
pub(super) fn stdio_bridge_request_obligation_ttl_from_env() -> Option<std::time::Duration> {
    const DEFAULT_SECS: u64 = 3600;
    let secs = khive_storage::read_env_number::<u64>("KHIVE_BRIDGE_REQUEST_OBLIGATION_SECS")
        .unwrap_or(DEFAULT_SECS);
    if secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(secs))
    }
}

/// Maximum number of requests a stdio bridge admits to rmcp while their
/// responses are still outstanding. A full session is closed before another
/// handler is spawned, bounding the per-session handler and obligation state.
///
/// Overridable via `KHIVE_BRIDGE_MAX_OUTSTANDING_REQUESTS`. Values must be
/// positive; `0`, an unparsable value, or a value too large for this platform
/// falls back to the default. Default: 1024, enough for ordinary concurrent
/// MCP traffic while keeping a peer that stops reading from growing the
/// session without limit.
pub(super) fn stdio_bridge_max_outstanding_requests_from_env() -> usize {
    khive_storage::read_env_number::<usize>("KHIVE_BRIDGE_MAX_OUTSTANDING_REQUESTS")
        .filter(|&value| value > 0)
        .unwrap_or(crate::transport::DEFAULT_MAX_OUTSTANDING_REQUESTS)
}

/// Response-delivery deadline for a stdio bridge session: the longest a
/// single response write may stay pending before it is abandoned (see
/// [`crate::transport::CancelOnEofTransport::send`]) and this session is
/// closed. Independent of the idle timeout above — it bounds an admitted
/// request's response write directly, rather than the gap between
/// requests. Without this bound, a peer that admits a request and then
/// stops reading its response — while leaving the pipe itself open — keeps
/// that write pending forever, so the idle timeout would defer indefinitely
/// (an in-flight response always defers idle-close) and the session would
/// never be reaped.
///
/// Overridable via `KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS`, accepted range
/// 1..=u64::MAX seconds. Unlike `KHIVE_BRIDGE_IDLE_TIMEOUT_SECS`, this bound
/// cannot be disabled: `0` is a startup error naming the variable, the
/// rejected value, and the accepted range, rather than a silent opt-out — a
/// configuration that restores an unbounded pending write restores the
/// defect this deadline exists to close (see the type doc above). An
/// unparsable value falls back to the default rather than erroring, matching
/// this codebase's other `_from_env` helpers. Default: 300s (5 minutes) —
/// long enough that legitimately slow verbs and ordinary reader backpressure
/// never trip it, short enough that a peer that has genuinely stopped
/// reading does not pin the session's reader-pool admission / DB connection
/// indefinitely.
pub(super) fn stdio_bridge_response_deadline_from_env() -> anyhow::Result<std::time::Duration> {
    const DEFAULT_SECS: u64 = 300;
    let secs = match std::env::var("KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) => secs,
            Err(_) => DEFAULT_SECS,
        },
        Err(_) => DEFAULT_SECS,
    };
    if secs == 0 {
        anyhow::bail!(
            "KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS=0 is not accepted: the response-delivery \
             deadline cannot be disabled (a disabled deadline lets a peer that stops reading \
             pin the bridge's response write forever). Set it to a positive number of seconds \
             (accepted range: 1..=u64::MAX, default {DEFAULT_SECS})."
        );
    }
    Ok(std::time::Duration::from_secs(secs))
}
