//! khived daemon server — persistent warm runtime over a Unix socket.
//!
//! The daemon binds `~/.khive/khived.sock`, accepts length-prefixed request
//! frames, dispatches them through a [`DaemonDispatch`] implementor, and serves
//! results back. It is transport-agnostic: the MCP crate provides the dispatch
//! impl, but any future client (CLI, HTTP gateway) can reuse this server.
//!
//! The client side (forwarding, auto-spawn) lives in the transport crate
//! (e.g. `khive-mcp`), not here.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use async_trait::async_trait;
#[cfg(unix)]
use libc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use khive_db::{run_checkpoint_task, CheckpointConfig, ConnectionPool};

use crate::pack::RequestIdentity;

/// Maximum frame size accepted in either direction.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Wire protocol version for the daemon IPC framing.
///
/// Increment this constant whenever the request or response frame shape
/// changes in a backward-incompatible way. The client sends its version
/// in every request; the daemon rejects mismatches with an explicit error
/// that names both sides so the operator knows exactly what to do
/// (`make local` rebuilds the client binary).
///
/// Version history:
///   1 — initial versioned framing (added `protocol_version` + `version_mismatch`);
///       added `probe_only` request field + probe-ack sentinel shape in response
///   2 — gate subhandler verbs by wire origin (`from_wire` request field)
///   3 — added per-request identity context to the request frame (`actor_id`,
///       `visible_namespaces`, ADR-096 Fork 1); the daemon now serves a request
///       under the frame's identity instead of rejecting on `namespace_mismatch`
///       (the `config_id` equality reject stays hard)
pub const PROTOCOL_VERSION: u32 = 3;

const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 10;

// ── paths ─────────────────────────────────────────────────────────────────────

fn khive_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".khive")
}

/// Unix socket path the daemon binds and clients connect to.
///
/// Overridable via the `KHIVE_SOCKET` env var (for tests and ops).
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("KHIVE_SOCKET") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    khive_dir().join("khived.sock")
}

/// PID file path written by the daemon.
///
/// Overridable via the `KHIVE_PID` env var.
pub fn pid_path() -> PathBuf {
    if let Ok(p) = std::env::var("KHIVE_PID") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    khive_dir().join("khived.pid")
}

/// Advisory lock file used to serialize stale-daemon recovery across concurrent
/// clients (flock on the file; released when the lock file handle is dropped).
///
/// Overridable via the `KHIVE_LOCK` env var (for tests).
pub fn lock_path() -> PathBuf {
    if let Ok(p) = std::env::var("KHIVE_LOCK") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    khive_dir().join("khived.recovery.lock")
}

/// Acquire an exclusive advisory flock on the recovery/startup lock file.
///
/// The returned `File` holds the lock for its lifetime; dropping it releases
/// it.  Used by both the client (serializing kill+spawn) and the daemon server
/// (serializing cleanup+bind+pid-write) so the two critical sections are
/// mutually exclusive across processes.
#[cfg(unix)]
pub fn acquire_recovery_lock() -> Option<std::fs::File> {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, path = ?path, "cannot open recovery lock file");
            return None;
        }
    };
    // SAFETY: flock is a POSIX advisory lock with no memory side-effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        tracing::warn!("flock LOCK_EX failed on recovery lock");
        return None;
    }
    Some(file)
}

// ── wire types ────────────────────────────────────────────────────────────────

/// Request frame sent from a client to the daemon.
#[derive(Serialize, Deserialize, Default)]
pub struct DaemonRequestFrame {
    pub ops: String,
    pub presentation: Option<String>,
    pub presentation_per_op: Option<Vec<Option<String>>>,
    /// The client's resolved storage/gate default namespace for this request.
    ///
    /// Historically rejected outright when it differed from the daemon's own
    /// construction-baked namespace (`namespace_mismatch`). As of protocol
    /// version 3 (ADR-096 Fork 1) the daemon instead serves the request under
    /// this namespace — the field is a per-request identity input, not a
    /// same-process-identity assertion.
    pub namespace: String,
    /// The client's resolved write-stamp / gate actor identity (ADR-057),
    /// carried on the frame so the warm daemon stamps writes with the
    /// *caller's* actor instead of the daemon's own baked `actor_id`
    /// (ADR-096 Fork 1). `None` mints `ActorRef::anonymous()`, matching an
    /// unconfigured actor. Pre-Fork-1 clients cannot send this field (an older
    /// protocol version rejects at the version check before it would matter).
    #[serde(default)]
    pub actor_id: Option<String>,
    /// The client's resolved extra read-visibility namespaces (ADR-007 Rev 4
    /// Rule 3b), carried on the frame so the warm daemon widens read scope to
    /// match the caller's own configuration rather than the daemon's baked
    /// `visible_namespaces` (ADR-096 Fork 1). Empty means no extra visibility
    /// beyond `namespace` itself.
    #[serde(default)]
    pub visible_namespaces: Vec<String>,
    /// Fingerprint of the client's resolved runtime config (packs, db target,
    /// embedders). The daemon rejects a request whose `config_id` differs from
    /// its own so a restricted client (e.g. `--pack kg`, `--db :memory:`) never
    /// dispatches through the broader default daemon. See ADR-027 / ADR-049.
    #[serde(default)]
    pub config_id: String,
    /// IPC protocol version sent by the client. Pre-versioning clients omit
    /// this field (deserializes to 0). The daemon compares against
    /// [`PROTOCOL_VERSION`] and rejects mismatches with an explicit error.
    #[serde(default)]
    pub protocol_version: u32,
    /// When `true`, the daemon returns an identity frame (ok=true, result=None)
    /// immediately after identity validation — without calling the dispatcher.
    /// Used by the client's under-lock recovery probe to confirm a daemon is
    /// alive and identity-matching without dispatching any mutating verb.
    /// Pre-probe clients omit this field (deserializes to false → normal dispatch).
    #[serde(default)]
    pub probe_only: bool,
    /// When `true`, the daemon returns a point-in-time [`MetricsSnapshot`] of
    /// its server-side gauges (a read-only measurement surface for the
    /// load/perf harness) instead of dispatching any op. Handled before the
    /// `config_id` equality reject: a gauge read is process-global and
    /// namespace/config-agnostic, not a namespaced record operation.
    /// READ-ONLY — this field is the only input the frame accepts for a
    /// metrics request; there is no reset or mutation reachable over the
    /// wire. Pre-metrics clients omit this field (deserializes to `false` →
    /// normal dispatch, unaffected).
    #[serde(default)]
    pub metrics_only: bool,
    /// Output format for this request (ADR-078). Forwarded to the daemon's
    /// serialization seam. `None` means use the daemon's resolved default.
    #[serde(default)]
    pub format: Option<String>,
    /// Per-operation output format overrides (ADR-078).
    #[serde(default)]
    pub format_per_op: Option<Vec<Option<String>>>,
    /// Whether this request originated from the agent-facing MCP `request`
    /// tool (the wire surface). When `true`, the daemon enforces verb
    /// visibility — `Visibility::Subhandler` verbs are rejected because agents
    /// must not invoke internal subhandlers. When `false` (the default, and the
    /// only value any operator path sends), subhandlers are allowed: `kkernel
    /// exec` and other in-process callers are trusted operator surfaces.
    ///
    /// This is the origin discriminator, not a daemon-vs-local one: operator
    /// requests flow through the daemon by default too, so the gate cannot key
    /// on transport. Pre-versioning clients omit this field (deserializes to
    /// `false` → ungated), which is safe: the only way to reach the gated wire
    /// surface is the MCP `request` tool, which always sets it explicitly.
    #[serde(default)]
    pub from_wire: bool,
}

