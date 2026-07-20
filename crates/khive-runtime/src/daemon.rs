//! khived daemon server — persistent warm runtime over a Unix socket.
//!
//! The daemon binds `~/.khive/khived.sock`, accepts length-prefixed request
//! frames, dispatches them through a [`DaemonDispatch`] implementor, and serves
//! results back. It is transport-agnostic: the MCP crate provides the dispatch
//! impl, but any future client (CLI, HTTP gateway) can reuse this server.
//!
//! The client side (forwarding, auto-spawn) lives in the transport crate
//! (e.g. `khive-mcp`), not here.

use std::sync::Arc;

#[cfg(unix)]
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use libc;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(unix)]
use khive_db::{run_checkpoint_task, CheckpointConfig, ConnectionPool};

#[cfg(unix)]
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
/// See `docs/api/daemon.md#protocol_version` for the version-by-version history.
pub const PROTOCOL_VERSION: u32 = 3;

#[cfg(unix)]
const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 10;

// ── paths ─────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn khive_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".khive")
}

/// Unix socket path the daemon binds and clients connect to.
///
/// Overridable via the `KHIVE_SOCKET` env var (for tests and ops).
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
pub fn lock_path() -> PathBuf {
    if let Ok(p) = std::env::var("KHIVE_LOCK") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    khive_dir().join("khived.recovery.lock")
}

/// Advisory lock file used to serialize RECOVERY (kill+respawn) attempts
/// across concurrent clients only — the daemon's own boot sequence never
/// acquires this file ([`lock_path`] / [`acquire_daemon_boot_guard`] is the
/// boot-side lock). A recoverer holding this lock across dead-confirmation
/// → kill → spawn (khive-mcp's `kill_and_respawn`) therefore can never
/// deadlock against a peer daemon's boot, unlike holding the shared boot
/// lock for that whole span would.
///
/// Overridable via the `KHIVE_RECOVERER_LOCK` env var (for tests).
#[cfg(unix)]
pub fn recoverer_lock_path() -> PathBuf {
    if let Ok(p) = std::env::var("KHIVE_RECOVERER_LOCK") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    khive_dir().join("khived.recoverer.lock")
}

#[cfg(unix)]
fn open_lock_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

#[cfg(unix)]
fn acquire_flock_blocking(path: &std::path::Path, label: &str) -> Option<std::fs::File> {
    let file = match open_lock_file(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, path = ?path, "cannot open {label} lock file");
            return None;
        }
    };
    // SAFETY: flock is a POSIX advisory lock with no memory side-effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        tracing::warn!("flock LOCK_EX failed on {label} lock");
        return None;
    }
    Some(file)
}

/// Acquire an exclusive advisory flock on the recovery/startup lock file.
///
/// The returned `File` holds the lock for its lifetime; dropping it releases
/// it.  Used by both the client (serializing kill+spawn) and the daemon server
/// (serializing cleanup+bind+pid-write) so the two critical sections are
/// mutually exclusive across processes.
#[cfg(unix)]
pub fn acquire_recovery_lock() -> Option<std::fs::File> {
    acquire_flock_blocking(&lock_path(), "recovery")
}

/// Attempt to acquire an exclusive advisory flock on `path`, retrying with a
/// non-blocking `flock(LOCK_NB)` until `deadline` elapses. Bounded alternative
/// to `acquire_recovery_lock`/`acquire_daemon_boot_guard`'s unbounded blocking
/// flock — see `docs/api/daemon.md#try_acquire_flock_until` for why a caller
/// merely detecting lock freedom needs a deadline instead.
///
/// - `Ok(Some(file))` — the lock was free within the deadline.
/// - `Ok(None)` — `deadline` elapsed while the lock stayed held; an explicit
///   "could not confirm" outcome, distinct from a hard I/O error.
/// - `Err(_)` — the lock file could not be opened, or `flock` failed for a
///   reason other than contention.
///
/// Blocking (paces retries with `std::thread::sleep`) — async callers must
/// run this via `spawn_blocking`.
#[cfg(unix)]
fn try_acquire_flock_until(
    path: &std::path::Path,
    deadline: std::time::Instant,
) -> std::io::Result<Option<std::fs::File>> {
    let file = open_lock_file(path)?;
    let poll_interval = std::time::Duration::from_millis(10);
    loop {
        // SAFETY: flock is a POSIX advisory lock with no memory side-effects.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(file));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(err);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(poll_interval.min(deadline - now));
    }
}

/// Bounded, deadline-aware variant of [`acquire_daemon_boot_guard`]: attempts
/// the SAME boot/recovery lock ([`lock_path`]) but gives up at `deadline`
/// instead of blocking forever. For callers that need to detect "is a boot in
/// progress right now" without risking an unbounded wait behind a wedged
/// holder: e.g. khive-mcp's `confirm_genuinely_dead` re-probing rounds,
/// where `DEAD_CONFIRM_ROUNDS` must bound elapsed time, not just probe count.
#[cfg(unix)]
pub fn try_acquire_daemon_boot_guard_until(
    deadline: std::time::Instant,
) -> std::io::Result<Option<DaemonBootGuard>> {
    try_acquire_flock_until(&lock_path(), deadline)
}

/// Bounded, deadline-aware acquisition of the recoverer-only lock
/// ([`recoverer_lock_path`]). See [`try_acquire_daemon_boot_guard_until`] for
/// the shared rationale — a second recoverer waiting for a peer's dead
/// confirmation/kill/spawn critical section must give up and report
/// "uncertain" rather than block forever if that peer is itself wedged.
#[cfg(unix)]
pub fn try_acquire_recoverer_lock_until(
    deadline: std::time::Instant,
) -> std::io::Result<Option<std::fs::File>> {
    try_acquire_flock_until(&recoverer_lock_path(), deadline)
}

/// Guard returned by [`acquire_daemon_boot_guard`], held across cold-boot
/// schema initialization (migrations + pack schema plans / FTS DDL) through
/// daemon bind + pid-write.
#[cfg(unix)]
pub type DaemonBootGuard = std::fs::File;

