//! khived daemon client — forwarding + auto-spawn.
//!
//! The daemon server lives in `khive-runtime::daemon`. This module provides the
//! client side: [`forward_or_spawn`] connects to the daemon, auto-spawns it on
//! first use, and maps responses to MCP error types. Every failure path falls
//! back to `None` so the caller can dispatch locally.
//!
//! Also provides the [`khive_runtime::daemon::DaemonDispatch`] impl for [`crate::server::KhiveMcpServer`].

use std::process::Stdio;

use async_trait::async_trait;
use khive_runtime::daemon::{
    self, env_truthy, lock_path, pid_path, read_frame, socket_path, write_frame,
    DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
use rmcp::ErrorData as McpError;
use tokio::net::UnixStream;

use crate::tools::request::RequestParams;

// ── DaemonDispatch impl ───────────────────────────────────────────────────────

#[async_trait]
impl daemon::DaemonDispatch for crate::server::KhiveMcpServer {
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
    ) -> Result<String, String> {
        let params = RequestParams {
            ops,
            presentation,
            presentation_per_op,
        };
        self.dispatch_request_local(params)
            .await
            .map_err(|e| e.message.to_string())
    }

    async fn warm_all(&self) {
        crate::server::KhiveMcpServer::warm_all(self).await;
    }

    fn namespace(&self) -> &str {
        self.default_namespace()
    }

    fn config_id(&self) -> &str {
        crate::server::KhiveMcpServer::config_id(self)
    }
}

// ── client ────────────────────────────────────────────────────────────────────

/// Result of a single forward attempt to the daemon socket.
enum ForwardOutcome {
    /// Successfully received and decoded a response frame.
    Response(DaemonResponseFrame),
    /// Socket was unreachable (connection refused / no file).
    NoSocket,
    /// Connected but the response could not be decoded — most likely a stale
    /// daemon speaking a different wire format.
    ParseFailure,
    /// Connected and decoded a response, but the daemon's `daemon_protocol_version`
    /// does not match [`PROTOCOL_VERSION`] even though `version_mismatch` is false.
    /// This is the new-client + old-daemon (pre-versioning) scenario: the old daemon
    /// ignores the unknown request field and returns a decodable response whose
    /// protocol fields default to `false`/`0`. The client must treat this exactly
    /// like `ParseFailure`: kill the stale daemon, respawn once, and return a clear
    /// error if the replacement still has the wrong version.
    ProtocolMismatch,
}

async fn try_forward_inner(frame: &DaemonRequestFrame) -> ForwardOutcome {
    let sock = socket_path();
    let mut stream = match UnixStream::connect(&sock).await {
        Ok(s) => s,
        Err(_) => return ForwardOutcome::NoSocket,
    };
    let payload = match serde_json::to_vec(frame) {
        Ok(p) => p,
        Err(_) => return ForwardOutcome::NoSocket,
    };
    if write_frame(&mut stream, &payload).await.is_err() {
        return ForwardOutcome::NoSocket;
    }
    let resp = match read_frame(&mut stream).await {
        Ok(r) => r,
        Err(_) => return ForwardOutcome::NoSocket,
    };
    match serde_json::from_slice::<DaemonResponseFrame>(&resp) {
        Ok(frame) => {
            // A pre-versioning daemon (old-daemon scenario) returns a decodable
            // response whose `daemon_protocol_version` defaults to 0. The field
            // `version_mismatch` is also false because the old daemon never set
            // it — making `map_response` accept the stale response when
            // `served_config_id` happens to match. Detect this here so the
            // caller can route it through the same kill/respawn path.
            if frame.daemon_protocol_version != PROTOCOL_VERSION && !frame.version_mismatch {
                tracing::warn!(
                    daemon_version = frame.daemon_protocol_version,
                    expected = PROTOCOL_VERSION,
                    "daemon protocol version mismatch (old daemon, no version_mismatch flag) \
                     — treating as stale",
                );
                return ForwardOutcome::ProtocolMismatch;
            }
            ForwardOutcome::Response(frame)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                bytes = resp.len(),
                "daemon response could not be decoded — stale daemon binary on {}?",
                sock.display()
            );
            ForwardOutcome::ParseFailure
        }
    }
}