/// Response frame sent from the daemon back to a client.
#[derive(Serialize, Deserialize)]
pub struct DaemonResponseFrame {
    pub ok: bool,
    pub result: Option<String>,
    pub error: Option<String>,
    pub namespace_mismatch: bool,
    /// Set when the request's `config_id` does not match the daemon's. Like
    /// `namespace_mismatch`, this signals the client to fall back to local
    /// dispatch rather than execute under a different runtime/config.
    #[serde(default)]
    pub config_mismatch: bool,
    /// The `config_id` the daemon dispatched under, echoed back so the client
    /// can positively confirm the result came from a matching runtime. A
    /// pre-`config_id` daemon omits this field (deserializes to `None`), which
    /// the client treats as a mismatch and falls back to local dispatch — this
    /// closes the upgrade window where a new restricted client could otherwise
    /// trust a still-warm legacy daemon's broader registry.
    #[serde(default)]
    pub served_config_id: Option<String>,
    /// Set when the client's `protocol_version` does not match the daemon's
    /// [`PROTOCOL_VERSION`]. The client must treat this as a hard error and
    /// surface the human-readable `error` field rather than falling back to
    /// local dispatch (which would hide the version skew).
    #[serde(default)]
    pub version_mismatch: bool,
    /// The daemon's [`PROTOCOL_VERSION`], echoed in error responses so the
    /// client can include both sides in the diagnostic message. Pre-versioning
    /// daemons omit this field (deserializes to 0).
    #[serde(default)]
    pub daemon_protocol_version: u32,
    /// Populated when the request set `metrics_only: true`: a point-in-time
    /// snapshot of the daemon's server-side gauges. `None` on every other
    /// response, and on any response from a daemon that predates this field
    /// (client-side back-compat via `#[serde(default)]`, matching
    /// `served_config_id`'s upgrade-window handling above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsSnapshot>,
}

/// Point-in-time snapshot of the daemon's server-side gauges — the
/// load/perf harness read-surface (measurement substrate, not a product feature).
///
/// Every field here is a **server-side** gauge reachable from `handle_conn`
/// without any mutation: [`khive_storage::tx_registry`] (ADR-091 Plank 0,
/// process-global singleton), the WAL checkpoint task's last-observed page
/// count and TRUNCATE counters (`khive_db::checkpoint`), and the ADR-067
/// Component A write queue depth (only when `KHIVE_WRITE_QUEUE=1`). There is
/// no reset reachable through this type or through [`DaemonRequestFrame`] —
/// gauges out, nothing in.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct MetricsSnapshot {
    /// Last-observed WAL page count from the periodic checkpoint tick.
    /// `None` when the checkpoint task has never ticked in this process
    /// (for example, an in-memory dispatcher with no pool, or a daemon that
    /// just started and hasn't hit its first tick yet).
    pub wal_pages: Option<u64>,
    /// Total WAL TRUNCATE escalation attempts (ADR-091 Plank 2) made in this
    /// process's lifetime, regardless of whether they succeeded in reclaiming
    /// pages.
    pub wal_truncate_attempts: u64,
    /// Current consecutive-failure count for TRUNCATE attempts that failed to
    /// bring the WAL back below `warn_pages`; resets to 0 the next time an
    /// attempt clears it.
    pub wal_truncate_consecutive_failures: u64,
    /// Age, in microseconds, of the oldest currently-open transaction
    /// registry entry (ADR-091 Plank 0). `None` when no transaction is
    /// currently open.
    pub oldest_pinned_tx_micros: Option<u64>,
    /// Diagnostic label of the oldest currently-open transaction registry
    /// entry, if any and if it was registered with one.
    pub oldest_pinned_tx_label: Option<String>,
    /// Number of currently open transaction registry entries.
    pub open_tx_count: usize,
    /// Current write-queue backlog depth (ADR-067 Component A): requests
    /// enqueued but not yet accepted by the `WriterTask` drain loop. `None`
    /// unless the write queue is enabled (`KHIVE_WRITE_QUEUE=1`) and a
    /// file-backed pool is available.
    pub write_queue_depth: Option<usize>,
    /// The write queue's configured bounded capacity
    /// (`PoolConfig::write_queue_capacity`), gated the same as
    /// `write_queue_depth`.
    pub write_queue_capacity: Option<usize>,
}