/// Acquire the recovery/boot lock, treating failure as fatal.
///
/// Unlike [`acquire_recovery_lock`] (best-effort, `None` on failure: used by
/// shutdown cleanup, where skipping unlink is safer than blocking forever),
/// daemon-mode boot must hold this lock across migrations/FTS DDL through
/// bind+pid-write. Silently continuing with no lock reopens the cold-boot FTS
/// race this guard exists to close, so callers that are about to run
/// daemon-mode boot (or wait for one to quiesce) must fail loudly instead of
/// proceeding unguarded.
#[cfg(unix)]
pub fn acquire_daemon_boot_guard() -> anyhow::Result<DaemonBootGuard> {
    acquire_recovery_lock()
        .ok_or_else(|| anyhow::anyhow!("failed to acquire daemon boot/recovery lock"))
}

/// Identity of a bound Unix socket path, used to tell "the socket I bound" apart
/// from "a same-path socket some other daemon bound after mine was removed".
///
/// A socket path can be recreated by a different process between the time
/// this daemon captures its identity and the time it later checks it, so `dev`
/// and `ino` (not the path) are what must match for cleanup to be safe.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
fn socket_identity(path: &std::path::Path) -> Option<SocketIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(SocketIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
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
    /// As of protocol version 3 (ADR-096) the daemon serves the request under
    /// this namespace instead of rejecting on mismatch: a per-request
    /// identity input, not a same-process-identity assertion.
    pub namespace: String,
    /// The client's resolved write-stamp / gate actor identity (ADR-057),
    /// carried on the frame so the warm daemon stamps writes with the
    /// *caller's* actor instead of its own baked `actor_id` (ADR-096). `None`
    /// mints `ActorRef::anonymous()`, matching an unconfigured actor.
    #[serde(default)]
    pub actor_id: Option<String>,
    /// The client's resolved extra read-visibility namespaces (ADR-007 Rule
    /// 3b), carried on the frame so the warm daemon widens read scope to
    /// match the caller's own configuration rather than its own baked
    /// `visible_namespaces` (ADR-096). Empty means no extra visibility beyond
    /// `namespace` itself.
    #[serde(default)]
    pub visible_namespaces: Vec<String>,
    /// Fingerprint of the client's engine-coherence config: packs, db target,
    /// embedders, backend routing, and construction-baked outbound policy.
    /// Identity fields are carried separately in this frame. The daemon rejects
    /// a request whose `config_id` differs from its own so a restricted client
    /// (e.g. `--pack kg`, `--db :memory:`) never dispatches through the broader
    /// default daemon. See ADR-027 / ADR-049 / ADR-096.
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
    /// tool (the wire surface). When `true`, the daemon rejects
    /// `Visibility::Subhandler` verbs: agents must not invoke internal
    /// subhandlers. When `false` (the default, and the only value any
    /// operator path sends), subhandlers are allowed: `kkernel exec` and
    /// other in-process callers are trusted operator surfaces.
    ///
    /// This is the origin discriminator, not a daemon-vs-local one: operator
    /// requests flow through the daemon by default too, so the gate cannot
    /// key on transport.
    #[serde(default)]
    pub from_wire: bool,
    /// Caller-supplied correlation id (khive#948): a `u64` from the caller's
    /// own process-local monotonic counter, echoed back unchanged on
    /// [`DaemonResponseFrame::request_id`] and stamped into the dispatch's
    /// audit event (`resource.request_id`) so a benchmark harness can join
    /// its own pre-send sample to the server-side audit row for the same
    /// request. Purely additive — `#[serde(default)]` matches
    /// `metrics_only`/`format`/`format_per_op` precedent, no
    /// `PROTOCOL_VERSION` bump. `None` means the caller supplied no id.
    #[serde(default)]
    pub request_id: Option<u64>,
}

/// Response frame sent from the daemon back to a client.
#[derive(Serialize, Deserialize, Debug)]
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
    /// Echo of the request's `request_id` (khive#948), present whenever the
    /// frame that produced this response carried one — including on every
    /// error/denied arm, not only success, so a client can join a failure
    /// the same way it joins a success. `#[serde(default)]` so an older
    /// daemon's response (predating this field) deserializes to `None`
    /// rather than a parse error.
    #[serde(default)]
    pub request_id: Option<u64>,
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
    /// Total checkpoint ticks skipped because the writer mutex was busy
    /// (ADR-091 checkpoint-pressure telemetry), across this process's
    /// lifetime. `#[serde(default)]` so an older client decoding a newer
    /// daemon's snapshot (or vice versa) does not fail.
    #[serde(default)]
    pub wal_checkpoint_skipped_ticks: u64,
    /// Current consecutive-skip run length; 0 once the next tick is observed.
    #[serde(default)]
    pub wal_checkpoint_consecutive_skips: u64,
    /// WAL page count last known at the time of the most recent skip, if any
    /// skip has occurred yet in this process.
    #[serde(default)]
    pub wal_checkpoint_last_skip_wal_pages: Option<u64>,
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
    /// (ADR-096): `Some(..)` when serving a request forwarded over the
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

    /// File-backed backend pools beyond [`Self::pool_for_checkpoint`]'s pool
    /// (ADR-091 Amendment 3): one checkpoint task is spawned per entry here,
    /// in addition to the one spawned for the primary pool, so a
    /// multi-backend deployment gets PASSIVE/TRUNCATE checkpointing and
    /// sidecar enumeration on every file-backed backend it wired, not only
    /// the main one.
    ///
    /// The default implementation returns an empty `Vec` — an implementor
    /// with only one backend (or none) needs no override.
    fn secondary_pools_for_checkpoint(&self) -> Vec<Arc<ConnectionPool>> {
        Vec::new()
    }

    /// Return the audit `EventStore` the checkpoint task should append
    /// ADR-094 lifecycle events (`CheckpointOutcomeRecorded`) to, if any.
    ///
    /// Mirrors [`Self::pool_for_checkpoint`]'s default-`None` shape: an
    /// implementor with no configured event store (or no pool at all) simply
    /// gets a checkpoint task that never appends events — the checkpoint
    /// task itself remains fully functional either way.
    ///
    /// The default implementation returns `None`.
    fn event_store_for_checkpoint(&self) -> Option<Arc<dyn khive_storage::EventStore>> {
        None
    }
}