fn map_response(
    resp: DaemonResponseFrame,
    expected_config_id: &str,
) -> Option<Result<String, McpError>> {
    // Protocol version mismatch is a hard error — do NOT fall back to local
    // dispatch, which would hide the skew. Surface the daemon's own message.
    if resp.version_mismatch {
        let msg = resp.error.unwrap_or_else(|| {
            format!(
                "daemon protocol mismatch: client={} daemon={} — \
                 rebuild/update the client binary (make local)",
                PROTOCOL_VERSION, resp.daemon_protocol_version,
            )
        });
        return Some(Err(McpError::internal_error(msg, None)));
    }

    if resp.namespace_mismatch || resp.config_mismatch {
        return None;
    }
    // Fail closed: only trust a result the daemon positively confirms it served
    // under our exact config. A legacy daemon omits `served_config_id` (→ None)
    // and a config-drifted daemon echoes a different id — both fall back local.
    if resp.served_config_id.as_deref() != Some(expected_config_id) {
        return None;
    }
    if resp.ok {
        Some(Ok(resp.result.unwrap_or_default()))
    } else {
        let msg = resp.error.unwrap_or_else(|| {
            format!(
                "daemon returned an error without a message \
                 (code: internal_error; daemon config: {})",
                resp.served_config_id.as_deref().unwrap_or("unknown"),
            )
        });
        Some(Err(McpError::internal_error(msg, None)))
    }
}

fn spawn_daemon() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    // The binary is `kkernel`; the MCP server (and its daemon mode) live under
    // the `mcp` subcommand.
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("mcp")
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    Ok(())
}

/// Return `true` if `args` (the full `ps -o args=` output for a process)
/// identifies a khive daemon.
///
/// Both conditions must hold:
/// (a) the first whitespace-delimited token's file-name basename is exactly
///     `kkernel` (an absolute path like `/Users/x/.cargo/bin/kkernel` is
///     accepted; a basename of `not-kkernel` or a wrapper whose argv[0] merely
///     mentions kkernel elsewhere is rejected), AND
/// (b) the remaining tokens contain both `mcp` and `--daemon` as distinct
///     whitespace-separated tokens (matching the daemon spawn shape
///     `kkernel mcp --daemon`; a bare `kkernel exec '...'` has no `--daemon`
///     token and is correctly rejected).
fn argv_is_khive_daemon(args: &str) -> bool {
    let mut tokens = args.split_whitespace();
    let Some(exe_token) = tokens.next() else {
        return false;
    };
    // Compare by file-name basename so absolute paths are handled correctly.
    let basename = std::path::Path::new(exe_token)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if basename != "kkernel" {
        return false;
    }
    let rest: Vec<&str> = tokens.collect();
    rest.contains(&"mcp") && rest.contains(&"--daemon")
}

/// Return `true` if the process with the given PID is identifiable as a khive
/// daemon. Uses `ps -p <pid> -o args=` (portable across macOS and Linux) and
/// verifies that (a) the executable basename is exactly `kkernel`, AND (b) the
/// remaining argv tokens include both `mcp` and `--daemon` (the daemon spawn
/// shape). A process that merely mentions "kkernel" in some other argument
/// position is rejected.
///
/// If the process is gone or is a foreign process, returns `false` — the caller
/// must then clean up the stale PID file without sending SIGTERM.
fn pid_is_khive_daemon(pid: u32) -> bool {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return false;
    };
    if pid_i32 <= 0 {
        return false;
    }
    // Quick liveness check before shelling out.
    // SAFETY: signal 0 is an existence/permission probe with no side effects.
    if unsafe { libc::kill(pid_i32, 0) } != 0 {
        return false;
    }
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
    {
        Ok(out) if out.status.success() => {
            let args = String::from_utf8_lossy(&out.stdout);
            argv_is_khive_daemon(args.trim())
        }
        _ => false,
    }
}