// ── framing ───────────────────────────────────────────────────────────────────

/// Read one length-prefixed frame (4-byte BE u32 length + JSON bytes).
pub async fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("daemon frame of {len} bytes exceeds {MAX_FRAME_BYTES} cap"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write one length-prefixed frame.
pub async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "daemon frame of {} bytes exceeds {MAX_FRAME_BYTES} cap",
                payload.len()
            ),
        ));
    }
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

// ── dispatch trait ────────────────────────────────────────────────────────────

/// Transport-agnostic dispatch interface for the daemon server.
///
/// The MCP crate implements this by dispatching through the shared request body
/// while honoring [`DaemonRequestFrame::from_wire`] (so subhandler visibility is
/// gated by request origin, not by transport); any future transport can do the
/// same.
#[async_trait]
pub trait DaemonDispatch: Clone + Send + Sync + 'static {
    /// Dispatch a verb-DSL request string and return the rendered result.
    ///
    /// `from_wire` carries the origin discriminator from
    /// [`DaemonRequestFrame::from_wire`]: when `true`, the implementor enforces
    /// verb visibility (rejects `Visibility::Subhandler` verbs); when `false`,
    /// the request is from a trusted operator surface and subhandlers pass.
    ///
    /// `identity` is the per-request identity context threaded from the frame
    /// (ADR-096 Fork 1): `Some(..)` when serving a request forwarded over the
    /// daemon socket (built from `frame.namespace` / `frame.actor_id` /
    /// `frame.visible_namespaces` by the connection handler), `None` for any
    /// other dispatch path. Implementors should mint the storage/gate token from
    /// `identity` when present and fall back to their own construction-baked
    /// identity when absent, so pure local (non-daemon) dispatch is unchanged.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<RequestIdentity>,
    ) -> Result<String, String>;

    /// Warm every pack's in-memory state (ANN indexes, etc.).
    async fn warm_all(&self);

    /// The namespace this dispatcher was configured for.
    fn namespace(&self) -> &str;

    /// Fingerprint of this dispatcher's resolved runtime config (packs, db
    /// target, embedders). Used to reject forwarded requests from clients whose
    /// config differs, so a restricted client cannot dispatch through a broader
    /// daemon.
    fn config_id(&self) -> &str;

    /// Return the pool to use for background WAL checkpointing, if available.
    ///
    /// Implementors backed by a file-based SQLite database should return
    /// `Some(pool_arc)`. In-memory or test dispatchers that have no pool
    /// return `None` and the checkpoint task is not spawned.
    ///
    /// The default implementation returns `None`.
    fn pool_for_checkpoint(&self) -> Option<Arc<ConnectionPool>> {
        None
    }
}

// ── tracked background tasks ─────────────────────────────────────────────────
//
// Pack handlers (e.g. memory.recall's ADR-081 §5 serve-ledger append) fire
// fire-and-forget `tokio::spawn`ed work off the response path so the caller
// never waits on a cross-pack dispatch or a SQL write. Left untracked, that
// work is invisible to `drain()`: a SIGTERM landing between the response
// returning and the spawned task completing can abort it mid-flight with no
// log and no row (internal review PR #583 round-1 Medium). `track_background_task`
// gives such spawns a process-wide presence that `drain()` waits on, exactly
// like the `active` counter does for in-flight connections — the caller still
// only pays for the spawn + counter increment, never the task's own work.
static BACKGROUND_TASKS: std::sync::OnceLock<Arc<std::sync::atomic::AtomicUsize>> =
    std::sync::OnceLock::new();

fn background_tasks() -> &'static Arc<std::sync::atomic::AtomicUsize> {
    BACKGROUND_TASKS.get_or_init(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
}

/// Decrements the shared background-task counter from `Drop`, so the count
/// comes back down whether the tracked future returns normally, panics, or
/// is cancelled — a plain post-`await` `fetch_sub` only covers the return
/// path and leaks the count forever on a panic (internal review PR #583 round-2
/// Medium), since unwinding skips every statement after the panic point.
struct BackgroundTaskGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for BackgroundTaskGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Spawn a fire-and-forget background task that daemon shutdown's `drain()`
/// waits for, instead of a bare `tokio::spawn` that a SIGTERM can abort
/// mid-flight with no trace. Only the enqueue (an atomic increment) is
/// synchronous on the caller's path — the future itself still runs fully
/// off-path, unawaited. The decrement happens via `BackgroundTaskGuard`'s
/// `Drop`, so a panic inside `fut` still restores the count.
pub fn track_background_task<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    background_tasks().fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let guard = BackgroundTaskGuard {
        counter: background_tasks().clone(),
    };
    tokio::spawn(async move {
        let _guard = guard;
        fut.await;
    });
}

