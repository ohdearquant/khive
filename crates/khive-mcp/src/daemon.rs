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
    self, acquire_recovery_lock, env_truthy, pid_path, read_frame, socket_path, write_frame,
    DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
use rmcp::ErrorData as McpError;
use tokio::net::UnixStream;

use crate::tools::request::RequestParams;

// ── test instrumentation seams ────────────────────────────────────────────────
//
// These counters are only compiled in `#[cfg(test)]` builds. They allow tests
// to assert that kill_stale_daemon_inner and spawn_daemon were called exactly
// the expected number of times — making the recheck-under-lock test
// fail-if-reverted: without the recheck, both counters would be non-zero even
// when a fresh daemon is already alive.

#[cfg(test)]
pub(crate) static KILL_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SPAWN_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// When set to `true` in tests, `pid_is_khive_daemon` returns `true` for any
/// positive PID.  This makes every PID file entry SIGTERM-eligible so that a
/// reverted recheck-under-lock would cause `kill_stale_daemon_inner` to attempt
/// SIGTERM against the real daemon PID — the `KILL_COUNT` assertion catches
/// both the SIGTERM-eligible and the skip-SIGTERM paths.
#[cfg(test)]
pub(crate) static FORCE_PID_IS_DAEMON: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Counts how many times the daemon's dispatcher has been invoked for a
/// NON-probe request.  The exactly-once test asserts this is exactly 1
/// across the entire recovery path.  A reverted fix (real request used as
/// probe + re-forwarded at the call site) yields 2 and fails the assertion.
#[cfg(test)]
pub(crate) static DAEMON_DISPATCH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_counters() {
    KILL_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    SPAWN_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    FORCE_PID_IS_DAEMON.store(false, std::sync::atomic::Ordering::SeqCst);
    DAEMON_DISPATCH.store(0, std::sync::atomic::Ordering::SeqCst);
}

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
        Err(e) => {
            // The request was sent but the daemon closed the connection before
            // sending a response frame — likely a daemon crash or panic during
            // dispatch. Treat as ParseFailure (not NoSocket) so the stale daemon
            // is killed and a fresh one is spawned, preventing the caller from
            // silently falling back to local dispatch against a broken daemon.
            tracing::warn!(
                error = %e,
                "daemon closed connection without sending a response \
                 (crash during dispatch?) — treating as stale"
            );
            return ForwardOutcome::ParseFailure;
        }
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
    #[cfg(test)]
    SPAWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

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
    if basename != "kkernel" && basename != "kkernel-bench" {
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
    // Test seam: when FORCE_PID_IS_DAEMON is set, treat any positive live PID
    // as a daemon so the SIGTERM branch is reachable in tests.
    #[cfg(test)]
    if FORCE_PID_IS_DAEMON.load(std::sync::atomic::Ordering::SeqCst) {
        // SAFETY: signal 0 is an existence/permission probe with no side effects.
        return unsafe { libc::kill(pid_i32, 0) } == 0;
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

/// Kill the daemon and remove its PID + socket files (caller holds the lock).
fn kill_stale_daemon_inner() {
    #[cfg(test)]
    KILL_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

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

/// Outcome of the under-lock identity probe.
enum ProbeOutcome {
    /// A live, identity-matching daemon responded before the deadline.
    Alive,
    /// Daemon is absent, crashed, or identity-mismatched — safe to kill+spawn.
    Dead,
    /// Probe timed out — daemon may be alive but slow; do NOT kill.
    Timeout,
}

/// Send a `probe_only` frame to the daemon and return whether a live,
/// identity-matching daemon responded within `timeout_ms` milliseconds.
///
/// Uses `DaemonRequestFrame::probe_only = true` so the daemon returns an
/// identity frame immediately after identity validation — without calling any
/// dispatcher verb, touching the DB, or executing any mutation.
///
/// Probe outcomes → kill decision:
///   `Alive`   → do NOT kill (identity-matching daemon is healthy)
///   `Dead`    → kill+spawn (definitively absent/crashed/mismatched)
///   `Timeout` → do NOT kill (daemon may be healthy-but-busy; NEVER-KILL-SLOW)
async fn probe_daemon_identity(config_id: &str, namespace: &str, timeout_ms: u64) -> ProbeOutcome {
    let probe = DaemonRequestFrame {
        ops: String::new(),
        presentation: None,
        presentation_per_op: None,
        namespace: namespace.to_string(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: true,
    };
    let deadline = std::time::Duration::from_millis(timeout_ms);
    match tokio::time::timeout(deadline, try_forward_inner(&probe)).await {
        Err(_elapsed) => {
            tracing::debug!(
                timeout_ms,
                "under-lock probe timed out — daemon may be busy; skipping kill"
            );
            ProbeOutcome::Timeout
        }
        Ok(ForwardOutcome::Response(resp)) => {
            // Positive probe-ack confirmation: the response must carry the exact
            // probe-ack sentinel shape (ok=true, result=None, error=None) plus
            // pass all identity checks.
            //
            // Why the sentinel is necessary: `probe_only` is `#[serde(default)]`
            // and the schema has no deny_unknown_fields. A daemon built before this
            // field existed (same protocol version, older binary) deserialises the
            // probe frame without error and falls through to normal dispatch on the
            // empty `ops` string. That produces ok=false (parse error) with matching
            // identity fields — it would be misclassified as Alive by identity
            // checks alone, leaving a stale daemon in place. Requiring the probe-ack
            // shape makes the classifier fail-closed: only the explicit probe branch
            // in handle_conn produces ok=true + result=None + error=None.
            //
            // A normal successful dispatch always sets result=Some(_) (never None).
            // A normal error dispatch sets ok=false. So the sentinel is unambiguous.
            let is_probe_ack = resp.ok && resp.result.is_none() && resp.error.is_none();
            if is_probe_ack
                && !resp.version_mismatch
                && !resp.namespace_mismatch
                && !resp.config_mismatch
                && resp.daemon_protocol_version == PROTOCOL_VERSION
                && resp.served_config_id.as_deref() == Some(config_id)
            {
                tracing::debug!("under-lock probe: live matching daemon confirmed; skipping kill");
                ProbeOutcome::Alive
            } else {
                tracing::debug!(
                    is_probe_ack,
                    version_mismatch = resp.version_mismatch,
                    namespace_mismatch = resp.namespace_mismatch,
                    config_mismatch = resp.config_mismatch,
                    "under-lock probe: daemon did not return probe-ack or identity mismatch — will kill+spawn"
                );
                ProbeOutcome::Dead
            }
        }
        Ok(
            ForwardOutcome::NoSocket
            | ForwardOutcome::ParseFailure
            | ForwardOutcome::ProtocolMismatch,
        ) => ProbeOutcome::Dead,
    }
}

/// Outcome returned by [`kill_and_respawn`] to the call site.
enum RecoveryOutcome {
    /// A concurrent client already replaced the daemon; forward the real request
    /// via the normal path (no new spawn occurred).
    Skipped,
    /// This client killed the stale daemon and spawned a replacement; caller
    /// must wait for readiness then forward the real request.
    Spawned,
}

/// Kill the stale daemon and spawn a fresh one under a single recovery lock.
///
/// Implements double-checked recovery: after acquiring the lock, sends a
/// bounded `probe_only` frame to the daemon (500 ms timeout). The probe uses
/// a DB-free identity check that the daemon answers without dispatching any
/// verb. Three outcomes:
///
///   `Alive`   → a concurrent client already replaced the stale daemon; return
///               `RecoveryOutcome::Skipped` without killing anything. The caller
///               forwards the real request exactly once via the normal path.
///   `Timeout` → daemon may be alive but slow; return `Skipped` without
///               killing (NEVER-KILL-SLOW invariant). Caller forwards the real
///               request; if the daemon is genuinely wedged the caller sees
///               ParseFailure on that forward — the same recovery path re-runs.
///   `Dead`    → kill+spawn under lock; return `RecoveryOutcome::Spawned`.
///
/// The lock is held only across the bounded probe and the kill/spawn. It is
/// released BEFORE the caller's readiness-probe loop and BEFORE the real
/// request is forwarded — so the daemon never races with a lock-holding client.
async fn kill_and_respawn(config_id: &str, namespace: &str) -> std::io::Result<RecoveryOutcome> {
    let _lock = acquire_recovery_lock();
    match probe_daemon_identity(config_id, namespace, 500).await {
        ProbeOutcome::Alive | ProbeOutcome::Timeout => {
            return Ok(RecoveryOutcome::Skipped);
        }
        ProbeOutcome::Dead => {}
    }
    kill_stale_daemon_inner();
    spawn_daemon()?;
    Ok(RecoveryOutcome::Spawned)
}

/// Forward a request to the daemon, auto-spawning it if absent.
///
/// Returns `None` on any failure → caller falls back to local dispatch.
///
/// If the daemon socket is reachable but its response cannot be decoded
/// (stale binary speaking a different wire format), or the decoded response
/// carries a `daemon_protocol_version` that does not match [`PROTOCOL_VERSION`]
/// (new-client + old-daemon scenario), the stale daemon is killed and a fresh
/// one is spawned exactly once under a single recovery lock. If the fresh daemon
/// still reports the wrong protocol version, a hard protocol-mismatch error is
/// returned rather than silently falling back to local dispatch.
pub async fn forward_or_spawn(frame: &DaemonRequestFrame) -> Option<Result<String, McpError>> {
    if env_truthy("KHIVE_NO_DAEMON") {
        return None;
    }

    match try_forward_inner(frame).await {
        ForwardOutcome::Response(resp) => return map_response(resp, &frame.config_id),
        ForwardOutcome::NoSocket => {
            // No socket present; fall through to the first-spawn path below.
        }
        ForwardOutcome::ParseFailure => {
            // The socket was up but the response was garbage — stale daemon.
            // kill_and_respawn holds the recovery lock across a bounded probe-only
            // frame (500ms timeout, no verb dispatch) then kill + spawn. A concurrent
            // client that already replaced the stale daemon causes the probe to return
            // Skipped — the real request is forwarded exactly once below, never used
            // as the probe itself.
            tracing::info!("killing stale daemon (undecodable response) and respawning");
            match kill_and_respawn(&frame.config_id, &frame.namespace).await {
                Err(_) => return None,
                Ok(RecoveryOutcome::Skipped) => {
                    // Under-lock probe confirmed a live matching daemon; forward the
                    // real request once — this is its ONLY dispatch on this path.
                    return match try_forward_inner(frame).await {
                        ForwardOutcome::Response(resp) => map_response(resp, &frame.config_id),
                        _ => None,
                    };
                }
                Ok(RecoveryOutcome::Spawned) => {}
            }
            // Give the kernel a moment to release the socket path and let the
            // spawned daemon process start.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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
            return None;
        }
        ForwardOutcome::ProtocolMismatch => {
            // The response was decodable but `daemon_protocol_version` does not
            // match PROTOCOL_VERSION (old daemon silently omits the field → 0).
            // kill_and_respawn holds the recovery lock across kill + spawn and
            // re-probes under the lock to avoid killing a freshly-spawned daemon.
            tracing::info!(
                "killing stale daemon (protocol version mismatch, old daemon) and respawning"
            );
            match kill_and_respawn(&frame.config_id, &frame.namespace).await {
                Err(_) => {
                    return Some(Err(McpError::internal_error(
                        format!(
                            "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                             respawn failed — run `make local` to rebuild the daemon binary"
                        ),
                        None,
                    )));
                }
                Ok(RecoveryOutcome::Skipped) => {
                    // Under-lock probe confirmed a live matching daemon; forward the
                    // real request once — this is its ONLY dispatch on this path.
                    return match try_forward_inner(frame).await {
                        ForwardOutcome::Response(resp) => map_response(resp, &frame.config_id),
                        _ => Some(Err(McpError::internal_error(
                            format!(
                                "daemon protocol mismatch: expected version {PROTOCOL_VERSION}; \
                                 run `make local` to rebuild the daemon binary"
                            ),
                            None,
                        ))),
                    };
                }
                Ok(RecoveryOutcome::Spawned) => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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

    // NoSocket path: first-time spawn (no stale daemon to kill).
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
            probe_only: false,
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
            probe_only: false,
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
            probe_only: false,
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
            probe_only: false,
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
            probe_only: false,
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
            probe_only: false,
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

    // ── daemon crash mid-dispatch regression (#91) ────────────────────────────
    //
    // When the daemon crashes (or panics) during dispatch it closes the
    // connection without writing a response frame. Before the fix, `try_forward_inner`
    // returned `ForwardOutcome::NoSocket` on the `read_frame` error, causing
    // `forward_or_spawn` to return `None` — a silent fallback to local dispatch.
    //
    // The fix promotes the `read_frame` error to `ForwardOutcome::ParseFailure`
    // so the stale daemon is killed and the client does NOT silently accept a
    // potentially broken daemon state for subsequent requests.
    //
    // This test binds a fake socket that reads the request but immediately drops
    // the connection without writing a response (simulating a crash during
    // dispatch), then asserts that `forward_or_spawn` falls back to `None`
    // (because the respawn attempt also sees no socket) with the WARN log
    // rather than silently accepting the empty response.
    //
    // We cannot assert an `Err` here because after killing the stale daemon the
    // respawn path calls `spawn_daemon()` (which tries to exec the real binary),
    // fails or times out, and returns `None`. The critical invariant is that
    // `NoSocket` is NOT returned directly on read failure — the `ParseFailure`
    // path is taken instead (logging the crash and killing the stale process).
    // The `try_forward_inner` unit test below validates the exact `ForwardOutcome`
    // discriminant.

    /// Serve one connection: read the request frame, then drop the stream
    /// without writing any response (simulating a daemon crash mid-dispatch).
    async fn serve_crash_on_dispatch(listener: tokio::net::UnixListener) {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Read the inbound request (and discard it — the "crash" happens here).
            let _ = read_frame(&mut stream).await;
            // Drop stream without writing a response — connection resets.
        }
        // Listener drops; subsequent connection attempts see "connection refused".
    }

    #[tokio::test]
    #[serial]
    async fn try_forward_inner_returns_parse_failure_when_daemon_closes_without_response() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        let listener =
            tokio::net::UnixListener::bind(&sock).expect("bind fake crash-daemon socket");
        // Write a placeholder PID file with the current process PID so
        // kill_stale_daemon has something to look at (it will skip SIGTERM
        // because the exe basename is not "kkernel" — which is the safe path).
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

        // Serve one connection that crashes without replying.
        let fake_handle = tokio::spawn(serve_crash_on_dispatch(listener));

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
        };

        // Call try_forward_inner directly to assert the discriminant.
        let outcome = try_forward_inner(&frame).await;

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

        assert!(
            matches!(outcome, ForwardOutcome::ParseFailure),
            "daemon crash (connection closed without response) must yield \
             ParseFailure, not NoSocket — got a different variant"
        );

        clear_daemon_env();
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

    #[test]
    fn argv_daemon_true_kkernel_bench_basename() {
        // bench binary copy is named "kkernel-bench"; pid management must treat it
        // as a valid daemon so stale bench daemons are SIGTERM'd on respawn.
        assert!(argv_is_khive_daemon("kkernel-bench mcp --daemon"));
        assert!(argv_is_khive_daemon(
            "/Users/x/.cargo/bin/kkernel-bench mcp --daemon"
        ));
    }

    // ── concurrent recovery — second client skips kill+spawn when daemon alive ──
    //
    // Exercises the recheck-under-lock (double-checked locking) in
    // kill_and_respawn.  Scenario:
    //
    //   1. A real daemon is running (via run_daemon).
    //   2. A recovering client calls kill_and_respawn directly — simulating a
    //      client that observed ParseFailure (from a now-dead OLD daemon) and
    //      wants to replace it, but a concurrent first-recoverer has ALREADY
    //      spawned a healthy daemon before this client reached the lock.
    //   3. Under the lock, kill_and_respawn sends a probe_only frame and finds a
    //      responsive, identity-matching daemon → returns RecoveryOutcome::Skipped.
    //   4. KILL_COUNT must be 0 and SPAWN_COUNT must be 0.
    //   5. The fresh daemon's PID file and socket must survive intact.
    //
    // FORCE_PID_IS_DAEMON=true makes every live PID SIGTERM-eligible so that
    // if the bounded-probe is removed (reverted) kill_stale_daemon_inner would
    // attempt SIGTERM against the real daemon PID, KILL_COUNT would be 1, and
    // the assertion below would catch the regression.
    //
    // Fail-if-reverted assertion: the `assert_eq!(KILL_COUNT, 0)` below catches
    // a reverted probe because without it, kill_stale_daemon_inner is called
    // unconditionally and KILL_COUNT increments to 1.

    #[tokio::test]
    #[serial]
    async fn concurrent_recovery_second_client_skips_kill_when_daemon_alive() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        // Run a real daemon so probe_daemon_identity in kill_and_respawn finds a
        // live, responsive, identity-matching daemon under the lock.
        let server = make_test_server();
        let config_id = server.config_id().to_string();
        let daemon_server = server.clone();
        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });

        // Wait for the daemon to bind the socket and write its PID file.
        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        // Record the daemon PID as written by run_daemon.
        let daemon_pid_str =
            std::fs::read_to_string(&pid_file).expect("daemon must have written a pid file");
        let daemon_pid: u32 = daemon_pid_str
            .trim()
            .parse()
            .expect("daemon pid file must contain a u32");

        // Arm the SIGTERM-eligible hook: pid_is_khive_daemon() will now return
        // true for the live daemon PID.  Without the bounded-probe, a reverted
        // kill_and_respawn would send SIGTERM to that PID and unlink the socket —
        // KILL_COUNT catches both paths.
        FORCE_PID_IS_DAEMON.store(true, std::sync::atomic::Ordering::SeqCst);
        reset_counters();

        // Call kill_and_respawn directly — simulates a second recovering client
        // whose turn arrives after the first recoverer already replaced the stale
        // daemon.  The bounded probe confirms the live daemon; Skipped is returned
        // without killing.
        let outcome = kill_and_respawn(&config_id, "test").await;

        assert!(
            matches!(outcome, Ok(RecoveryOutcome::Skipped)),
            "kill_and_respawn must return Ok(RecoveryOutcome::Skipped) when a live \
             matching daemon exists under the lock"
        );
        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "KILL_COUNT must be 0: the probe found the daemon alive so \
             kill_stale_daemon_inner must NOT be called \
             (this assertion fails if the probe-under-lock is removed)"
        );
        assert_eq!(
            SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "SPAWN_COUNT must be 0: no respawn needed when the daemon is alive \
             (this assertion fails if the probe-under-lock is removed)"
        );

        // The daemon's PID file and socket must be intact.
        assert!(
            pid_file.exists(),
            "PID file must survive: kill_and_respawn must NOT unlink it"
        );
        assert!(
            sock.exists(),
            "socket must survive: kill_and_respawn must NOT unlink it"
        );
        let surviving_pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("pid file readable")
            .trim()
            .parse()
            .expect("pid file is a u32");
        assert_eq!(
            surviving_pid, daemon_pid,
            "PID in file must be the original daemon PID — no new daemon was spawned"
        );

        handle.abort();
        let _ = handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── oversized daemon response does not trigger kill/respawn ───────────────
    //
    // When the daemon's serialized response exceeds MAX_FRAME_BYTES the server
    // sends a small explicit error frame (ok=false, "response too large") instead
    // of closing the connection without a response.  The client receives this as
    // ForwardOutcome::Response (decodable frame) → map_response → Some(Err(..));
    // NOT ParseFailure, so no kill/respawn is triggered.
    //
    // This test drives the REAL handle_conn server path via run_daemon with a
    // BigDispatch that returns a string larger than MAX_FRAME_BYTES.  The client
    // calls forward_or_spawn and must receive Some(Err(..)) containing "too large".
    // The daemon's PID file and socket must survive the call.
    //
    // Fail-if-reverted: if the handle_conn oversized gate (the `if payload.len()
    // > MAX_FRAME_BYTES` branch that sends the small error frame) is removed,
    // handle_conn falls through to write_frame with the oversized payload, which
    // write_frame REJECTS (its own guard returns Err), causing the connection to
    // close without a response → the client sees ParseFailure → kill_and_respawn
    // runs → KILL_COUNT > 0 and the PID file assertion fails.

    /// A minimal DaemonDispatch that returns a payload larger than MAX_FRAME_BYTES
    /// so handle_conn's oversized guard fires and emits the "response too large"
    /// error frame.
    #[derive(Clone)]
    struct BigDispatch {
        namespace: String,
        config_id: String,
    }

    #[async_trait]
    impl daemon::DaemonDispatch for BigDispatch {
        async fn dispatch(
            &self,
            _ops: String,
            _presentation: Option<String>,
            _presentation_per_op: Option<Vec<Option<String>>>,
        ) -> Result<String, String> {
            // Return a string whose serialized DaemonResponseFrame JSON length
            // exceeds MAX_FRAME_BYTES.  The frame JSON overhead is ~200 bytes so
            // a result of this size is comfortably over the cap.
            Ok("X".repeat(khive_runtime::daemon::MAX_FRAME_BYTES + 1))
        }

        async fn warm_all(&self) {}

        fn namespace(&self) -> &str {
            &self.namespace
        }

        fn config_id(&self) -> &str {
            &self.config_id
        }
    }

    #[tokio::test]
    #[serial]
    async fn oversized_daemon_response_sends_error_frame_not_kills_daemon() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "test-oversized-config";
        let dispatcher = BigDispatch {
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
        };

        // Run the real daemon server (with handle_conn's oversized gate live).
        let handle = tokio::spawn(async move {
            let _ = run_daemon(dispatcher).await;
        });

        // Wait for the daemon to bind and write its PID.
        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        let daemon_pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("daemon must have written a pid file")
            .trim()
            .parse()
            .expect("daemon pid must be a u32");

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
        };

        let result = forward_or_spawn(&frame).await;

        // Primary assertion: the daemon's PID file and socket must be intact
        // (no kill/respawn triggered).
        assert!(
            pid_file.exists(),
            "PID file must survive: oversized response is NOT a daemon crash"
        );
        assert!(
            sock.exists(),
            "socket must survive: oversized response is NOT a daemon crash"
        );
        let surviving_pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("pid file readable")
            .trim()
            .parse()
            .expect("pid file is a u32");
        assert_eq!(
            surviving_pid, daemon_pid,
            "daemon PID must not change — no kill+respawn occurred"
        );
        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "KILL_COUNT must be 0 — oversized response must NOT trigger \
             kill_stale_daemon_inner (fails if handle_conn oversized gate is removed)"
        );

        // The result must be Some(Err(..)) containing "too large" — the explicit
        // error frame the server sends when the real response is oversized.
        match result {
            Some(Err(e)) => {
                assert!(
                    e.message.contains("too large"),
                    "error must describe the oversized response; got: {}",
                    e.message
                );
            }
            Some(Ok(_)) => panic!("oversized response must not produce Ok result"),
            None => panic!(
                "oversized response must produce Some(Err(..)) from the explicit \
                 error frame, not None (None would mean map_response fell back to \
                 local dispatch, hiding the server-side error)"
            ),
        }

        handle.abort();
        let _ = handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── EXACTLY-ONCE: real request dispatched exactly once on recovery path ────
    //
    // Scenario:
    //   1. A fake "stale" socket reads one request frame then closes without
    //      responding (simulating a crashed old daemon on the first forward).
    //   2. `forward_or_spawn` sees ParseFailure → enters kill_and_respawn.
    //   3. Under the lock, probe_daemon_identity sends a probe_only frame to a
    //      REAL CountingDispatch daemon (already running).  The daemon's
    //      handle_conn returns an identity frame without calling dispatch() —
    //      DAEMON_DISPATCH stays 0.
    //   4. kill_and_respawn returns RecoveryOutcome::Skipped (live daemon found).
    //   5. The call site forwards the REAL request once via try_forward_inner.
    //      CountingDispatch.dispatch() is called → DAEMON_DISPATCH == 1.
    //
    // Fail-if-reverted: if kill_and_respawn is reverted to use the real frame as
    // the probe (try_forward_inner(frame) under the lock), CountingDispatch.dispatch()
    // is called for the probe (DAEMON_DISPATCH == 1), and again at the call site
    // (DAEMON_DISPATCH == 2).  The assert_eq!(DAEMON_DISPATCH, 1) then fails.
    //
    // FOLLOWUP: this test calls kill_and_respawn + try_forward_inner directly to
    // avoid the two-socket problem (stale socket for first forward, real socket
    // for probe).  A future enhancement could drive the full forward_or_spawn
    // entry point using a proxy fake-socket that serves garbage on the first
    // connection then falls through to the real daemon — catching any future
    // call-site that double-forwards after RecoveryOutcome::Skipped without
    // going through the seam functions.  File a follow-up issue if needed.

    /// A minimal DaemonDispatch that increments DAEMON_DISPATCH on every real
    /// (non-probe) dispatch.  probe_only frames never reach dispatch() — they
    /// are short-circuited by handle_conn before calling the dispatcher.
    #[derive(Clone)]
    struct CountingDispatch {
        namespace: String,
        config_id: String,
    }

    #[async_trait]
    impl daemon::DaemonDispatch for CountingDispatch {
        async fn dispatch(
            &self,
            _ops: String,
            _presentation: Option<String>,
            _presentation_per_op: Option<Vec<Option<String>>>,
        ) -> Result<String, String> {
            DAEMON_DISPATCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("{\"ok\":true,\"counted\":true}".to_string())
        }

        async fn warm_all(&self) {}

        fn namespace(&self) -> &str {
            &self.namespace
        }

        fn config_id(&self) -> &str {
            &self.config_id
        }
    }

    #[tokio::test]
    #[serial]
    async fn recovery_path_dispatches_real_request_exactly_once() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let real_sock = dir.path().join("khived.sock");
        let stale_sock = dir.path().join("stale.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        // Start a real CountingDispatch daemon on `real_sock`.
        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
        let counting_dispatcher = CountingDispatch {
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
        };
        // Temporarily point KHIVE_SOCKET at real_sock to let run_daemon bind there.
        std::env::set_var("KHIVE_SOCKET", &real_sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let counting_handle = tokio::spawn(async move {
            let _ = run_daemon(counting_dispatcher).await;
        });
        let _ready = connect_when_ready(&real_sock).await;
        drop(_ready);

        // Bind the stale fake socket on `stale_sock` BEFORE redirecting the client
        // env so the client sees it first.  The fake stale socket reads one frame
        // then drops without responding — simulating a crashed old daemon.
        let stale_listener =
            tokio::net::UnixListener::bind(&stale_sock).expect("bind stale socket");
        let stale_handle = tokio::spawn(serve_crash_on_dispatch(stale_listener));

        // Now point the client at the STALE socket so its first try_forward_inner
        // sees ParseFailure (the stale socket crashes after reading the request).
        // The recovery lock and PID file point at the stale path so kill_stale_daemon
        // looks at a non-kkernel PID (the current test process) and skips SIGTERM.
        let stale_pid_file = dir.path().join("stale.pid");
        std::fs::write(&stale_pid_file, std::process::id().to_string()).expect("write stale pid");
        std::env::set_var("KHIVE_SOCKET", &stale_sock);
        std::env::set_var("KHIVE_PID", &stale_pid_file);

        // After kill_and_respawn's probe, the probe needs to reach the REAL daemon.
        // We achieve this by having the probe use `real_sock` (via KHIVE_SOCKET).
        // Redirect KHIVE_SOCKET back to the real daemon socket AFTER the stale
        // socket has been read (but we need the probe to already know where to look).
        //
        // Simpler approach: use a single socket for both — the stale response is
        // from a `serve_crash_on_dispatch` (closes after one read), the NEXT
        // connection attempt (the probe) goes to the same path, but the stale
        // listener is now gone.  Instead we use real_sock for the probe by
        // redirecting KHIVE_SOCKET before the probe fires.
        //
        // The cleanest approach: let forward_or_spawn do everything from the stale
        // socket (ParseFailure), then kill_and_respawn calls probe_daemon_identity
        // which uses socket_path() — still pointing at stale_sock (now dead).
        // probe_daemon_identity sees NoSocket → ProbeOutcome::Dead → kill+spawn
        // path.  That's NOT the scenario we want to test (double-dispatch on Skipped).
        //
        // To get the "Skipped" (live daemon under lock) scenario without a real
        // spawn: call kill_and_respawn directly with KHIVE_SOCKET pointing at the
        // live CountingDispatch daemon, and separately assert DAEMON_DISPATCH from
        // a direct try_forward_inner call.

        // Point back at the real daemon for the probe and the real forward.
        std::env::set_var("KHIVE_SOCKET", &real_sock);
        std::env::set_var("KHIVE_PID", &pid_file);

        // Simulate the exactly-once scenario:
        //   (a) kill_and_respawn sees a live daemon → returns Skipped (0 dispatches)
        //   (b) call site forwards the real request once → 1 dispatch
        let recovery = kill_and_respawn(config_id, "test").await;
        assert!(
            matches!(recovery, Ok(RecoveryOutcome::Skipped)),
            "probe must find the live CountingDispatch daemon and return Skipped"
        );
        // DAEMON_DISPATCH must still be 0: the probe_only frame does not reach dispatch().
        assert_eq!(
            DAEMON_DISPATCH.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "probe_only frame must NOT increment DAEMON_DISPATCH \
             (fails if the real request is used as the probe)"
        );

        // Now forward the real request exactly once — the call site's single forward.
        let real_frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: config_id.to_string(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
        };
        let fwd = try_forward_inner(&real_frame).await;
        assert!(
            matches!(fwd, ForwardOutcome::Response(_)),
            "real forward after Skipped must succeed; got non-Response outcome"
        );

        // DAEMON_DISPATCH must now be exactly 1.
        assert_eq!(
            DAEMON_DISPATCH.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "real request must be dispatched EXACTLY ONCE across the recovery path \
             (assert fails with count==2 if the real frame is used as the probe \
              AND re-forwarded at the call site — the double-dispatch bug)"
        );

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stale_handle).await;
        counting_handle.abort();
        let _ = counting_handle.await;
        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }

    // ── probe classifier is fail-CLOSED for same-protocol pre-probe daemons ──
    //
    // Regression test for the version-skew gap: a daemon built BEFORE probe_only
    // was introduced but carrying PROTOCOL_VERSION (same numeric version, older
    // binary) deserialises the probe frame via serde default and falls through to
    // dispatch on the empty `ops` string.  It returns ok=false (parse error on
    // empty ops) WITH matching identity fields (namespace / config / protocol all
    // match).  Before leg-1 of the round-5 fix, probe_daemon_identity classified
    // ANY response with matching identity as Alive, leaving the stale daemon in
    // place.
    //
    // After the fix, the classifier requires the probe-ack sentinel shape:
    //   resp.ok && resp.result.is_none() && resp.error.is_none()
    // An ok=false response fails this predicate → Dead → kill+spawn.
    //
    // Fail-if-reverted: removing the `is_probe_ack` check from the classifier
    // causes the ok=false identity-matching response to be classified Alive →
    // kill_and_respawn returns Skipped → KILL_COUNT stays 0 → assertion fails.

    /// Build a response that matches all identity fields but has ok=false (the
    /// shape a pre-probe daemon produces when dispatching the empty-ops probe).
    fn pre_probe_daemon_response(config_id: &str) -> DaemonResponseFrame {
        DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some("parse error: empty ops string".to_string()),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(config_id.to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
        }
    }

    #[tokio::test]
    #[serial]
    async fn probe_classifier_dead_when_same_protocol_daemon_lacks_probe_support() {
        clear_daemon_env();
        reset_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid_file = dir.path().join("khived.pid");
        let lock_file = dir.path().join("khived.recovery.lock");

        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid_file);
        std::env::set_var("KHIVE_LOCK", &lock_file);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

        // Bind a fake socket that serves one pre-probe response, then stops
        // accepting (simulates a same-protocol daemon without probe_only support).
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake pre-probe socket");
        std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
        let pre_probe_resp = pre_probe_daemon_response(config_id);
        let fake_handle = tokio::spawn(serve_one_response(listener, pre_probe_resp));

        // Arm FORCE_PID_IS_DAEMON so kill_stale_daemon_inner would attempt SIGTERM
        // IF it were called.  This makes KILL_COUNT the reliable regression signal:
        // if the classifier incorrectly returns Alive (Skipped), kill is not called
        // and KILL_COUNT stays 0.
        FORCE_PID_IS_DAEMON.store(true, std::sync::atomic::Ordering::SeqCst);
        reset_counters();

        let outcome = kill_and_respawn(config_id, "test").await;

        // The fake socket served exactly one response; join it before asserting.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

        // The ok=false response must NOT be classified as Alive.  kill_and_respawn
        // must attempt kill+spawn (Spawned outcome — spawn itself fails because there
        // is no real kkernel binary in test, but KILL_COUNT is checked BEFORE spawn).
        assert!(
            matches!(outcome, Ok(RecoveryOutcome::Spawned) | Err(_)),
            "pre-probe same-protocol daemon must NOT be classified Alive; \
             expected Spawned or spawn-error, got Skipped"
        );
        assert_eq!(
            KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "KILL_COUNT must be 1 — the pre-probe response must classify as Dead \
             so kill_stale_daemon_inner is called \
             (this fails if is_probe_ack check is removed and ok=false response \
              is incorrectly classified as Alive)"
        );

        reset_counters();
        clear_daemon_env();
        std::env::remove_var("KHIVE_LOCK");
    }
}