// ── tracked background tasks ─────────────────────────────────────────────────
//
// Pack handlers (e.g. memory.recall's ADR-081 serve-ledger append) fire
// fire-and-forget `tokio::spawn`ed work off the response path so the caller
// never waits on a cross-pack dispatch or a SQL write. Left untracked, that
// work is invisible to `drain()`: a SIGTERM landing between the response
// returning and the spawned task completing can abort it mid-flight with no
// log and no row. `track_background_task` gives such spawns a process-wide
// presence that `drain()` waits on, exactly like the `active` counter does
// for in-flight connections: the caller still only pays for the spawn +
// counter increment, never the task's own work.
static BACKGROUND_TASKS: std::sync::OnceLock<Arc<std::sync::atomic::AtomicUsize>> =
    std::sync::OnceLock::new();

fn background_tasks() -> &'static Arc<std::sync::atomic::AtomicUsize> {
    BACKGROUND_TASKS.get_or_init(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
}

/// Decrements the shared background-task counter from `Drop`, so the count
/// comes back down whether the tracked future returns normally, panics, or
/// is cancelled — a plain post-`await` `fetch_sub` only covers the return
/// path and leaks the count forever on a panic, since unwinding skips every
/// statement after the panic point.
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

// ── active background phase names (ADR-103) ──────────────────────────────────
//
// A lightweight, best-effort process-wide gauge of which named background
// phases (e.g. `ann_warm`) are in flight right now, read by `comm.health`'s
// resource self-report so a caller can see "what is the daemon doing" at a
// glance without correlating timestamps across the event log itself. Counted
// per name rather than boolean, since more than one occurrence of the same
// named phase can legitimately overlap (e.g. two embedding models warming
// concurrently) — the name only drops out of the reported set once every
// concurrent occurrence has ended.
static ACTIVE_PHASES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::OnceLock::new();

fn active_phases() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    ACTIVE_PHASES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// RAII guard for one occurrence of a named background phase. Increments the
/// phase's count on creation (see [`register_active_phase`]); decrements on
/// `Drop`, so the count comes back down whether the guarded work returns
/// normally, panics, or is cancelled — the same rationale as
/// `BackgroundTaskGuard` above.
pub struct PhaseGuard {
    name: String,
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let mut map = active_phases()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = map.get_mut(&self.name) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.name);
            }
        }
    }
}

/// Register one occurrence of a named background phase as currently active.
/// Returns a guard: drop it (or let it fall out of scope) when the phase
/// ends. Best-effort process-wide gauge only, read by `comm.health` — never
/// load-bearing for correctness.
pub fn register_active_phase(name: &str) -> PhaseGuard {
    let mut map = active_phases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *map.entry(name.to_string()).or_insert(0) += 1;
    PhaseGuard {
        name: name.to_string(),
    }
}

/// Currently active background-phase names, sorted for deterministic output.
/// Empty when no tracked phase is in flight.
pub fn active_phase_names() -> Vec<String> {
    let map = active_phases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
}

// ── server ────────────────────────────────────────────────────────────────────

/// Build a point-in-time [`MetricsSnapshot`] of this process's server-side
/// gauges. Called only from `handle_conn`'s `metrics_only` arm — a
/// process-global, read-only assembly with no side effects of its own.
/// See `docs/api/daemon.md#build_metrics_snapshot` for where each gauge is sourced
/// from and why.
#[cfg(unix)]
fn build_metrics_snapshot<D: DaemonDispatch>(dispatcher: &D) -> MetricsSnapshot {
    let open_tx_count = khive_storage::tx_registry::snapshot().len();
    let (oldest_pinned_tx_micros, oldest_pinned_tx_label) =
        match khive_storage::tx_registry::oldest() {
            Some((_id, age, label)) => (Some(age.as_micros() as u64), label),
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
        wal_checkpoint_skipped_ticks: khive_db::checkpoint::checkpoint_skipped_ticks(),
        wal_checkpoint_consecutive_skips: khive_db::checkpoint::checkpoint_consecutive_skips(),
        wal_checkpoint_last_skip_wal_pages: khive_db::checkpoint::checkpoint_last_skip_wal_pages(),
        oldest_pinned_tx_micros,
        oldest_pinned_tx_label,
        open_tx_count,
        write_queue_depth,
        write_queue_capacity,
    }
}