/// Acquire an exclusive advisory flock on the recovery lock file. The returned
/// `File` holds the lock for its lifetime; dropping it releases the lock.
fn acquire_recovery_lock() -> Option<std::fs::File> {
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
    let rc = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
    if rc != 0 {
        tracing::warn!("flock LOCK_EX failed on recovery lock");
        return None;
    }
    Some(file)
}

/// Kill the daemon recorded in the PID file (best-effort, no error on failure).
///
/// Before sending SIGTERM, the PID is validated: `ps` must confirm that the
/// process has basename `kkernel` AND carries both `mcp` and `--daemon` as
/// distinct argv tokens (the daemon spawn shape). If the PID is gone or belongs
/// to a foreign process that does not match that shape, SIGTERM is skipped and
/// only the stale files are cleaned up. An advisory flock on the recovery lock
/// file serializes concurrent clients.
fn kill_stale_daemon() {
    // Hold the lock for the duration of kill + file cleanup so concurrent
    // clients do not race through the same PID/socket.
    let _lock = acquire_recovery_lock();

    let pid_file = pid_path();
    if let Ok(contents) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = contents.trim().parse::<u32>() {
            if pid_is_khive_daemon(pid) {
                if let Ok(signed) = i32::try_from(pid) {
                    if signed > 0 {
                        // SAFETY: SIGTERM is a standard termination signal with no
                        // side effects beyond asking the process to exit.
                        unsafe {
                            libc::kill(signed, libc::SIGTERM);
                        }
                    }
                }
            } else {
                tracing::warn!(
                    pid,
                    "PID in daemon file does not belong to a khive daemon — skipping SIGTERM"
                );
            }
        }
    }
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(socket_path());
}