/// Current count of in-flight tasks started via [`track_background_task`].
/// Exposed for tests; `drain()` reads the shared counter directly.
pub fn background_task_count() -> usize {
    background_tasks().load(std::sync::atomic::Ordering::Relaxed)
}

// ── server ────────────────────────────────────────────────────────────────────

/// Build a point-in-time [`MetricsSnapshot`] of this process's server-side
/// gauges. Called only from `handle_conn`'s `metrics_only` arm — a
/// process-global, read-only assembly with no side effects of its own.
///
/// `tx_registry` (ADR-091 Plank 0) is a process-global singleton reachable
/// directly, with no plumbing through `dispatcher`. `wal_pages` and the
/// TRUNCATE counters (ADR-091 Plank 2) are read from `khive_db::checkpoint`'s
/// module-scoped atomics, updated wherever the checkpoint task already calls
/// `query_wal_pages`/`note_truncate_outcome` — mirroring the fallback-counter
/// pattern in `khive-mcp/src/daemon.rs` rather than threading a metrics
/// handle through every checkpoint call site, since the checkpoint task
/// itself is a fire-and-forget `tokio::spawn` with no handle retained
/// anywhere this accept loop can reach. `write_queue_depth`/`_capacity`
/// (ADR-067 Component A) come from the dispatcher's own pool, if any, and
/// are `None` unless `KHIVE_WRITE_QUEUE=1` actually spawned a writer task.
fn build_metrics_snapshot<D: DaemonDispatch>(dispatcher: &D) -> MetricsSnapshot {
    let open_tx_count = khive_storage::tx_registry::snapshot().len();
    let (oldest_pinned_tx_micros, oldest_pinned_tx_label) =
        match khive_storage::tx_registry::oldest() {
            Some((age, label)) => (Some(age.as_micros() as u64), label),
            None => (None, None),
        };

    let (write_queue_depth, write_queue_capacity) = dispatcher
        .pool_for_checkpoint()
        .and_then(|pool| pool.writer_task_handle().ok().flatten())
        .map(|handle| (Some(handle.queue_depth()), Some(handle.capacity())))
        .unwrap_or((None, None));

    MetricsSnapshot {
        wal_pages: khive_db::checkpoint::last_observed_wal_pages(),
        wal_truncate_attempts: khive_db::checkpoint::truncate_attempts(),
        wal_truncate_consecutive_failures: khive_db::checkpoint::truncate_consecutive_failures(),
        oldest_pinned_tx_micros,
        oldest_pinned_tx_label,
        open_tx_count,
        write_queue_depth,
        write_queue_capacity,
    }
}

