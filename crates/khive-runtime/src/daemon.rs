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

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Maximum frame size accepted in either direction.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

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

// ── wire types ────────────────────────────────────────────────────────────────

/// Request frame sent from a client to the daemon.
#[derive(Serialize, Deserialize)]
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
/// The MCP crate implements this by wrapping `dispatch_request_local`; any
/// future transport can do the same.
#[async_trait]
pub trait DaemonDispatch: Clone + Send + Sync + 'static {
    /// Dispatch a verb-DSL request string and return the JSON result.
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
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

    let resp = if frame.namespace != dispatcher.namespace() {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: true,
            config_mismatch: false,
        }
    } else if frame.config_id != dispatcher.config_id() {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: true,
        }
    } else {
        match dispatcher
            .dispatch(frame.ops, frame.presentation, frame.presentation_per_op)
            .await
        {
            Ok(result) => DaemonResponseFrame {
                ok: true,
                result: Some(result),
                error: None,
                namespace_mismatch: false,
                config_mismatch: false,
            },
            Err(e) => DaemonResponseFrame {
                ok: false,
                result: None,
                error: Some(e),
                namespace_mismatch: false,
                config_mismatch: false,
            },
        }
    };

    match serde_json::to_vec(&resp) {
        Ok(payload) => {
            if let Err(e) = write_frame(&mut stream, &payload).await {
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
    tracing::info!(socket = ?sock, pid = std::process::id(), "khived listening");

    {
        let warm = dispatcher.clone();
        tokio::spawn(async move {
            warm.warm_all().await;
        });
    }

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let shutdown = async {
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
    if active.load(Ordering::Relaxed) == 0 {
        return;
    }
    let deadline = tokio::time::Instant::now() + drain_timeout();
    while active.load(Ordering::Relaxed) > 0 {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                remaining = active.load(Ordering::Relaxed),
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
}
