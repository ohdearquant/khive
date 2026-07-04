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
pub const PROTOCOL_VERSION: u32 = 2;

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
    pub namespace: String,
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
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
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
// log and no row (codex PR #583 round-1 Medium). `track_background_task`
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
/// path and leaks the count forever on a panic (codex PR #583 round-2
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
        }
    } else if frame.namespace != dispatcher.namespace() {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: true,
            config_mismatch: false,
            served_config_id,
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
        }
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
        }
    } else {
        match dispatcher
            .dispatch(
                frame.ops,
                frame.presentation,
                frame.presentation_per_op,
                frame.format,
                frame.format_per_op,
                frame.from_wire,
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

    // codex PR #583 round-1 Medium: `drain()` must wait for tracked background
    // tasks (e.g. memory.recall's serve-ledger append), not just in-flight
    // connections, or a SIGTERM lands mid-flight with no log and no row.
    //
    // `#[serial(background_tasks)]`: this test reads/asserts on the
    // process-wide `BACKGROUND_TASKS` static shared with the two counter
    // tests below. Under default parallel execution one test's increment
    // leaks into another's snapshot-then-assert window (codex PR #583
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
        // codex PR #583 round-2 Medium: a panic inside the tracked future must
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
}