async fn handle_conn<D: DaemonDispatch>(mut stream: UnixStream, dispatcher: D) {
    let raw = match read_frame(&mut stream).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "failed to read daemon request frame");
            return;
        }
    };
    let frame: DaemonRequestFrame = match serde_json::from_slice(&raw) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(error = %e, "failed to decode daemon request frame");
            return;
        }
    };

    let served_config_id = Some(dispatcher.config_id().to_string());
    let resp = if frame.protocol_version != PROTOCOL_VERSION {
        let msg = format!(
            "daemon protocol mismatch: client={} daemon={} — \
             rebuild/update the client binary (make local)",
            frame.protocol_version, PROTOCOL_VERSION,
        );
        tracing::warn!(
            client_version = frame.protocol_version,
            daemon_version = PROTOCOL_VERSION,
            "daemon protocol version mismatch"
        );
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some(msg),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: true,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
        }
    } else if frame.metrics_only {
        // Process-global gauge read: namespace/config-agnostic, so this is
        // handled BEFORE the `config_id` equality reject below (unlike every
        // other arm) — a metrics probe must work regardless of which
        // client's config is asking, since it never touches the dispatcher's
        // packs/db/embed registry. READ-ONLY: builds a snapshot and returns
        // immediately, never reaching the ops-dispatch arm.
        DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: Some(build_metrics_snapshot(&dispatcher)),
        }
    // ADR-096 Fork 1: there is no `frame.namespace != dispatcher.namespace()`
    // reject here. The daemon accepts and serves the request under the
    // frame's own identity (namespace / actor / visible_namespaces, built
    // into a `RequestIdentity` below) over its one shared warm registry,
    // rather than rejecting a differently-attributed same-uid connection to
    // a cold local-dispatch fallback. `config_id` — which governs
    // packs/db/embed coherence for the shared warm engine — remains a hard
    // reject; it is not an identity field and softening it would let a
    // restricted client dispatch through an incompatible broader daemon.
    } else if frame.config_id != dispatcher.config_id() {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: true,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
        }
    } else if frame.probe_only {
        // Probe-only request: identity checks passed; return immediately without
        // dispatching any verb. The client uses this to confirm the daemon is
        // alive and identity-matching without triggering any mutation.
        DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
        }
    } else {
        // Build the per-request identity context from the frame (ADR-096
        // Fork 1) so the implementor mints the storage/gate token from the
        // CALLER's identity, not the dispatcher's own construction-baked
        // scalars. This is always `Some` here: every frame that reaches
        // this arm carries a `namespace` (required on the wire) plus
        // whatever `actor_id`/`visible_namespaces` the client resolved
        // (defaulting to `None`/`vec![]` for a pre-Fork-1 field-absent
        // payload, which is exactly the prior anonymous/no-extra-visibility
        // behavior).
        let identity = RequestIdentity {
            namespace: frame.namespace.clone(),
            actor_id: frame.actor_id.clone(),
            visible_namespaces: frame.visible_namespaces.clone(),
        };
        match dispatcher
            .dispatch(
                frame.ops,
                frame.presentation,
                frame.presentation_per_op,
                frame.format,
                frame.format_per_op,
                frame.from_wire,
                Some(identity),
            )
            .await
        {
            Ok(result) => DaemonResponseFrame {
                ok: true,
                result: Some(result),
                error: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id,
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
            },
            Err(e) => DaemonResponseFrame {
                ok: false,
                result: None,
                error: Some(e),
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id,
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
            },
        }
    };

    match serde_json::to_vec(&resp) {
        Ok(payload) => {
            if payload.len() > MAX_FRAME_BYTES {
                // The serialized response exceeds the IPC frame cap.  Send a
                // small explicit error frame so the client can distinguish a
                // per-request payload-size failure from a daemon crash.  A
                // client that receives this error frame will NOT trigger
                // stale-daemon kill/respawn (ParseFailure requires a read_frame
                // error, not an ok=false result).
                tracing::warn!(
                    bytes = payload.len(),
                    limit = MAX_FRAME_BYTES,
                    "daemon response exceeds MAX_FRAME_BYTES; sending explicit error frame"
                );
                let err_resp = DaemonResponseFrame {
                    ok: false,
                    result: None,
                    error: Some(format!(
                        "response too large: {} bytes exceeds {} byte IPC cap",
                        payload.len(),
                        MAX_FRAME_BYTES,
                    )),
                    namespace_mismatch: false,
                    config_mismatch: false,
                    served_config_id: resp.served_config_id,
                    version_mismatch: false,
                    daemon_protocol_version: PROTOCOL_VERSION,
                    metrics: None,
                };
                if let Ok(err_payload) = serde_json::to_vec(&err_resp) {
                    if let Err(e) = write_frame(&mut stream, &err_payload).await {
                        tracing::debug!(error = %e, "failed to write oversized-response error frame");
                    }
                }
            } else if let Err(e) = write_frame(&mut stream, &payload).await {
                tracing::debug!(error = %e, "failed to write daemon response frame");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to serialize daemon response frame"),
    }
}

/// Run the daemon: bind the socket, warm in the background, serve request
/// frames until SIGTERM/SIGINT.
pub async fn run_daemon<D: DaemonDispatch>(dispatcher: D) -> anyhow::Result<()> {
    let sock = socket_path();
    let pid_file = pid_path();

    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, path = ?parent, "failed to chmod 0700 khive dir");
        }
    }

    // Hold the startup lock across cleanup → bind → pid-write so a concurrent
    // client's kill_and_respawn (which also holds this lock) cannot unlink the
    // socket between our bind and our pid-write.  The lock is released once the
    // listener is bound and the PID file is written — at that point any racing
    // client will find a live socket+pid and skip the stale-cleanup path.
    //
    // Deadlock note: the client holds this lock only during kill+spawn and
    // releases it before the spawned daemon process starts (the lock guard is
    // dropped when kill_and_respawn returns, before the readiness probe loop).
    // The daemon then acquires the lock here without any client holding it.
    #[cfg(unix)]
    let _startup_lock = acquire_recovery_lock();

    if !cleanup_stale_daemon(&sock, &pid_file).await {
        tracing::info!("a responsive khived is already running; exiting");
        return Ok(());
    }

    let listener = UnixListener::bind(&sock)?;
    #[cfg(unix)]
    if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, path = ?sock, "failed to chmod 0600 socket");
    }

    write_pid_file(&pid_file)?;
    // Release the startup lock now: the listener is bound and the PID file is
    // written.  Any concurrent client or daemon startup will observe a live
    // socket+pid and take the non-recovery path.
    #[cfg(unix)]
    drop(_startup_lock);
    tracing::info!(socket = ?sock, pid = std::process::id(), "khived listening");

    {
        let warm = dispatcher.clone();
        tokio::spawn(async move {
            warm.warm_all().await;
        });
    }

    if let Some(pool) = dispatcher.pool_for_checkpoint() {
        let cfg = CheckpointConfig::from_env();
        tokio::spawn(run_checkpoint_task(pool, cfg));
        tracing::info!("WAL checkpoint task started");
    }

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let shutdown = async {
        // REASON: signal handler registration can only fail if the global Tokio runtime
        // is not running or the OS rejects the signal number — both are unrecoverable
        // at this point in startup, so panic is the correct response.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
        }
    };

    tokio::select! {
        _ = async {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let d = dispatcher.clone();
                        let active = Arc::clone(&active);
                        tokio::spawn(async move {
                            active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            handle_conn(stream, d).await;
                            active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        });
                    }
                    Err(e) => tracing::error!(error = %e, "accept failed"),
                }
            }
        } => {}
        _ = shutdown => {}
    }

    drain(&active).await;

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&pid_file);
    tracing::info!("khived stopped");
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn is_process_running(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 is an existence/permission probe with no side effects.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn cleanup_stale_daemon(sock: &std::path::Path, pid_file: &std::path::Path) -> bool {
    if let Ok(pid_str) = std::fs::read_to_string(pid_file) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if pid != std::process::id()
                && is_process_running(pid)
                && sock.exists()
                && UnixStream::connect(sock).await.is_ok()
            {
                return false;
            }
        }
    }
    if sock.exists() {
        if let Err(e) = std::fs::remove_file(sock) {
            tracing::warn!(error = %e, path = ?sock, "failed to remove stale socket");
        }
    }
    if pid_file.exists() {
        if let Err(e) = std::fs::remove_file(pid_file) {
            tracing::warn!(error = %e, path = ?pid_file, "failed to remove stale PID file");
        }
    }
    true
}