#[cfg(unix)]
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
            request_id: frame.request_id,
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
            request_id: frame.request_id,
        }
    // There is no `frame.namespace != dispatcher.namespace()` reject here.
    // The daemon accepts and serves the request under the frame's own
    // identity (namespace / actor / visible_namespaces, built into a
    // `RequestIdentity` below) over its one shared warm registry, rather
    // than rejecting a differently-attributed same-uid connection to a cold
    // local-dispatch fallback. `config_id`: which governs packs/db/embed
    // coherence for the shared warm engine: remains a hard reject; it is
    // not an identity field and softening it would let a restricted client
    // dispatch through an incompatible broader daemon.
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
            request_id: frame.request_id,
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
            request_id: frame.request_id,
        }
    } else {
        // Build the per-request identity context from the frame so the
        // implementor mints the storage/gate token from the CALLER's
        // identity, not the dispatcher's own construction-baked scalars.
        // This is always `Some` here: every frame that reaches this arm
        // carries a `namespace` (required on the wire) plus whatever
        // `actor_id`/`visible_namespaces` the client resolved (defaulting to
        // `None`/`vec![]` for an older, field-absent payload, which is
        // exactly the prior anonymous/no-extra-visibility behavior).
        let identity = RequestIdentity {
            namespace: frame.namespace.clone(),
            actor_id: frame.actor_id.clone(),
            visible_namespaces: frame.visible_namespaces.clone(),
            request_id: frame.request_id,
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
                request_id: frame.request_id,
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
                request_id: frame.request_id,
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
                    request_id: resp.request_id,
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
///
/// Fatally acquires its own startup lock, which only protects
/// cleanup→bind→pid-write — `dispatcher` has already run migrations and
/// applied pack schema plans while constructing itself, unguarded. Production
/// boot must go through [`run_daemon_with_boot_guard`] instead, which extends
/// the same lock back over construction. This entry point is for callers
/// (and tests) that build the dispatcher and start serving as one atomic
/// step with no separate boot-guard window to protect.
#[cfg(unix)]
pub async fn run_daemon<D: DaemonDispatch>(dispatcher: D) -> anyhow::Result<()> {
    let boot_guard = Some(acquire_daemon_boot_guard()?);
    run_daemon_with_boot_guard(dispatcher, boot_guard).await
}

/// Run the daemon using a startup lock acquired by the caller *before*
/// building `dispatcher`, so a second process racing to boot (e.g. two
/// `kkernel mcp --daemon` spawns before either has bound its socket) cannot
/// run migrations/FTS DDL concurrently against the same database file.
/// `boot_guard` is only `None` on non-unix targets, where there is no
/// advisory boot lock to hold in the first place; every unix daemon-mode
/// caller passes `Some`.
///
/// The guard is held across cleanup → bind → pid-write, then dropped. The
/// caller must not still be holding a *different* handle to the same lock
/// file when this function is entered — see the "Deadlock note" on the
/// `_startup_lock` binding below for why that would self-deadlock on `flock`.
#[cfg(unix)]
pub async fn run_daemon_with_boot_guard<D: DaemonDispatch>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
) -> anyhow::Result<()> {
    let sock = socket_path();
    let pid_file = pid_path();

    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
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
    // The daemon holds exactly one handle to this lock for its whole boot
    // sequence (received as `boot_guard`, extended from before `dispatcher`
    // was constructed) — never a second, independently-acquired handle in the
    // same process, which would self-deadlock on `flock`.
    let _startup_lock = boot_guard;

    if !cleanup_stale_daemon(&sock, &pid_file).await {
        tracing::info!("a responsive khived is already running; exiting");
        return Ok(());
    }

    let listener = UnixListener::bind(&sock)?;
    if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, path = ?sock, "failed to chmod 0600 socket");
    }
    // Captured while still holding the startup lock, immediately after
    // bind, so shutdown cleanup can later prove "this is still the same socket
    // I bound" rather than trusting the path alone.
    let bound_identity = socket_identity(&sock);

    if let Err(e) = write_pid_file_exclusive(&pid_file) {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            // A PID file appeared between our own `cleanup_stale_daemon`
            // removing it and this write — only possible if the boot lock did
            // not actually exclude a concurrent booter (e.g. `acquire_recovery_lock`
            // failed for one side). Never touch the winner's files: drop only
            // the socket entry we ourselves just bound (proven via identity,
            // not path), then decide by checking whether the PID now on disk
            // names a live, reachable daemon.
            if bound_identity.is_some() && socket_identity(&sock) == bound_identity {
                drop(listener);
                let _ = std::fs::remove_file(&sock);
            }
            if pid_file_names_a_reachable_daemon(&pid_file, &sock).await {
                tracing::info!(
                    "a replacement khived already claimed the pid/socket rendezvous; exiting"
                );
                return Ok(());
            }
            anyhow::bail!(
                "failed to claim daemon pid file at {pid_file:?}: it already exists \
                 and does not name a reachable daemon"
            );
        }
        return Err(e.into());
    }
    // Release the startup lock now: the listener is bound and the PID file is
    // written.  Any concurrent client or daemon startup will observe a live
    // socket+pid and take the non-recovery path.
    drop(_startup_lock);
    tracing::info!(socket = ?sock, pid = std::process::id(), "khived listening");

    {
        let warm = dispatcher.clone();
        tokio::spawn(async move {
            warm.warm_all().await;
        });
    }

    // The checkpoint task's own strong-count-based exit is unreachable
    // whenever `event_store_for_checkpoint()` returns `Some` (the ordinary
    // production shape), because the `SqlEventStore` it wraps retains its
    // own clone of the same pool. An explicit watch channel replaces that
    // mechanism: the sender is held for the remainder of this function's
    // scope and signalled as the first action once shutdown is observed,
    // below.
    let (checkpoint_shutdown_tx, checkpoint_shutdown_rx) = tokio::sync::watch::channel(());
    // ADR-091 Amendment 3: one checkpoint task per file-backed backend the
    // dispatcher wired — the primary pool plus every entry
    // `secondary_pools_for_checkpoint` returns — sharing this one shutdown
    // channel (the sender broadcasts to every receiver clone), so the single
    // send below stops every spawned task before `drain()`.
    let mut checkpoint_pools: Vec<(Arc<ConnectionPool>, bool)> = Vec::new();
    if let Some(pool) = dispatcher.pool_for_checkpoint() {
        checkpoint_pools.push((pool, true));
    }
    for pool in dispatcher.secondary_pools_for_checkpoint() {
        checkpoint_pools.push((pool, false));
    }
    if !checkpoint_pools.is_empty() {
        let cfg = CheckpointConfig::from_env();
        let event_store = dispatcher.event_store_for_checkpoint();
        let namespace = dispatcher.namespace().to_string();
        let checkpoint_task_count = checkpoint_pools.len();
        for (pool, is_main) in checkpoint_pools {
            track_background_task(run_checkpoint_task(
                pool,
                cfg.clone(),
                event_store.clone(),
                namespace.clone(),
                checkpoint_shutdown_rx.clone(),
                is_main,
            ));
        }
        tracing::info!(checkpoint_task_count, "WAL checkpoint task(s) started");
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

    // Signal the checkpoint task to exit before draining, so `drain()`
    // actually waits on it via `track_background_task` rather than the
    // task outliving the drain window (or the process) unsignalled.
    let _ = checkpoint_shutdown_tx.send(());

    drain(&active).await;

    // A concurrent client's `kill_and_respawn` may have already decided
    // this daemon looked stale, killed it, and spawned a replacement that
    // bound the same socket/PID paths while this daemon was draining above.
    // Reacquire the recovery lock (the same one that serializes startup) and
    // only unlink if the PID file still names this process AND the socket at
    // `sock` is still the exact one this daemon bound — otherwise a
    // replacement daemon owns those paths now and unlinking would delete its
    // live socket/PID out from under it.
    match acquire_recovery_lock() {
        Some(_shutdown_lock) => {
            shutdown_cleanup_if_owned(&sock, &pid_file, bound_identity);
        }
        None => {
            tracing::warn!(
                "could not acquire recovery lock for shutdown cleanup; \
                 skipping unlink to avoid deleting a replacement daemon's paths"
            );
        }
    }
    tracing::info!("khived stopped");
    Ok(())
}