/// Forward a request to the daemon, auto-spawning it if absent.
///
/// Returns `None` on any failure → caller falls back to local dispatch.
///
/// If the daemon socket is reachable but its response cannot be decoded
/// (stale binary speaking a different wire format), or the decoded response
/// carries a `daemon_protocol_version` that does not match [`PROTOCOL_VERSION`]
/// (new-client + old-daemon scenario), the stale daemon is killed and a fresh
/// one is spawned exactly once. If the fresh daemon still reports the wrong
/// protocol version, a hard protocol-mismatch error is returned rather than
/// silently falling back to local dispatch (which would hide the version skew).
pub async fn forward_or_spawn(frame: &DaemonRequestFrame) -> Option<Result<String, McpError>> {
    if env_truthy("KHIVE_NO_DAEMON") {
        return None;
    }

    match try_forward_inner(frame).await {
        ForwardOutcome::Response(resp) => return map_response(resp, &frame.config_id),
        ForwardOutcome::NoSocket => {}
        ForwardOutcome::ParseFailure => {
            // The socket was up but the response was garbage — stale daemon.
            // Kill it, remove the socket, then fall through to the spawn path.
            tracing::info!("killing stale daemon (undecodable response) and respawning");
            kill_stale_daemon();
            // Give the kernel a moment to release the socket path.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        ForwardOutcome::ProtocolMismatch => {
            // The response was decodable but `daemon_protocol_version` does not
            // match PROTOCOL_VERSION (old daemon silently omits the field → 0).
            // Kill it, respawn, and if the replacement STILL mismatches, return
            // a hard error — never silently accept a stale daemon's results.
            tracing::info!(
                "killing stale daemon (protocol version mismatch, old daemon) and respawning"
            );
            kill_stale_daemon();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            if spawn_daemon().is_err() {
                return Some(Err(McpError::internal_error(
                    format!(
                        "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                         respawn failed — run `make local` to rebuild the daemon binary"
                    ),
                    None,
                )));
            }

            let sock = socket_path();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                if UnixStream::connect(&sock).await.is_ok() {
                    return match try_forward_inner(frame).await {
                        ForwardOutcome::Response(resp) => map_response(resp, &frame.config_id),
                        ForwardOutcome::ProtocolMismatch | ForwardOutcome::ParseFailure => {
                            Some(Err(McpError::internal_error(
                                format!(
                                    "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                                     respawned daemon still reports wrong version — \
                                     run `make local` to rebuild the daemon binary"
                                ),
                                None,
                            )))
                        }
                        ForwardOutcome::NoSocket => Some(Err(McpError::internal_error(
                            format!(
                                "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                                 respawned daemon did not accept connections — \
                                 run `make local` to rebuild the daemon binary"
                            ),
                            None,
                        ))),
                    };
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            return Some(Err(McpError::internal_error(
                format!(
                    "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                     respawned daemon did not become ready within 5s — \
                     run `make local` to rebuild the daemon binary"
                ),
                None,
            )));
        }
    }

    if spawn_daemon().is_err() {
        return None;
    }

    let sock = socket_path();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if UnixStream::connect(&sock).await.is_ok() {
            return match try_forward_inner(frame).await {
                ForwardOutcome::Response(resp) => map_response(resp, &frame.config_id),
                ForwardOutcome::ParseFailure => {
                    tracing::warn!(
                        "freshly spawned daemon also returned an undecodable response; \
                         falling back to local dispatch"
                    );
                    None
                }
                ForwardOutcome::ProtocolMismatch => Some(Err(McpError::internal_error(
                    format!(
                        "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                         run `make local` to rebuild the daemon binary"
                    ),
                    None,
                ))),
                ForwardOutcome::NoSocket => None,
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::daemon::run_daemon;
    use serial_test::serial;

    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

    fn make_test_server() -> crate::server::KhiveMcpServer {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "gtd".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+gtd")
    }

    fn clear_daemon_env() {
        std::env::remove_var("KHIVE_SOCKET");
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::remove_var("KHIVE_LOCK");
    }

    async fn connect_when_ready(sock: &std::path::Path) -> UnixStream {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(s) = UnixStream::connect(sock).await {
                return s;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon never bound {sock:?} within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    async fn exchange(sock: &std::path::Path, frame: &DaemonRequestFrame) -> DaemonResponseFrame {
        let mut stream = UnixStream::connect(sock)
            .await
            .expect("connect to daemon socket");
        let payload = serde_json::to_vec(frame).expect("serialize request frame");
        write_frame(&mut stream, &payload)
            .await
            .expect("write request frame");
        let resp = read_frame(&mut stream).await.expect("read response frame");
        serde_json::from_slice(&resp).expect("decode response frame")
    }

    // ── map_response (pure, MCP-specific) ─────────────────────────────────────

    const CFG: &str = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    fn frame_ok(result: &str) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: true,
            result: Some(result.to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
        }
    }

    fn frame_err(error: Option<&str>) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: error.map(str::to_string),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
        }
    }

    #[test]
    fn map_response_namespace_mismatch_yields_none() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: true,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
        };
        assert!(map_response(resp, CFG).is_none());
    }

    #[test]
    fn map_response_config_mismatch_yields_none() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: true,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
        };
        assert!(map_response(resp, CFG).is_none());
    }

    #[test]
    fn map_response_legacy_daemon_missing_echo_yields_none() {
        // A pre-config_id daemon omits served_config_id (→ None). Even on an
        // ok=true result the client MUST fall back to local dispatch.
        let resp = DaemonResponseFrame {
            ok: true,
            result: Some("served-by-broad-registry".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: None,
            version_mismatch: false,
            daemon_protocol_version: 0,
        };
        assert!(map_response(resp, CFG).is_none());
    }

    #[test]
    fn map_response_echo_drift_yields_none() {
        // A daemon serving under a different config (echo != expected) is not
        // trusted, even without an explicit config_mismatch flag.
        let resp = DaemonResponseFrame {
            ok: true,
            result: Some("served-by-other-config".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(
                "packs=[kg,gtd];db=/x;embed=none;extra=[];backend=main".to_string(),
            ),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
        };
        assert!(map_response(resp, CFG).is_none());
    }

    #[test]
    fn map_response_ok_with_result_yields_some_ok() {
        match map_response(frame_ok("the-result"), CFG) {
            Some(Ok(s)) => assert_eq!(s, "the-result"),
            other => panic!("expected Some(Ok(\"the-result\")), got {other:?}"),
        }
    }

    #[test]
    fn map_response_ok_with_no_result_yields_some_ok_empty() {
        let resp = DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
        };
        match map_response(resp, CFG) {
            Some(Ok(s)) => assert_eq!(s, ""),
            other => panic!("expected Some(Ok(\"\")), got {other:?}"),
        }
    }

    #[test]
    fn map_response_not_ok_yields_some_err_preserving_message() {
        match map_response(frame_err(Some("boom: bad verb")), CFG) {
            Some(Err(McpError { message, .. })) => {
                assert!(message.contains("boom: bad verb"), "got: {message}");
            }
            other => panic!("expected Some(Err(..)), got {other:?}"),
        }
    }

    #[test]
    fn map_response_not_ok_without_message_yields_contextual_err() {
        match map_response(frame_err(None), CFG) {
            Some(Err(McpError { message, .. })) => {
                assert!(!message.is_empty(), "fallback message must not be empty");
                assert!(
                    message.contains("daemon returned an error"),
                    "fallback must say 'daemon returned an error'; got: {message}"
                );
            }
            other => panic!("expected Some(Err(..)), got {other:?}"),
        }
    }

    #[test]
    fn map_response_version_mismatch_yields_explicit_error() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some("daemon protocol mismatch: client=0 daemon=1 — rebuild/update the client binary (make local)".to_string()),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: true,
            daemon_protocol_version: PROTOCOL_VERSION,
        };
        match map_response(resp, CFG) {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("protocol mismatch"),
                    "version mismatch error must name the mismatch; got: {message}"
                );
                assert!(
                    message.contains("make local"),
                    "version mismatch error must tell the operator what to do; got: {message}"
                );
            }
            other => panic!("expected Some(Err(..)): got {other:?}"),
        }
    }

    #[test]
    fn map_response_version_mismatch_without_error_field_synthesizes_message() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
            version_mismatch: true,
            daemon_protocol_version: 99,
        };
        match map_response(resp, CFG) {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("protocol mismatch"),
                    "synthesized message must name the mismatch; got: {message}"
                );
                assert!(
                    message.contains("99"),
                    "synthesized message must include daemon version; got: {message}"
                );
            }
            other => panic!("expected Some(Err(..)): got {other:?}"),
        }
    }

    // ── forward_or_spawn fallback (env-mutating → serial) ─────────────────────

    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_returns_none_when_no_daemon_set() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: "test".to_string(),
            protocol_version: PROTOCOL_VERSION,
        };
        let out = forward_or_spawn(&frame).await;
        assert!(out.is_none());
        assert!(!sock.exists());

        clear_daemon_env();
    }

    // ── daemon socket round-trip (env-mutating → serial) ─────────────────────

    #[tokio::test]
    #[serial]
    async fn daemon_round_trip_dispatches_and_enforces_namespace() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let reference = make_test_server();
        let config_id = reference.config_id().to_string();
        let daemon_server = reference.clone();

        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });

        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        // (a) valid same-namespace, same-config op
        let req = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
        };
        let resp = exchange(&sock, &req).await;
        assert!(resp.ok, "valid op must succeed; error={:?}", resp.error);
        assert!(!resp.namespace_mismatch);
        assert!(!resp.config_mismatch);
        assert!(!resp.version_mismatch);
        assert_eq!(resp.daemon_protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            resp.served_config_id.as_deref(),
            Some(config_id.as_str()),
            "daemon must echo the config it served under"
        );

        let reference_result = reference
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: Some("verbose".to_string()),
                presentation_per_op: None,
            })
            .await
            .expect("local dispatch of stats() must succeed");
        assert_eq!(resp.result.as_deref(), Some(reference_result.as_str()));
        assert!(reference_result.contains("\"entities\""));

        // (b) different namespace → namespace_mismatch (config matches)
        let other = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "other".to_string(),
            config_id: config_id.clone(),
            protocol_version: PROTOCOL_VERSION,
        };
        let resp_other = exchange(&sock, &other).await;
        assert!(resp_other.namespace_mismatch);
        assert!(!resp_other.ok);

        // (c) same namespace but different config (e.g. a `--pack kg` client
        // hitting the broader daemon) → config_mismatch, no dispatch.
        let mismatched_config = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: "packs=[kg];db=:memory:;embed=none;extra=[];backend=main".to_string(),
            protocol_version: PROTOCOL_VERSION,
        };
        let resp_cfg = exchange(&sock, &mismatched_config).await;
        assert!(
            resp_cfg.config_mismatch,
            "differing config must be rejected"
        );
        assert!(!resp_cfg.namespace_mismatch);
        assert!(!resp_cfg.ok);

        // (d) version mismatch → explicit error, NOT namespace/config mismatch
        let wrong_version = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: config_id.clone(),
            protocol_version: 0,
        };
        let resp_ver = exchange(&sock, &wrong_version).await;
        assert!(
            resp_ver.version_mismatch,
            "wrong protocol version must set version_mismatch"
        );
        assert!(!resp_ver.ok);
        assert!(
            resp_ver
                .error
                .as_deref()
                .unwrap_or("")
                .contains("protocol mismatch"),
            "version mismatch error must include 'protocol mismatch'; got: {:?}",
            resp_ver.error
        );
        assert!(
            resp_ver
                .error
                .as_deref()
                .unwrap_or("")
                .contains("make local"),
            "version mismatch error must tell operator what to do; got: {:?}",
            resp_ver.error
        );
        assert_eq!(
            resp_ver.daemon_protocol_version, PROTOCOL_VERSION,
            "daemon must echo its own protocol version in the mismatch response"
        );

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
    }

    // ── new-client + old-daemon regression (fix for #98 BLOCKER) ─────────────
    //
    // Simulates a pre-versioning daemon that:
    //   • Ignores the unknown `protocol_version` request field (it deserializes
    //     as missing and is silently dropped).
    //   • Returns a decodable response with a matching `served_config_id` but
    //     WITHOUT `daemon_protocol_version` or `version_mismatch` (they default
    //     to `0` / `false` on the client).
    //
    // Before the fix, `map_response` accepted this response because `version_mismatch`
    // was false and `served_config_id` matched — the stale daemon was trusted.
    //
    // After the fix, `try_forward_inner` detects `daemon_protocol_version == 0 != 1`
    // and returns `ForwardOutcome::ProtocolMismatch`, which `forward_or_spawn` routes
    // through kill/respawn/error rather than accepting.
    //
    // The test verifies at the `forward_or_spawn` level (not just `map_response`)
    // that:
    //   1. The stale response is NOT accepted.
    //   2. At most one recovery attempt is made (the fake socket closes after one
    //      exchange; a second attempt sees NoSocket rather than looping).
    //   3. A clear "protocol mismatch" error is returned.

    /// Minimal "old daemon" frame: has `served_config_id` but omits the
    /// protocol version fields (they default to `false`/`0` on the client side).
    fn old_daemon_response(config_id: &str) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: true,
            result: Some("stale-result".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(config_id.to_string()),
            // Pre-versioning daemon would never set these:
            version_mismatch: false,
            daemon_protocol_version: 0,
        }
    }

    /// Serve exactly one connection with `response`, then stop accepting.
    async fn serve_one_response(listener: tokio::net::UnixListener, response: DaemonResponseFrame) {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Read the inbound request frame (and discard it — old daemon ignores
            // unknown fields, which is the scenario we're simulating).
            if read_frame(&mut stream).await.is_ok() {
                if let Ok(payload) = serde_json::to_vec(&response) {
                    let _ = write_frame(&mut stream, &payload).await;
                }
            }
        }
        // Listener drops here; subsequent connection attempts see "connection refused".
    }

    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_rejects_old_daemon_and_returns_protocol_mismatch_error() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        // Bind the fake old-daemon socket BEFORE starting the client.
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake old-daemon socket");
        // Write a placeholder PID file so kill_stale_daemon finds a PID to look at.
        // We use the current process's PID, which pid_is_khive_daemon() will reject
        // because the current exe is the test binary (not "kkernel"), so no SIGTERM
        // will be sent — this is the safe path we want to exercise.
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

        let old_resp = old_daemon_response(config_id);
        // Serve exactly one exchange, then let the listener drop (no second connection
        // will be served — this enforces the "at most one recovery attempt" constraint:
        // if forward_or_spawn tried the old socket a second time it would get NoSocket).
        let fake_handle = tokio::spawn(serve_one_response(listener, old_resp));

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
        };

        let result = forward_or_spawn(&frame).await;

        // The fake socket served exactly one old-protocol response.  The fake handle
        // should have completed by now; join it to catch any panics.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

        // Must NOT accept the stale daemon result.
        match result {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("protocol mismatch"),
                    "error must name 'protocol mismatch'; got: {message}"
                );
                assert!(
                    message.contains("make local") || message.contains("rebuild"),
                    "error must tell the operator what to do; got: {message}"
                );
            }
            Some(Ok(v)) => {
                panic!("forward_or_spawn must NOT accept old-daemon response; got Ok({v:?})")
            }
            None => panic!(
                "forward_or_spawn must return Some(Err(..)) for protocol mismatch, \
                 not None (which would cause silent fallback to local dispatch)"
            ),
        }

        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── argv_is_khive_daemon unit tests ───────────────────────────────────────

    #[test]
    fn argv_daemon_true_bare() {
        // Exact daemon argv as spawned by spawn_daemon().
        assert!(argv_is_khive_daemon("kkernel mcp --daemon"));
    }

    #[test]
    fn argv_daemon_true_absolute_path() {
        // Absolute path to kkernel binary — basename must match.
        assert!(argv_is_khive_daemon(
            "/Users/x/.cargo/bin/kkernel mcp --daemon"
        ));
    }

    #[test]
    fn argv_daemon_false_editor_with_kkernel_in_filename() {
        // Editor opened on a file whose name contains kkernel — not a daemon.
        assert!(!argv_is_khive_daemon("vim kkernel-notes.md"));
    }

    #[test]
    fn argv_daemon_false_less_with_kkernel_path() {
        // less paging a kkernel source file — argv[0] is "less", not "kkernel".
        assert!(!argv_is_khive_daemon(
            "less /Users/x/projects/kkernel/daemon.rs"
        ));
    }

    #[test]
    fn argv_daemon_false_kkernel_no_daemon_flag() {
        // kkernel exec subcommand — has kkernel basename but no --daemon token.
        assert!(!argv_is_khive_daemon("kkernel exec 'something'"));
    }

    #[test]
    fn argv_daemon_false_wrapper_argv0_not_kkernel() {
        // A wrapper script passes kkernel mcp --daemon as args but its own
        // argv[0] is "some-wrapper" — basename check must reject it.
        assert!(!argv_is_khive_daemon("some-wrapper kkernel mcp --daemon"));
    }

    #[test]
    fn argv_daemon_false_empty_string() {
        assert!(!argv_is_khive_daemon(""));
    }

    #[test]
    fn argv_daemon_true_with_surrounding_and_inner_whitespace() {
        assert!(argv_is_khive_daemon(
            "  /Users/x/.cargo/bin/kkernel   mcp    --daemon  "
        ));
    }
}