fn write_pid_file(pid_file: &std::path::Path) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(pid_file)?;
    f.write_all(std::process::id().to_string().as_bytes())?;
    Ok(())
}

async fn drain(active: &std::sync::atomic::AtomicUsize) {
    use std::sync::atomic::Ordering;
    let remaining = || active.load(Ordering::Relaxed) + background_task_count();
    if remaining() == 0 {
        return;
    }
    let deadline = tokio::time::Instant::now() + drain_timeout();
    while remaining() > 0 {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                remaining_connections = active.load(Ordering::Relaxed),
                remaining_background_tasks = background_task_count(),
                "drain timeout reached; forcing shutdown"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn drain_timeout() -> std::time::Duration {
    let secs = std::env::var("KHIVE_DRAIN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAIN_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Returns `true` for non-empty env values that are not `"0"` or `"false"`.
pub fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Focused regression tests for the unsafe process probe (SAFETY: signal 0
    // is an existence check with no side effects; see is_process_running).

    #[test]
    fn current_process_is_running() {
        // The current PID is always alive.
        let pid = std::process::id();
        assert!(
            is_process_running(pid),
            "current process {pid} should be detected as running"
        );
    }

    #[test]
    fn pid_zero_is_not_running() {
        // PID 0 is the process group; kill(0, 0) sends to the group,
        // which we treat as invalid — the guard `pid <= 0` must block it.
        assert!(
            !is_process_running(0),
            "pid 0 must be rejected by the guard before the unsafe call"
        );
    }

    #[test]
    fn very_large_pid_is_not_running() {
        // u32::MAX overflows i32 — try_from returns Err, guard returns false.
        assert!(
            !is_process_running(u32::MAX),
            "u32::MAX should fail i32 conversion and return false"
        );
    }

    #[test]
    fn env_truthy_recognises_set_values() {
        assert!(!env_truthy("__KHIVE_TEST_ABSENT_VAR_XYZ__"));

        // env_truthy with a live value — set and unset atomically to avoid
        // cross-test pollution (not parallel-safe without serial_test, but these
        // are fast unit tests and the variable name is unique).
        let key = "__KHIVE_TEST_TRUTHY_ABC__";
        std::env::set_var(key, "1");
        assert!(env_truthy(key));
        std::env::set_var(key, "false");
        assert!(!env_truthy(key));
        std::env::set_var(key, "0");
        assert!(!env_truthy(key));
        std::env::remove_var(key);
    }

    // internal review PR #583 round-1 Medium: `drain()` must wait for tracked background
    // tasks (e.g. memory.recall's serve-ledger append), not just in-flight
    // connections, or a SIGTERM lands mid-flight with no log and no row.
    //
    // `#[serial(background_tasks)]`: this test reads/asserts on the
    // process-wide `BACKGROUND_TASKS` static shared with the two counter
    // tests below. Under default parallel execution one test's increment
    // leaks into another's snapshot-then-assert window (internal review PR #583
    // round-3 Medium, reproduced: both counter tests failed together,
    // passed with `--test-threads=1`). Serializing just this named group
    // isolates them from each other without forcing the whole test binary
    // (including unrelated `#[serial]` tests elsewhere in this crate) onto
    // one thread.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn drain_waits_for_tracked_background_tasks_before_returning() {
        let active = std::sync::atomic::AtomicUsize::new(0);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        track_background_task(async move {
            let _ = rx.await;
        });
        assert!(
            background_task_count() >= 1,
            "track_background_task must make the in-flight task visible immediately"
        );

        let drain_fut = drain(&active);
        tokio::pin!(drain_fut);

        // Must NOT resolve while the tracked task is still pending.
        let too_early =
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut drain_fut).await;
        assert!(
            too_early.is_err(),
            "drain() must not return while a tracked background task is still running"
        );

        // Completing the task must let drain() proceed promptly.
        tx.send(())
            .expect("tracked task still awaiting the oneshot");
        let done = tokio::time::timeout(std::time::Duration::from_secs(5), drain_fut).await;
        assert!(
            done.is_ok(),
            "drain() must return once the tracked background task finishes"
        );
    }

    // See the `#[serial(background_tasks)]` note on
    // `drain_waits_for_tracked_background_tasks_before_returning` above —
    // this test shares the same process-wide `BACKGROUND_TASKS` static and
    // races it (and the panic test below) under default parallelism.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn track_background_task_count_returns_to_zero_after_completion() {
        // Sanity check on the counter's own bookkeeping, independent of drain().
        let before = background_task_count();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        track_background_task(async move {
            let _ = rx.await;
        });
        assert_eq!(background_task_count(), before + 1);
        tx.send(()).expect("still awaiting");
        // Yield until the spawned task's decrement has actually run.
        for _ in 0..100 {
            if background_task_count() == before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(background_task_count(), before);
    }

    // See the `#[serial(background_tasks)]` note above — shares
    // `BACKGROUND_TASKS` with the other two tests in this group.
    #[tokio::test]
    #[serial(background_tasks)]
    async fn track_background_task_count_returns_to_baseline_after_panic() {
        // internal review PR #583 round-2 Medium: a panic inside the tracked future must
        // still decrement the counter (via BackgroundTaskGuard's Drop), not
        // leak it forever. `track_background_task` discards the spawned
        // task's `JoinHandle` (it is fire-and-forget by design — the caller
        // never awaits it), so this test does not await the panic directly;
        // tokio isolates the panic to the spawned task instead of aborting
        // the process, and we observe the recovery purely through the
        // shared counter returning to baseline after the guard's `Drop`
        // fires during that task's unwind.
        let before = background_task_count();

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        track_background_task(async move {
            let _ = rx.await;
            panic!("intentional panic to exercise the Drop-guard decrement path");
        });
        assert_eq!(background_task_count(), before + 1);

        tx.send(()).expect("still awaiting");
        for _ in 0..100 {
            if background_task_count() == before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            background_task_count(),
            before,
            "background task counter must return to baseline after the tracked future panics"
        );
    }

    // ── metrics-only frame (load/perf harness read-surface) ────────────────

    /// Minimal `DaemonDispatch` for the metrics tests: `dispatch` just counts
    /// how many times it was called (so tests can assert the ops path was
    /// never reached) and `pool_for_checkpoint` returns whatever pool the
    /// test wired in (or `None`, matching an in-memory/poolless dispatcher).
    #[derive(Clone)]
    struct MockDispatch {
        namespace: String,
        config_id: String,
        dispatch_calls: Arc<std::sync::atomic::AtomicUsize>,
        pool: Option<Arc<ConnectionPool>>,
    }

    #[async_trait]
    impl DaemonDispatch for MockDispatch {
        async fn dispatch(
            &self,
            _ops: String,
            _presentation: Option<String>,
            _presentation_per_op: Option<Vec<Option<String>>>,
            _format: Option<String>,
            _format_per_op: Option<Vec<Option<String>>>,
            _from_wire: bool,
            _identity: Option<RequestIdentity>,
        ) -> Result<String, String> {
            self.dispatch_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("{}".to_string())
        }

        async fn warm_all(&self) {}

        fn namespace(&self) -> &str {
            &self.namespace
        }

        fn config_id(&self) -> &str {
            &self.config_id
        }

        fn pool_for_checkpoint(&self) -> Option<Arc<ConnectionPool>> {
            self.pool.clone()
        }
    }

    fn base_request_frame(config_id: &str) -> DaemonRequestFrame {
        DaemonRequestFrame {
            ops: String::new(),
            presentation: None,
            presentation_per_op: None,
            namespace: "local".to_string(),
            actor_id: None,
            visible_namespaces: Vec::new(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: false,
        }
    }

    /// Drive `handle_conn` over an in-process `UnixStream::pair()` (no real
    /// socket file needed) and decode the response frame it writes back.
    async fn round_trip(dispatcher: MockDispatch, req: &DaemonRequestFrame) -> DaemonResponseFrame {
        let (mut client, server) = UnixStream::pair().expect("unix stream pair");
        let payload = serde_json::to_vec(req).expect("encode request frame");
        let handle = tokio::spawn(async move {
            handle_conn(server, dispatcher).await;
        });
        write_frame(&mut client, &payload)
            .await
            .expect("write request frame");
        let raw = read_frame(&mut client).await.expect("read response frame");
        handle.await.expect("handle_conn task panicked");
        serde_json::from_slice(&raw).expect("decode response frame")
    }

    /// Test 1: a `metrics_only: true` request
    /// returns `metrics: Some(_)` and never reaches the ops-dispatch path; a
    /// normal request (the default `metrics_only: false`) still dispatches
    /// exactly as before and carries no metrics. Also proves `metrics_only`
    /// bypasses the `config_id` equality reject (a gauge read is
    /// process-global, not namespaced to a particular client config).
    #[tokio::test]
    async fn metrics_only_frame_returns_snapshot_without_dispatching() {
        let dispatch_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-a".to_string(),
            dispatch_calls: Arc::clone(&dispatch_calls),
            pool: None,
        };

        let mut metrics_req = base_request_frame("cfg-a");
        metrics_req.metrics_only = true;
        let metrics_resp = round_trip(dispatcher.clone(), &metrics_req).await;

        assert!(metrics_resp.ok, "metrics_only response must be ok=true");
        assert!(
            metrics_resp.metrics.is_some(),
            "metrics_only=true must return Some(snapshot)"
        );
        assert_eq!(
            dispatch_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "metrics_only must never reach the ops-dispatch path"
        );

        // metrics_only bypasses the config_id equality reject.
        let mut mismatched_req = base_request_frame("some-other-config");
        mismatched_req.metrics_only = true;
        let mismatched_resp = round_trip(dispatcher.clone(), &mismatched_req).await;
        assert!(mismatched_resp.ok);
        assert!(mismatched_resp.metrics.is_some());
        assert!(!mismatched_resp.config_mismatch);
        assert_eq!(
            dispatch_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a mismatched-config metrics_only request must still skip dispatch"
        );

        // A normal request (default metrics_only=false) is unaffected: it
        // still dispatches and carries no metrics.
        let normal_req = base_request_frame("cfg-a");
        let normal_resp = round_trip(dispatcher, &normal_req).await;
        assert!(normal_resp.ok);
        assert!(normal_resp.metrics.is_none());
        assert_eq!(dispatch_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Test 2: `wal_pages` reflects a real
    /// checkpoint observation after writes, deterministically forced via a
    /// direct `checkpoint_once` call rather than waiting on the async
    /// periodic task.
    #[tokio::test]
    async fn metrics_snapshot_wal_pages_reflects_recent_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("metrics_wal_test.db");
        let pool = Arc::new(
            ConnectionPool::new(khive_db::PoolConfig {
                path: Some(path),
                ..khive_db::PoolConfig::default()
            })
            .expect("pool open"),
        );

        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE t (x INTEGER); \
                     INSERT INTO t VALUES (1); \
                     INSERT INTO t VALUES (2);",
                )
                .expect("seed writes");
        }

        let tick = khive_db::checkpoint_once(
            &pool,
            &CheckpointConfig::default(),
            &mut khive_db::checkpoint::TruncateState::default(),
        );
        assert!(
            matches!(tick, khive_db::CheckpointTick::Observed(_)),
            "checkpoint_once on a freshly-writer-held pool must observe, not skip: {tick:?}"
        );

        let dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-wal".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: Some(pool),
        };

        let snapshot = build_metrics_snapshot(&dispatcher);
        assert!(
            snapshot.wal_pages.is_some(),
            "wal_pages must be observed after a real checkpoint tick, got {snapshot:?}"
        );
    }

    /// Test 3: the tx-pin oracle. Uses a
    /// before/after delta rather than asserting a global `open_tx_count==0`
    /// baseline — `tx_registry` is a process-wide singleton shared with every
    /// other test in this binary (including write-path tests elsewhere in
    /// this crate that register short-lived entries), the same reason
    /// `track_background_task_count_returns_to_zero_after_completion` above
    /// asserts a before/after delta on `background_task_count()` rather than
    /// an absolute value.
    #[tokio::test]
    #[serial(tx_registry)]
    async fn metrics_snapshot_reflects_open_transaction_registry() {
        let dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-tx".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: None,
        };

        let before = build_metrics_snapshot(&dispatcher).open_tx_count;

        let handle = khive_storage::tx_registry::register(Some("metrics_test_tx".to_string()));
        let during = build_metrics_snapshot(&dispatcher);
        assert!(
            during.open_tx_count > before,
            "open_tx_count must reflect the freshly registered transaction: \
             before={before} during={}",
            during.open_tx_count
        );
        assert!(
            during.oldest_pinned_tx_micros.is_some(),
            "oldest_pinned_tx_micros must be Some while a transaction is open"
        );

        drop(handle);

        let mut after = during.open_tx_count;
        for _ in 0..20 {
            after = build_metrics_snapshot(&dispatcher).open_tx_count;
            if after <= before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            after <= before,
            "open_tx_count must drop back down after the transaction handle is dropped: \
             before={before} after={after}"
        );
    }

    /// Test 4: write-queue depth is flag-gated
    /// on `PoolConfig::write_queue_enabled` (the `KHIVE_WRITE_QUEUE=1`
    /// setting), never on a specific depth value (racy under concurrency).
    #[tokio::test]
    async fn metrics_snapshot_write_queue_depth_flag_gated() {
        let dir = tempfile::tempdir().expect("tempdir");

        let enabled_pool = Arc::new(
            ConnectionPool::new(khive_db::PoolConfig {
                path: Some(dir.path().join("wq_enabled.db")),
                write_queue_enabled: true,
                ..khive_db::PoolConfig::default()
            })
            .expect("pool open"),
        );
        let enabled_dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-wq-on".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: Some(enabled_pool),
        };
        let snapshot_on = build_metrics_snapshot(&enabled_dispatcher);
        assert!(
            snapshot_on.write_queue_depth.is_some(),
            "write_queue_depth must be Some when write_queue_enabled=true, got {snapshot_on:?}"
        );
        assert!(snapshot_on.write_queue_capacity.is_some());

        let disabled_pool = Arc::new(
            ConnectionPool::new(khive_db::PoolConfig {
                path: Some(dir.path().join("wq_disabled.db")),
                write_queue_enabled: false,
                ..khive_db::PoolConfig::default()
            })
            .expect("pool open"),
        );
        let disabled_dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-wq-off".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: Some(disabled_pool),
        };
        let snapshot_off = build_metrics_snapshot(&disabled_dispatcher);
        assert!(
            snapshot_off.write_queue_depth.is_none(),
            "write_queue_depth must be None when write_queue_enabled=false, got {snapshot_off:?}"
        );
        assert!(snapshot_off.write_queue_capacity.is_none());

        // No pool at all (in-memory/poolless dispatcher): also None.
        let no_pool_dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-no-pool".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: None,
        };
        let snapshot_no_pool = build_metrics_snapshot(&no_pool_dispatcher);
        assert!(snapshot_no_pool.write_queue_depth.is_none());
        assert!(snapshot_no_pool.write_queue_capacity.is_none());
    }

    /// Test 5: serde default back-compat in
    /// both directions — a request JSON without `metrics_only` deserializes
    /// with it `false`, and a response JSON without `metrics` (an old
    /// daemon's shape) deserializes with it `None`.
    #[test]
    fn frame_serde_defaults_metrics_fields_when_absent() {
        let req_json = serde_json::json!({
            "ops": "",
            "presentation": null,
            "presentation_per_op": null,
            "namespace": "local",
            "actor_id": null,
            "visible_namespaces": [],
            "config_id": "cfg",
            "protocol_version": PROTOCOL_VERSION,
            "probe_only": false,
            "format": null,
            "format_per_op": null,
            "from_wire": false
        });
        let frame: DaemonRequestFrame =
            serde_json::from_value(req_json).expect("decode a metrics_only-absent request frame");
        assert!(
            !frame.metrics_only,
            "metrics_only must default to false when absent from the wire payload"
        );

        let resp_json = serde_json::json!({
            "ok": true,
            "result": null,
            "error": null,
            "namespace_mismatch": false,
            "config_mismatch": false,
            "served_config_id": "cfg",
            "version_mismatch": false,
            "daemon_protocol_version": PROTOCOL_VERSION
        });
        let resp: DaemonResponseFrame =
            serde_json::from_value(resp_json).expect("decode a metrics-absent response frame");
        assert!(
            resp.metrics.is_none(),
            "metrics must default to None when absent from the wire payload"
        );
    }
}