/// Remove `sock`/`pid_file` only if they still belong to this process: the PID
/// file must name `std::process::id()` AND the socket currently at `sock` must
/// still be the exact one identified by `bound_identity` (dev/ino, not path).
///
/// Returns `true` if cleanup ran, `false` if it was skipped because a
/// replacement daemon already owns those paths. The caller must hold
/// the recovery lock across this call — the same lock daemon startup holds
/// across cleanup+bind+pid-write — so no replacement can bind between this
/// function's checks and its unlinks.
#[cfg(unix)]
fn shutdown_cleanup_if_owned(
    sock: &std::path::Path,
    pid_file: &std::path::Path,
    bound_identity: Option<SocketIdentity>,
) -> bool {
    let pid_is_ours = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        == Some(std::process::id());
    let socket_is_ours = bound_identity.is_some() && socket_identity(sock) == bound_identity;
    if pid_is_ours && socket_is_ours {
        let _ = std::fs::remove_file(sock);
        let _ = std::fs::remove_file(pid_file);
        true
    } else {
        tracing::warn!(
            socket = ?sock,
            pid_file = ?pid_file,
            "skipping shutdown cleanup — a replacement daemon already owns this socket/PID"
        );
        false
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Liveness verdict for a `kill(pid, 0)` probe.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidLiveness {
    /// errno 0 — signal delivery succeeded, the process exists and this
    /// caller may signal it.
    Alive,
    /// ESRCH (or any other non-EPERM errno) — no such process.
    Dead,
    /// EPERM — the process exists but this caller lacks permission to
    /// signal it. Unknown-safe: treated as running so stale-daemon cleanup
    /// never unlinks a live daemon's socket/PID file just because it is
    /// owned by a different user/uid.
    PermissionDenied,
}

#[cfg(unix)]
impl PidLiveness {
    fn is_running(self) -> bool {
        !matches!(self, PidLiveness::Dead)
    }
}

/// Maps a `kill(pid, 0)` outcome (return code + errno) to a [`PidLiveness`].
/// Pure and side-effect-free so the errno mapping can be unit tested without
/// a real process probe.
#[cfg(unix)]
fn classify_kill_result(rc: i32, errno: i32) -> PidLiveness {
    if rc == 0 {
        return PidLiveness::Alive;
    }
    match errno {
        libc::EPERM => PidLiveness::PermissionDenied,
        _ => PidLiveness::Dead,
    }
}

#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 is an existence/permission probe with no side effects.
    let rc = unsafe { libc::kill(pid, 0) };
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    classify_kill_result(rc, errno).is_running()
}

#[cfg(unix)]
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

/// Create `pid_file` exclusively (`O_EXCL`) and write this process's PID.
///
/// Uses `create_new(true)` rather than `create(true).truncate(true)` so
/// this can never silently overwrite a PID file another process created —
/// under normal operation the boot lock already serializes cleanup → bind →
/// pid-write across processes, but that guarantee depends on
/// `acquire_recovery_lock` succeeding for every party. Exclusive creation is
/// the defense that holds even if the lock itself is unavailable to one side:
/// the loser observes `ErrorKind::AlreadyExists` instead of clobbering the
/// winner's PID out from under it.
#[cfg(unix)]
fn write_pid_file_exclusive(pid_file: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    let mut f = opts.open(pid_file)?;
    f.write_all(std::process::id().to_string().as_bytes())?;
    Ok(())
}

/// Return `true` if `pid_file` currently names a different, live process that
/// still answers on `sock` — i.e. a daemon already owns this rendezvous and it
/// is safe to defer to it rather than treat the `AlreadyExists` PID-file
/// collision as a boot failure.
#[cfg(unix)]
async fn pid_file_names_a_reachable_daemon(
    pid_file: &std::path::Path,
    sock: &std::path::Path,
) -> bool {
    let Ok(pid_str) = std::fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        return false;
    };
    pid != std::process::id()
        && is_process_running(pid)
        && sock.exists()
        && UnixStream::connect(sock).await.is_ok()
}

#[cfg(unix)]
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

#[cfg(unix)]
fn drain_timeout() -> std::time::Duration {
    let secs = std::env::var("KHIVE_DRAIN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAIN_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Returns `true` for non-empty env values that are not `"0"` or `"false"`.
#[cfg(unix)]
pub fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

#[cfg(all(test, unix))]
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

    // EPERM (process exists, no permission to signal it) must not be
    // misread as "not running" during stale-daemon cleanup.

    #[test]
    fn classify_kill_result_zero_is_alive() {
        assert_eq!(classify_kill_result(0, 0), PidLiveness::Alive);
        assert!(classify_kill_result(0, 0).is_running());
    }

    #[test]
    fn classify_kill_result_esrch_is_dead() {
        assert_eq!(classify_kill_result(-1, libc::ESRCH), PidLiveness::Dead);
        assert!(!classify_kill_result(-1, libc::ESRCH).is_running());
    }

    #[test]
    fn classify_kill_result_eperm_is_permission_denied_and_counts_as_running() {
        assert_eq!(
            classify_kill_result(-1, libc::EPERM),
            PidLiveness::PermissionDenied
        );
        assert!(
            classify_kill_result(-1, libc::EPERM).is_running(),
            "EPERM must be unknown-safe: treated as running, never as a basis \
             for stale cleanup to unlink a live daemon's rendezvous files"
        );
    }

    #[test]
    fn pid_1_probe_is_running_regardless_of_permission_outcome() {
        // PID 1 (init/launchd) always exists. An unprivileged process gets
        // EPERM signaling it (never ESRCH); running as root would get 0
        // instead. Either way `is_process_running` must report true — this
        // is the live regression guard for EPERM being misread as dead;
        // `classify_kill_result` above is the pure-function unit coverage
        // for the same mapping, kept independent of process ownership so
        // it is never flaky in CI.
        assert!(
            is_process_running(1),
            "PID 1 always exists; EPERM must not read as dead"
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

    // `drain()` must wait for tracked background tasks (e.g. memory.recall's
    // serve-ledger append), not just in-flight connections, or a SIGTERM
    // lands mid-flight with no log and no row.
    //
    // `#[serial(background_tasks)]`: this test reads/asserts on the
    // process-wide `BACKGROUND_TASKS` static shared with the two counter
    // tests below. Under default parallel execution one test's increment
    // leaks into another's snapshot-then-assert window (reproduced: both
    // counter tests failed together, passed with `--test-threads=1`).
    // Serializing just this named group isolates them from each other
    // without forcing the whole test binary (including unrelated
    // `#[serial]` tests elsewhere in this crate) onto one thread.
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
        // A panic inside the tracked future must still decrement the
        // counter (via BackgroundTaskGuard's Drop), not leak it forever.
        // `track_background_task` discards the spawned
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

    // ── active background phase names (ADR-103) ──────────────────────────

    // `#[serial(active_phases)]`: these tests read/assert on the process-wide
    // `ACTIVE_PHASES` static. No other test in this crate touches it today,
    // but the group mirrors the `background_tasks` precedent above so a
    // future addition does not silently reintroduce the same interleaving
    // hazard that motivated it there.
    #[test]
    #[serial(active_phases)]
    fn register_active_phase_appears_and_disappears_with_the_guard() {
        assert!(
            !active_phase_names().contains(&"adr103_test_phase".to_string()),
            "must start absent (leaked from a prior failed run would poison this test)"
        );

        let guard = register_active_phase("adr103_test_phase");
        assert!(active_phase_names().contains(&"adr103_test_phase".to_string()));

        drop(guard);
        assert!(
            !active_phase_names().contains(&"adr103_test_phase".to_string()),
            "the phase name must drop out of the gauge once its guard is dropped"
        );
    }

    #[test]
    #[serial(active_phases)]
    fn register_active_phase_counts_concurrent_occurrences_of_the_same_name() {
        let first = register_active_phase("adr103_concurrent_phase");
        let second = register_active_phase("adr103_concurrent_phase");
        assert!(active_phase_names().contains(&"adr103_concurrent_phase".to_string()));

        drop(first);
        assert!(
            active_phase_names().contains(&"adr103_concurrent_phase".to_string()),
            "one of two concurrent occurrences ending must not remove the name early"
        );

        drop(second);
        assert!(
            !active_phase_names().contains(&"adr103_concurrent_phase".to_string()),
            "the name must be removed only once every concurrent occurrence has ended"
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
        /// When `Some(msg)`, `dispatch` returns `Err(msg)` instead of the
        /// default `Ok("{}")` — lets a test drive `handle_conn`'s real
        /// dispatch-error arm (khive#948 request_id echo coverage).
        dispatch_err: Option<String>,
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
            match &self.dispatch_err {
                Some(msg) => Err(msg.clone()),
                None => Ok("{}".to_string()),
            }
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
            request_id: None,
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
            dispatch_err: None,
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
            dispatch_err: None,
        };

        let snapshot = build_metrics_snapshot(&dispatcher);
        assert!(
            snapshot.wal_pages.is_some(),
            "wal_pages must be observed after a real checkpoint tick, got {snapshot:?}"
        );
        // The snapshot carries the checkpoint-pressure fields read-only
        // (no mutation path reachable through `MetricsSnapshot`/`DaemonRequestFrame`);
        // an observed tick (not a skip) must report a zero-length skip streak.
        assert_eq!(
            snapshot.wal_checkpoint_consecutive_skips, 0,
            "an observed (non-skipped) tick must report zero consecutive skips, got {snapshot:?}"
        );
    }

    /// Test 3: the tx-pin oracle. The registry is process-global, so an
    /// unrelated transaction can depart between snapshots and exactly offset
    /// this test's registration. Keep an owned handle live and assert the
    /// resulting count floor instead of comparing two points in time.
    #[test]
    #[serial(tx_registry)]
    fn metrics_snapshot_reflects_open_transaction_registry() {
        let dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-tx".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: None,
            dispatch_err: None,
        };

        let departing_handle = khive_storage::tx_registry::register(Some(
            "daemon_metrics_snapshot_departing_test_tx".to_string(),
        ));
        let before = build_metrics_snapshot(&dispatcher).open_tx_count;
        assert!(before >= 1);

        let handle = khive_storage::tx_registry::register(Some(
            "daemon_metrics_snapshot_owned_test_tx".to_string(),
        ));
        drop(departing_handle);

        let during = build_metrics_snapshot(&dispatcher);
        assert!(
            during.open_tx_count >= 1,
            "open_tx_count must reflect the live owned transaction despite registry churn: \
             churn_baseline={before} during={}",
            during.open_tx_count
        );
        assert!(
            during.oldest_pinned_tx_micros.is_some(),
            "oldest_pinned_tx_micros must be Some while a transaction is open"
        );

        drop(handle);
        assert!(
            !khive_storage::tx_registry::snapshot()
                .iter()
                .any(|(_, label)| label.as_deref()
                    == Some("daemon_metrics_snapshot_owned_test_tx")),
            "the owned registry entry must disappear when its handle is dropped"
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
            dispatch_err: None,
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
            dispatch_err: None,
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
            dispatch_err: None,
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
        assert_eq!(
            frame.request_id, None,
            "request_id must default to None when absent from the wire payload (khive#948)"
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
        assert_eq!(
            resp.request_id, None,
            "request_id must default to None when absent from the wire payload (khive#948)"
        );
    }

    /// khive#948: a request carrying `request_id: Some(n)` gets back a
    /// response with `request_id: Some(n)` on both the success and the
    /// error/denied dispatch arms — the echo must survive every branch of
    /// `handle_conn`, not only the happy path.
    #[tokio::test]
    async fn request_id_echoed_on_success_and_error_arms() {
        let dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-a".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: None,
            dispatch_err: None,
        };
        let mut ok_req = base_request_frame("cfg-a");
        ok_req.request_id = Some(42);
        let ok_resp = round_trip(dispatcher, &ok_req).await;
        assert!(ok_resp.ok, "expected successful dispatch: {ok_resp:?}");
        assert_eq!(
            ok_resp.request_id,
            Some(42),
            "request_id must be echoed back on a successful dispatch response"
        );

        // config_mismatch is a rejection arm that never reaches dispatch —
        // must still echo the id so the client can join the failure.
        let mismatched_dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-a".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: None,
            dispatch_err: None,
        };
        let mut mismatch_req = base_request_frame("cfg-WRONG");
        mismatch_req.request_id = Some(99);
        let mismatch_resp = round_trip(mismatched_dispatcher, &mismatch_req).await;
        assert!(mismatch_resp.config_mismatch);
        assert_eq!(
            mismatch_resp.request_id,
            Some(99),
            "request_id must be echoed on the config_mismatch rejection arm too"
        );

        // The real ops-dispatch error arm (`Err(e)` from `dispatcher.dispatch`)
        // must echo the id as well, not only the pre-dispatch rejection arms.
        let erroring_dispatcher = MockDispatch {
            namespace: "local".to_string(),
            config_id: "cfg-a".to_string(),
            dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pool: None,
            dispatch_err: Some("simulated dispatch error".to_string()),
        };
        let mut err_req = base_request_frame("cfg-a");
        err_req.request_id = Some(7);
        let err_resp = round_trip(erroring_dispatcher, &err_req).await;
        assert!(!err_resp.ok, "expected a dispatch error: {err_resp:?}");
        assert_eq!(
            err_resp.request_id,
            Some(7),
            "request_id must be echoed on the real ops-dispatch error arm"
        );
    }

    // ── owner-checked shutdown cleanup ────────────────────────────────────────
    //
    // A draining daemon must not unlink a socket/PID pair that a replacement
    // daemon has already bound. These tests exercise `shutdown_cleanup_if_owned`
    // directly (the pure decision the caller makes under the recovery lock)
    // rather than driving `run_daemon`'s real SIGTERM shutdown, which would
    // require sending a signal to the whole test process.

    #[test]
    fn shutdown_cleanup_removes_paths_it_still_owns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");

        let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind socket");
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
        let identity = socket_identity(&sock);
        assert!(
            identity.is_some(),
            "must read identity of a freshly bound socket"
        );

        let cleaned = shutdown_cleanup_if_owned(&sock, &pid_file, identity);

        assert!(
            cleaned,
            "cleanup must proceed when PID and socket still match"
        );
        assert!(!sock.exists(), "owned socket must be removed");
        assert!(!pid_file.exists(), "owned pid file must be removed");
    }

    #[test]
    fn shutdown_cleanup_skips_when_pid_file_names_a_different_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");

        let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind socket");
        let identity = socket_identity(&sock);
        // A concurrent client's kill_and_respawn already replaced the PID file
        // with a different (replacement daemon's) PID before this daemon's
        // drain completed.
        std::fs::write(&pid_file, "1").expect("write foreign pid file");

        let cleaned = shutdown_cleanup_if_owned(&sock, &pid_file, identity);

        assert!(
            !cleaned,
            "cleanup must be skipped when the PID file no longer names this process"
        );
        assert!(sock.exists(), "replacement daemon's socket must survive");
        assert!(
            pid_file.exists(),
            "replacement daemon's pid file must survive"
        );
    }

    #[test]
    fn shutdown_cleanup_skips_when_socket_was_rebound_by_a_replacement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let original_sock = dir.path().join("original.sock");
        let pid_file = dir.path().join("khived.pid");

        // Bind two sockets at DIFFERENT paths, both alive at the same time,
        // so the OS cannot recycle an inode between them the way it could
        // across a bind/drop/rebind cycle at a single path (the flakiness a
        // prior version of this test hit on some filesystems). Both
        // identities are captured through the real production
        // `socket_identity()` path, not a synthetic/sentinel value, so a
        // regression where `socket_identity()` returns a constant identity
        // for every socket makes the `assert!` below fail loudly instead of
        // silently passing.
        let _original_listener =
            std::os::unix::net::UnixListener::bind(&original_sock).expect("bind original socket");
        let _replacement_listener =
            std::os::unix::net::UnixListener::bind(&sock).expect("bind replacement socket");

        let original_identity = socket_identity(&original_sock);
        let replacement_identity = socket_identity(&sock);
        assert!(
            original_identity.is_some(),
            "must read identity of the original socket"
        );
        assert!(
            replacement_identity.is_some(),
            "must read identity of the replacement socket"
        );
        assert!(
            original_identity != replacement_identity,
            "two concurrently bound sockets must have distinct identities"
        );

        std::fs::write(&pid_file, std::process::id().to_string())
            .expect("write pid file matching this process");

        // `sock` (the replacement bind's path) is checked against
        // `original_identity` (a different, concurrently-alive socket's
        // identity) - the mismatch alone must be enough to block cleanup,
        // even though the pid file matches this process.
        let cleaned = shutdown_cleanup_if_owned(&sock, &pid_file, original_identity);

        assert!(
            !cleaned,
            "cleanup must be skipped when the socket at this path is a different \
             inode than the one this daemon originally bound"
        );
        assert!(sock.exists(), "replacement daemon's socket must survive");
        assert!(
            pid_file.exists(),
            "replacement daemon's pid file must survive"
        );
    }

    // ── the recovery lock actually serializes two boot sequences ─────────────
    //
    // Production wiring (`khive_mcp::serve::run` / `serve_server`) now acquires
    // this same lock *before* building a `KhiveMcpServer` (which runs
    // migrations and applies pack schema plans / FTS DDL) and holds it through
    // daemon bind+pid-write, via `run_daemon_with_boot_guard`. That closes the
    // cold-boot race only if `acquire_recovery_lock` genuinely provides mutual
    // exclusion across concurrent boot attempts — this test proves the
    // primitive itself: two "boot sequences" (each holding the lock across a
    // simulated schema-init critical section) must never run their critical
    // sections at the same time.
    #[test]
    #[serial]
    fn recovery_lock_serializes_two_concurrent_boot_sequences() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock_file = dir.path().join("khived.recovery.lock");
        std::env::set_var("KHIVE_LOCK", &lock_file);

        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let overlap_detected = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let run_one_boot =
            |active: Arc<std::sync::atomic::AtomicUsize>,
             overlap: Arc<std::sync::atomic::AtomicBool>| {
                move || {
                    let _guard = acquire_recovery_lock().expect("acquire recovery lock");
                    // Enter the "schema-init" critical section.
                    if active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) != 0 {
                        overlap.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    // `_guard` drops here, releasing the lock.
                }
            };

        let t1 = std::thread::spawn(run_one_boot(active.clone(), overlap_detected.clone()));
        let t2 = std::thread::spawn(run_one_boot(active.clone(), overlap_detected.clone()));
        t1.join().expect("boot thread 1 must not panic");
        t2.join().expect("boot thread 2 must not panic");

        assert!(
            !overlap_detected.load(std::sync::atomic::Ordering::SeqCst),
            "two concurrent boot sequences must never hold the schema-init \
             critical section at the same time (#667)"
        );

        std::env::remove_var("KHIVE_LOCK");
    }

    // ── acquire_daemon_boot_guard treats lock failure as fatal ───────────────
    // (unlike best-effort acquire_recovery_lock, whose `None` on failure is
    // correct for its own best-effort callers).

    #[test]
    #[serial]
    fn acquire_daemon_boot_guard_returns_guard_when_lock_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock_file = dir.path().join("khived.recovery.lock");
        std::env::set_var("KHIVE_LOCK", &lock_file);

        let guard = acquire_daemon_boot_guard();
        assert!(
            guard.is_ok(),
            "daemon boot guard must succeed when the lock file can be opened and flocked"
        );
        drop(guard);

        std::env::remove_var("KHIVE_LOCK");
    }

    #[test]
    #[serial]
    fn acquire_daemon_boot_guard_fails_loudly_when_lock_file_cannot_be_opened() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Point KHIVE_LOCK at a directory, not a file: opening a directory
        // with `write(true)` fails (EISDIR), so `acquire_recovery_lock`
        // returns `None` here — the exact failure mode
        // `acquire_daemon_boot_guard` must turn into a hard `Err` instead of
        // silently letting daemon-mode boot proceed unguarded.
        std::env::set_var("KHIVE_LOCK", dir.path());

        let result = acquire_daemon_boot_guard();
        assert!(
            result.is_err(),
            "daemon boot guard must fail loudly, never silently proceed unguarded, \
             when the underlying recovery lock cannot be acquired"
        );

        std::env::remove_var("KHIVE_LOCK");
    }

    // ── write_pid_file_exclusive never truncates a winner's pid file ────────

    #[test]
    fn write_pid_file_exclusive_creates_new_file_with_own_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("khived.pid");
        write_pid_file_exclusive(&pid_file).expect("first writer must win");
        let contents = std::fs::read_to_string(&pid_file).expect("read pid file");
        assert_eq!(contents, std::process::id().to_string());
    }

    #[test]
    fn write_pid_file_exclusive_refuses_to_overwrite_an_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("khived.pid");
        std::fs::write(&pid_file, "999999").expect("seed an existing pid file");

        let err = write_pid_file_exclusive(&pid_file)
            .expect_err("must not silently overwrite an existing pid file");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        // The existing content must be completely untouched — proving this is
        // `create_new`, not the old `create(true).truncate(true)`.
        let contents = std::fs::read_to_string(&pid_file).expect("read pid file");
        assert_eq!(
            contents, "999999",
            "an existing pid file must never be truncated by a losing writer"
        );
    }

    // Real (not simulated) concurrency: two OS threads race to `create_new`
    // the exact same path, synchronized with a `Barrier` so they genuinely
    // overlap at the syscall rather than relying on a sleep-based ordering
    // guess. This is the deterministic race oracle for the convergence
    // requirement the atomic-creation primitive `write_pid_file_exclusive`
    // is built on: exactly one of two simultaneous daemon starters may claim
    // the pid file, and the loser must see `AlreadyExists`, never silently
    // clobber the winner's content.
    #[test]
    fn two_concurrent_writers_converge_on_exactly_one_pid_file_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = std::sync::Arc::new(dir.path().join("khived.pid"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let spawn_writer =
            |pid_file: std::sync::Arc<std::path::PathBuf>,
             barrier: std::sync::Arc<std::sync::Barrier>| {
                std::thread::spawn(move || {
                    barrier.wait();
                    write_pid_file_exclusive(&pid_file)
                })
            };

        let t1 = spawn_writer(pid_file.clone(), barrier.clone());
        let t2 = spawn_writer(pid_file.clone(), barrier.clone());
        let r1 = t1.join().expect("writer 1 must not panic");
        let r2 = t2.join().expect("writer 2 must not panic");

        let results = [&r1, &r2];
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        let already_exists_count = results
            .iter()
            .filter(|r| matches!(r, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists))
            .count();
        assert_eq!(
            ok_count, 1,
            "exactly one of two concurrent writers must win the pid file"
        );
        assert_eq!(
            already_exists_count, 1,
            "the other writer must observe AlreadyExists, never a silent overwrite"
        );
        assert!(pid_file.exists(), "the winner's pid file must exist");
        let contents = std::fs::read_to_string(&*pid_file).expect("read pid file");
        assert_eq!(
            contents,
            std::process::id().to_string(),
            "the surviving pid file must contain the winner's pid — both threads \
             share this process's pid, so an unexpected value would also prove a \
             lost/garbled write raced through"
        );
    }
}
