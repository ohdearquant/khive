//! khived daemon client — forwarding + auto-spawn.
//!
//! The daemon server lives in `khive-runtime::daemon`. This module provides the
//! client side: [`forward_or_spawn`] connects to the daemon, auto-spawns it on
//! first use, and maps responses to MCP error types. Ordinary fallback paths
//! return `None` so the caller can dispatch locally. `KHIVE_DAEMON_STRICT=1`
//! (#947) is the exception: it turns a recordable fallback into a
//! caller-visible per-op error instead, via `fallback_or_reject`.
//! `KHIVE_NO_DAEMON` and `crate::server`'s `save_to` bypass remain
//! intentional, unconditional local paths — neither is affected by strict
//! mode, since nothing is ever recorded or falls back for them.
//!
//! Also provides the [`khive_runtime::daemon::DaemonDispatch`] impl for [`crate::server::KhiveMcpServer`].

use async_trait::async_trait;
#[cfg(test)]
use khive_runtime::daemon::MAX_FRAME_BYTES;
use khive_runtime::daemon::{
    self, acquire_recovery_lock, env_truthy, pid_path, read_frame, socket_path, write_frame,
    DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
#[cfg(test)]
use khive_runtime::process_retry::{spawn_retrying_executable_busy, EXECUTABLE_BUSY_BACKOFF_MS};
use rmcp::ErrorData as McpError;
use sha2::{Digest, Sha256};
use tokio::net::UnixStream;

use crate::tools::request::RequestParams;

mod dispatch;
pub(crate) mod executable;
mod fallback;
mod forward;
mod launch;
mod self_heal;

pub(crate) use fallback::{bridge_diagnostics_snapshot, FallbackReason, STRICT_FALLBACK_MARKER};
use fallback::{fallback_or_reject, is_daemon_strict_mode, opaque_config_id};

#[cfg(test)]
pub(crate) use fallback::{
    fallback_count, reset_fallback_counters, test_recordable_fallback_rejected,
};
#[cfg(test)]
use fallback::{
    fallback_strict_violations, fallback_total, first_config_mismatch_field, record_fallback,
    FallbackSeverity, FALLBACK_NO_SOCKET, FALLBACK_PARSE_FAILURE,
};

use forward::{
    acquire_supervisor_marker_lock, bounded_retry_deadline, daemon_mcp_error, map_response,
    pid_file_directory_is_trusted_if_present, protocol_mismatch_error, protocol_mismatch_message,
    read_supervisor_marker, recorded_daemon_is_alive, request_too_large_error, sleep_until_retry,
    try_forward_before, try_forward_with_read_replay, wait_for_supervisor, ForwardOutcome,
    ReadReplayBudget, SupervisorMarker,
};

#[cfg(test)]
use forward::{
    classify_socket_connect_error, socket_exchange_deadline, supervisor_marker_lock_path,
    try_forward_inner, DEFAULT_SUPERVISOR_RESTART_INTERVAL,
};

use launch::{spawn_daemon, spawn_daemon_with_exe_and_config};

#[cfg(test)]
use launch::{
    daemon_launch_command, daemon_log_path, daemon_log_path_from_home, daemon_log_should_rotate,
    prepare_daemon_log_file_with_cap, spawn_daemon_with_exe, DAEMON_LOG_MAX_BYTES,
};

use self_heal::{arm_executable_self_heal, trigger_bridge_self_heal};
pub(crate) use self_heal::{resumed_generation, SelfHealOnFlushTransport};
// Preserve the original crate-visible paths, including in non-test builds.
#[allow(unused_imports)]
pub(crate) use self_heal::{
    fire_pending_self_heal, schedule_drain_and_exit, schedule_reexec_on_mismatch,
};

#[cfg(all(test, unix))]
pub(crate) use self_heal::REEXEC_INVOKED_COUNT;
#[cfg(test)]
use self_heal::{
    clear_pending_self_heal, decide_mismatch_recovery, resumed_generation_from_args,
    MismatchRecovery, PENDING_SELF_HEAL,
};
#[cfg(test)]
pub(crate) use self_heal::{reset_self_heal_counters, DRAIN_EXIT_INVOKED_COUNT};

/// Snapshot the stdio bridge image before config discovery or database boot can
/// wait across an installation. Daemon and one-shot exec entrypoints omit this.
pub fn capture_bridge_executable() {
    let _ = crate::server::bridge_instance_id();
    executable::capture_at_startup();
}

#[cfg(test)]
mod bridge_diagnostics_tests;

#[cfg(test)]
mod memory_namespace_tests;

#[cfg(test)]
mod test_harness;

#[cfg(test)]
use test_harness::{
    reset_counters, DAEMON_DISPATCH, FORCED_CONNECT_ERROR, FORCE_PID_IS_DAEMON,
    FORCE_PID_IS_FOREIGN, KILL_COUNT, RECOVERY_RACE_BARRIER, SIGTERM_COUNT, SPAWN_COUNT,
};

// ── client ────────────────────────────────────────────────────────────────────

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

enum PidIdentity {
    KhiveDaemon,
    Foreign,
    Indeterminate,
}

/// Classify the process named by the daemon PID file from its command line.
/// A live process whose identity cannot be read remains indeterminate so the
/// caller conservatively waits for its exit instead of treating it as stale.
fn classify_pid_identity(pid: u32) -> PidIdentity {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return PidIdentity::Foreign;
    };
    if pid_i32 <= 0 {
        return PidIdentity::Foreign;
    }
    #[cfg(test)]
    if FORCE_PID_IS_FOREIGN.load(std::sync::atomic::Ordering::SeqCst) {
        return PidIdentity::Foreign;
    }
    // Test seam: when FORCE_PID_IS_DAEMON is set, treat any positive live PID
    // as a daemon so the SIGTERM branch is reachable in tests.
    #[cfg(test)]
    if FORCE_PID_IS_DAEMON.load(std::sync::atomic::Ordering::SeqCst) {
        // SAFETY: signal 0 is an existence/permission probe with no side effects.
        return if unsafe { libc::kill(pid_i32, 0) } == 0 {
            PidIdentity::KhiveDaemon
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            PidIdentity::Indeterminate
        } else {
            PidIdentity::Foreign
        };
    }
    // Quick liveness check before shelling out.
    // SAFETY: signal 0 is an existence/permission probe with no side effects.
    if unsafe { libc::kill(pid_i32, 0) } != 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) => PidIdentity::Indeterminate,
            Some(libc::ESRCH) => PidIdentity::Foreign,
            _ => PidIdentity::Indeterminate,
        };
    }
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
    {
        Ok(out) if out.status.success() => {
            let args = String::from_utf8_lossy(&out.stdout);
            let args = args.trim();
            if args.is_empty() {
                PidIdentity::Indeterminate
            } else if argv_is_khive_daemon(args) {
                PidIdentity::KhiveDaemon
            } else {
                PidIdentity::Foreign
            }
        }
        _ => PidIdentity::Indeterminate,
    }
}

const INCUMBENT_EXIT_TIMEOUT_SECS: u64 = 12;
const INCUMBENT_EXIT_POLL_MS: u64 = 25;

#[derive(Debug)]
enum RecoveryError {
    Spawn(std::io::Error),
    IncumbentStillAlive { pid: u32 },
    RequestExpired,
    PidFileDirectoryUntrusted(String),
}

fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 is an existence/permission probe with no side effects.
    let result = unsafe { libc::kill(pid, 0) };
    let exists = result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !exists {
        return false;
    }
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    {
        Ok(output) if output.status.success() => !String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with('Z'),
        _ => true,
    }
}

async fn wait_for_process_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process_is_alive(pid) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(
            std::time::Duration::from_millis(INCUMBENT_EXIT_POLL_MS).min(deadline - now),
        )
        .await;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PidFileSnapshot {
    Missing,
    Present(Vec<u8>),
    Unreadable,
}

impl PidFileSnapshot {
    fn read(path: &std::path::Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => Self::Present(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Missing,
            Err(_) => Self::Unreadable,
        }
    }

    fn pid(&self) -> Option<u32> {
        match self {
            Self::Present(bytes) => std::str::from_utf8(bytes).ok()?.trim().parse().ok(),
            Self::Missing | Self::Unreadable => None,
        }
    }
}

/// Signal under the boot lock, but release it before waiting: the incumbent
/// needs that same lock to finish its own shutdown cleanup.
async fn kill_stale_daemon_inner(
    exit_timeout: std::time::Duration,
    boot_guard: Option<std::fs::File>,
) -> Result<PidFileSnapshot, RecoveryError> {
    #[cfg(test)]
    KILL_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let pid_file = pid_path();
    let expected_snapshot = match pid_file_directory_is_trusted_if_present(&pid_file) {
        Ok(true) => PidFileSnapshot::read(&pid_file),
        Ok(false) => PidFileSnapshot::Missing,
        Err(message) => return Err(RecoveryError::PidFileDirectoryUntrusted(message)),
    };
    let expected_pid = expected_snapshot.pid();

    let wait_for_exit = if let Some(pid) = expected_pid {
        match classify_pid_identity(pid) {
            PidIdentity::KhiveDaemon => {
                if let Ok(signed) = i32::try_from(pid) {
                    if signed > 0 {
                        #[cfg(test)]
                        SIGTERM_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // SAFETY: SIGTERM is a standard termination signal with no
                        // side effects beyond asking the process to exit.
                        unsafe {
                            libc::kill(signed, libc::SIGTERM);
                        }
                    }
                }
                true
            }
            PidIdentity::Foreign => {
                tracing::warn!(
                    pid,
                    "PID in daemon file belongs to a foreign process — treating it as stale"
                );
                false
            }
            PidIdentity::Indeterminate => {
                tracing::warn!(
                    pid,
                    "could not read PID identity — skipping SIGTERM and waiting conservatively"
                );
                true
            }
        }
    } else {
        false
    };
    drop(boot_guard);
    if let Some(pid) = expected_pid {
        if wait_for_exit && !wait_for_process_exit(pid, exit_timeout).await {
            return Err(RecoveryError::IncumbentStillAlive { pid });
        }
    }

    Ok(expected_snapshot)
}

/// Remove `pid_file`/the daemon socket only if ownership has not changed since
/// `expected_snapshot` was observed: the PID file must be unchanged or absent,
/// and connecting to the socket must report absence or connection refusal.
/// Other connect errors leave ownership uncertain and refuse cleanup.
/// Either signal changing means a replacement daemon claimed the rendezvous
/// between the observation and this call, and unlinking would delete its live
/// paths instead of the truly-stale ones (#645).
fn remove_daemon_paths_if_still_stale(
    pid_file: &std::path::Path,
    expected_snapshot: &PidFileSnapshot,
) -> bool {
    let (current_snapshot, pid_directory_trusted) =
        match pid_file_directory_is_trusted_if_present(pid_file) {
            Ok(true) => (PidFileSnapshot::read(pid_file), true),
            Ok(false) => (PidFileSnapshot::Missing, false),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "daemon PID-file directory is not trusted; skipping stale-path cleanup"
                );
                return false;
            }
        };
    if matches!(expected_snapshot, PidFileSnapshot::Unreadable)
        || matches!(current_snapshot, PidFileSnapshot::Unreadable)
    {
        return false;
    }
    // Graceful incumbent cleanup may already have removed its own PID file.
    if current_snapshot != PidFileSnapshot::Missing && &current_snapshot != expected_snapshot {
        tracing::warn!(
            expected_pid = ?expected_snapshot.pid(),
            current_pid = ?current_snapshot.pid(),
            "pid file changed during stale-daemon cleanup — a replacement daemon \
             already claimed it; skipping unlink to avoid deleting its live paths"
        );
        return false;
    }

    let sock = socket_path();
    // A plain blocking connect is enough here: any success means *something*
    // is now listening at this path, which can only be a replacement daemon
    // that bound after our probe found the old one dead. Combined with the
    // PID-file recheck above, this closes the window even when the recovery
    // lock alone did not exclude the replacement's boot.
    match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(_) => {
            tracing::warn!(socket = ?sock, "live listener claimed rendezvous; skipping cleanup and launch");
            return false;
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(_) => return false,
    }

    if pid_directory_trusted {
        if let Err(error) = std::fs::remove_file(pid_file) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return false;
            }
        }
    }
    if let Err(error) = std::fs::remove_file(&sock) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return false;
        }
    }
    true
}

/// Outcome of the under-lock identity probe.
#[derive(Debug)]
enum ProbeOutcome {
    /// A live, identity-matching daemon responded before the deadline.
    Alive,
    /// Daemon is absent, crashed, or identity-mismatched — safe to kill+spawn.
    Dead,
    /// Probe timed out — daemon may be alive but slow; do NOT kill.
    Timeout,
    /// The boot/recovery lock ([`khive_runtime::daemon::lock_path`]) stayed
    /// contended past its bounded acquisition deadline while
    /// [`quiesce_then_probe_identity`] was trying to confirm no peer boot is
    /// in flight (#838). Distinct from `Timeout` (which
    /// means "the daemon itself answered slowly") — this means "could not
    /// even confirm whether a peer boot is still running" — so a caller must
    /// not conflate it with a healthy-but-slow daemon. NEVER-KILL on this
    /// outcome either: an unconfirmed peer boot is exactly the ambiguity
    /// `confirm_genuinely_dead` exists to resolve safely.
    LockContended,
}

/// The kernel identity of a socket holder that returned a decoded khived
/// identity response. A mismatching configuration can still name a daemon;
/// the explicit handover acknowledgement is checked separately. Unlike
/// client recovery, the launcher does not require its own configuration id.
#[cfg(unix)]
#[derive(Debug)]
pub struct SupervisorDaemonPeer {
    pub pid: Option<u32>,
    pub uid: Option<u32>,
    pub protocol_version: u32,
    pub served_config_id: String,
    pub supervisor_claim: Option<String>,
    pub handover_accepted: bool,
}

#[cfg(unix)]
#[derive(Debug)]
pub enum SupervisorSocketProbe {
    Absent,
    Daemon(SupervisorDaemonPeer),
    Unidentified {
        pid: Option<u32>,
        uid: Option<u32>,
        reason: &'static str,
    },
}

/// Probe the launcher rendezvous without trusting a PID file or demanding an
/// identity match. Only a decoded daemon frame that explicitly reports its
/// protocol and served configuration can identify the connected holder.
/// Connect, write, and read share one timeout; an accepting silent/foreign
/// socket is never classified as absent.
#[cfg(unix)]
pub async fn probe_supervisor_socket(timeout: std::time::Duration) -> SupervisorSocketProbe {
    probe_supervisor_socket_inner(timeout, false).await
}

/// Keep the existing socket-owner refusal ahead of a contended store claim.
/// The caller holds the HOME boot lock; this read-only probe cannot open SQLite
/// or disturb the incumbent's rendezvous. Unknown peers still reach the normal
/// boot fence after the store claims have been acquired.
#[cfg(unix)]
pub async fn refuse_serving_socket_before_store_claim() -> anyhow::Result<()> {
    if let SupervisorSocketProbe::Daemon(peer) =
        probe_supervisor_socket(std::time::Duration::from_millis(500)).await
    {
        let holder = peer
            .pid
            .map_or_else(|| "unknown pid".to_string(), |pid| format!("pid {pid}"));
        anyhow::bail!(
            "refusing to start: a khived instance is already serving this socket {} ({holder})",
            socket_path().display()
        );
    }
    Ok(())
}

/// Ask the process on the connected socket to enter its own SIGTERM shutdown
/// path. The peer receives this on the same connection that supplies its
/// credentials and identity response, so no later numeric PID signal is sent.
#[cfg(unix)]
pub async fn request_supervisor_handover(timeout: std::time::Duration) -> SupervisorSocketProbe {
    probe_supervisor_socket_inner(timeout, true).await
}

#[cfg(unix)]
async fn probe_supervisor_socket_inner(
    timeout: std::time::Duration,
    handover: bool,
) -> SupervisorSocketProbe {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut stream =
        match tokio::time::timeout_at(deadline, UnixStream::connect(socket_path())).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return SupervisorSocketProbe::Absent;
            }
            Err(_) => {
                return SupervisorSocketProbe::Unidentified {
                    pid: None,
                    uid: None,
                    reason: "socket connection timed out",
                };
            }
            Ok(Err(_)) => {
                return SupervisorSocketProbe::Unidentified {
                    pid: None,
                    uid: None,
                    reason: "socket connection failed",
                };
            }
        };
    let credentials = stream.peer_cred().ok();
    let pid = credentials
        .as_ref()
        .and_then(|credentials| credentials.pid())
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    let uid = credentials.as_ref().map(|credentials| credentials.uid());
    if handover && (pid.is_none() || uid != Some(supervisor_effective_uid())) {
        return SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "handover requires a same-uid peer with a kernel pid",
        };
    }
    let probe = DaemonRequestFrame {
        probe_only: true,
        protocol_version: PROTOCOL_VERSION,
        // A mismatch is expected and useful: the answer still reports the
        // incumbent's real served configuration, without dispatching a verb.
        config_id: String::new(),
        ..Default::default()
    };
    let Ok(mut probe) = serde_json::to_value(&probe) else {
        return SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "identity probe could not be encoded",
        };
    };
    if handover {
        probe["supervisor_handover"] = serde_json::Value::Bool(true);
    }
    let Ok(payload) = serde_json::to_vec(&probe) else {
        return SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "identity probe could not be encoded",
        };
    };
    if !matches!(
        tokio::time::timeout_at(deadline, write_frame(&mut stream, &payload)).await,
        Ok(Ok(()))
    ) {
        return SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "identity probe write failed",
        };
    }
    let raw = match tokio::time::timeout_at(deadline, read_frame(&mut stream)).await {
        Ok(Ok(raw)) => raw,
        _ => {
            return SupervisorSocketProbe::Unidentified {
                pid,
                uid,
                reason: "identity probe received no daemon response",
            };
        }
    };
    let decoded: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(decoded) => decoded,
        Err(_) => {
            return SupervisorSocketProbe::Unidentified {
                pid,
                uid,
                reason: "identity probe response did not decode",
            };
        }
    };
    let reported_protocol = decoded
        .get("daemon_protocol_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok());
    let reported_config = decoded
        .get("served_config_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let supervisor_claim = decoded
        .get("supervisor_claim")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let handover_accepted = decoded
        .get("supervisor_handover_accepted")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let frame = serde_json::from_value::<DaemonResponseFrame>(decoded);
    match (frame, reported_protocol, reported_config) {
        (Ok(_frame), Some(protocol_version), Some(served_config_id)) => {
            SupervisorSocketProbe::Daemon(SupervisorDaemonPeer {
                pid,
                uid,
                protocol_version,
                served_config_id,
                supervisor_claim,
                handover_accepted,
            })
        }
        _ => SupervisorSocketProbe::Unidentified {
            pid,
            uid,
            reason: "identity probe response omitted daemon identity",
        },
    }
}

#[cfg(unix)]
pub fn supervisor_pid_is_alive(pid: u32) -> bool {
    process_is_alive(pid)
}

#[cfg(unix)]
pub fn supervisor_effective_uid() -> u32 {
    // SAFETY: `geteuid` reads the process's kernel credential.
    unsafe { libc::geteuid() }
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
        plan: false,
        ops: String::new(),
        presentation: None,
        presentation_per_op: None,
        namespace: namespace.to_string(),
        // A probe never reaches the identity-context / dispatch arm (it
        // short-circuits on `probe_only` right after the protocol/config_id
        // checks), so no per-request identity is meaningful here.
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: true,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let deadline = std::time::Duration::from_millis(timeout_ms);
    let probe_deadline = tokio::time::Instant::now() + deadline;
    let exchange = tokio::time::timeout_at(
        probe_deadline,
        try_forward_before(&probe, Some(probe_deadline)),
    )
    .await;
    // The inner connect/read timeout and this outer probe timeout can become
    // ready in the same scheduler turn. A timed-out inner NoSocket/ParseFailure
    // is uncertainty about a slow peer, never evidence that it is dead.
    if tokio::time::Instant::now() >= probe_deadline
        && !matches!(&exchange, Ok(ForwardOutcome::Response(_)))
    {
        return ProbeOutcome::Timeout;
    }
    match exchange {
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
                && resp.served_config_id.as_deref().is_some_and(|served| {
                    khive_runtime::daemon::config_ids_compatible(config_id, served)
                })
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
            | ForwardOutcome::ResponseLost
            | ForwardOutcome::ProtocolMismatch { .. },
        ) => ProbeOutcome::Dead,
        Ok(ForwardOutcome::RequestTooLarge { .. }) => ProbeOutcome::Timeout,
        Ok(ForwardOutcome::Unreachable {
            kind,
            os_error_code,
        }) => {
            tracing::debug!(
                ?kind,
                ?os_error_code,
                "under-lock probe could not reach the daemon socket; treating its state as \
                 uncertain and suppressing lifecycle recovery"
            );
            ProbeOutcome::Timeout
        }
    }
}

/// Launch seam for daemon recovery.
///
/// Production closures return a real [`std::process::Child`]. The test harness
/// supplies an in-process handle around the real [`daemon::run_daemon`] server,
/// allowing parallel recovery to assert server convergence without forking the
/// test binary with synthetic CLI arguments (#539/#544).
trait DaemonLauncher: Sync {
    type Handle: std::fmt::Debug;

    fn launch(&self) -> std::io::Result<Self::Handle>;
}

struct ProcessDaemonLauncher<'a, F>(&'a F);

impl<F> DaemonLauncher for ProcessDaemonLauncher<'_, F>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    type Handle = std::process::Child;

    fn launch(&self) -> std::io::Result<Self::Handle> {
        (self.0)()
    }
}

/// Outcome returned by [`kill_and_respawn`] to the call site.
#[derive(Debug)]
enum RecoveryOutcome<H = std::process::Child> {
    /// A concurrent client already replaced the daemon; forward the real request
    /// via the normal path (no new spawn occurred).
    Skipped,
    /// This client killed the stale daemon and spawned a replacement; caller
    /// must wait for readiness then forward the real request. The production
    /// launcher returns a [`std::process::Child`] (#898), so
    /// `forward_or_spawn` can, once it is otherwise about to give up and fall
    /// back locally, positively confirm whether that specific respawn attempt
    /// already exited instead of ever binding the socket. The test launcher
    /// carries an in-process task handle with the same ownership role.
    Spawned(H),
    /// Could not obtain a positive confirmation either way within the
    /// deadline-bound recovery window (the recoverer lock or the boot/recovery
    /// lock stayed contended past its deadline) — #838. The
    /// caller's behavior is identical to `Skipped` (never kill on an
    /// unconfirmed state), but this is reported as a distinct variant rather
    /// than silently folded into `Skipped`, so logs/metrics do not conflate
    /// "positively confirmed alive" with "gave up without confirming".
    Uncertain,
}

/// Bounded number of quiescence-confirm rounds [`confirm_genuinely_dead`]
/// performs before trusting a `Dead` classification enough to kill+spawn.
const DEAD_CONFIRM_ROUNDS: u32 = 4;

/// Pacing between [`confirm_genuinely_dead`] rounds. Not a synchronization
/// mechanism by itself — the real synchronization is the bounded `flock` in
/// [`quiesce_then_probe_identity`]; this only avoids busy-spinning while
/// waiting for a peer that has not yet reached its own boot-guard call.
const DEAD_CONFIRM_POLL_MS: u64 = 75;

/// Deadline for each round's bounded wait on the boot/recovery lock inside
/// [`quiesce_then_probe_identity`]. #838: the previous
/// unbounded blocking `flock` meant `DEAD_CONFIRM_ROUNDS` bounded probe
/// *count*, not elapsed *time* — a wedged lock holder blocked recovery
/// forever. Bounding each round's lock wait makes the whole
/// `confirm_genuinely_dead` call bounded by
/// `DEAD_CONFIRM_ROUNDS * (BOOT_QUIESCENCE_LOCK_TIMEOUT_MS +
/// BOOT_FENCE_PROBE_TIMEOUT_MS + DEAD_CONFIRM_POLL_MS)` in the worst case.
const BOOT_QUIESCENCE_LOCK_TIMEOUT_MS: u64 = 500;

/// Block until no concurrent boot holds the shared boot/recovery lock (or the
/// bounded wait's deadline elapses), then re-probe daemon identity — reused
/// by [`confirm_genuinely_dead`] (#758). Deadline-bounded (unlike an
/// unbounded `flock`), so a wedged lock holder cannot block confirmation
/// rounds forever; a contended/failed acquisition returns the distinct
/// [`ProbeOutcome::LockContended`], not `Timeout` (#838). See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
async fn quiesce_then_probe_identity(
    config_id: &str,
    namespace: &str,
    timeout_ms: u64,
) -> ProbeOutcome {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(BOOT_QUIESCENCE_LOCK_TIMEOUT_MS);
    match tokio::task::spawn_blocking(move || {
        khive_runtime::daemon::try_acquire_daemon_boot_guard_until(deadline)
    })
    .await
    {
        Ok(Ok(Some(guard))) => drop(guard),
        Ok(Ok(None)) => {
            tracing::debug!(
                BOOT_QUIESCENCE_LOCK_TIMEOUT_MS,
                "boot/recovery lock still contended past its bounded wait; \
                 could not confirm quiescence this round"
            );
            return ProbeOutcome::LockContended;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to probe boot/recovery lock state");
            return ProbeOutcome::LockContended;
        }
        Err(_join_err) => return ProbeOutcome::LockContended,
    }
    probe_daemon_identity(config_id, namespace, timeout_ms).await
}

/// Confirm a `Dead` probe result is not racing a peer's in-flight
/// `kill_and_respawn` or the daemon's own cold boot (#758) — closes the
/// fork-to-flock gap where `spawn_daemon()`'s child exists but has not yet
/// reached its own `acquire_daemon_boot_guard()` call. Retries
/// [`quiesce_then_probe_identity`] up to [`DEAD_CONFIRM_ROUNDS`] times,
/// paced by [`DEAD_CONFIRM_POLL_MS`]; returns as soon as a peer's boot is
/// observed completing (`Alive`) or going slow (`Timeout`, NEVER-KILL-SLOW).
/// Only `Dead` once every round agrees. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
async fn confirm_genuinely_dead(config_id: &str, namespace: &str) -> ProbeOutcome {
    confirm_genuinely_dead_with_round_observer(config_id, namespace, |_, _| {}).await
}

async fn confirm_genuinely_dead_with_round_observer(
    config_id: &str,
    namespace: &str,
    mut observe_round: impl FnMut(u32, &ProbeOutcome),
) -> ProbeOutcome {
    // `LockContended` means "still could not confirm this round" — the same
    // "keep polling" shape as `Dead`, not a terminal state like `Alive`/
    // `Timeout`. A peer's boot can legitimately hold the lock across several
    // rounds; only give up (return `LockContended`) once every round agreed
    // nobody could confirm, exactly mirroring how `Dead` only becomes trusted
    // once every round agreed the daemon was absent.
    //
    // #838: `LockContended` is STICKY across rounds — once
    // any round can't confirm quiescence, the aggregate must never collapse
    // back to `Dead` just because a LATER round happened to observe it. The
    // old code tracked only the last round's outcome, so a
    // LockContended-then-Dead sequence overwrote the earlier contention and
    // returned `Dead`, permitting kill+spawn on a call that never actually
    // established quiescence across every round. `Dead` is only trustworthy
    // when EVERY round agrees; a single contended round makes the whole call
    // `LockContended` regardless of what any other round returned.
    let mut saw_contention = false;
    for round in 0..DEAD_CONFIRM_ROUNDS {
        let outcome =
            quiesce_then_probe_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await;
        observe_round(round, &outcome);
        match outcome {
            ProbeOutcome::Dead => {}
            ProbeOutcome::LockContended => saw_contention = true,
            other => return other,
        }
        if round + 1 < DEAD_CONFIRM_ROUNDS {
            tokio::time::sleep(std::time::Duration::from_millis(DEAD_CONFIRM_POLL_MS)).await;
        }
    }
    if saw_contention {
        ProbeOutcome::LockContended
    } else {
        ProbeOutcome::Dead
    }
}

/// Deadline for acquiring the recoverer-only lock before starting the
/// dead-confirmation → kill → confirmed-exit → spawn critical section.
/// This exceeds the incumbent-exit deadline so a peer can finish a full
/// recovery turn before another recoverer treats the lock as wedged. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
const RECOVERER_LOCK_TIMEOUT_MS: u64 = 16_000;

/// Kill the stale daemon and spawn a fresh one, serialized against concurrent
/// recoverers by a dedicated recoverer-only lock (#838 double-checked
/// recovery). Outcomes: `Alive`/`Timeout` → `Skipped`, no kill
/// (NEVER-KILL-SLOW). `LockContended` (confirm rounds inconclusive, or the
/// recoverer lock itself timed out) → `Uncertain`, no kill — same safe
/// behavior as `Skipped` but reported distinctly. `Dead` (confirmed,
/// recoverer lock held) → signal + bounded exit confirmation + spawn →
/// `Spawned`; a PID still alive at the deadline returns
/// [`RecoveryError::IncumbentStillAlive`] without spawning. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md` for why a second lock
/// file is required and how this avoids deadlocking a booting daemon.
async fn kill_and_respawn<F>(
    config_id: &str,
    namespace: &str,
    spawn: &F,
) -> Result<RecoveryOutcome, RecoveryError>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    let launcher = ProcessDaemonLauncher(spawn);
    kill_and_respawn_with_launcher(config_id, namespace, &launcher).await
}

/// Test-only shim over [`kill_and_respawn_with_launcher_and_exit_timeout`]:
/// same production code path, with only the launcher construction and the
/// exit-timeout parameter (production passes its own constant through
/// [`kill_and_respawn`]) supplied by the test. Keep it a pure forwarder —
/// any logic added here would drift from the non-test path.
#[cfg(test)]
async fn kill_and_respawn_with_exit_timeout<F>(
    config_id: &str,
    namespace: &str,
    spawn: &F,
    exit_timeout: std::time::Duration,
) -> Result<RecoveryOutcome, RecoveryError>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    let launcher = ProcessDaemonLauncher(spawn);
    kill_and_respawn_with_launcher_and_exit_timeout(config_id, namespace, &launcher, exit_timeout)
        .await
}

async fn kill_and_respawn_with_launcher<L>(
    config_id: &str,
    namespace: &str,
    launcher: &L,
) -> Result<RecoveryOutcome<L::Handle>, RecoveryError>
where
    L: DaemonLauncher,
{
    kill_and_respawn_with_launcher_and_exit_timeout(
        config_id,
        namespace,
        launcher,
        std::time::Duration::from_secs(INCUMBENT_EXIT_TIMEOUT_SECS),
    )
    .await
}

async fn kill_and_respawn_with_launcher_and_exit_timeout<L>(
    config_id: &str,
    namespace: &str,
    launcher: &L,
    exit_timeout: std::time::Duration,
) -> Result<RecoveryOutcome<L::Handle>, RecoveryError>
where
    L: DaemonLauncher,
{
    let initial_probe = {
        let _lock = acquire_recovery_lock();
        probe_daemon_identity(config_id, namespace, 500).await
    };
    match initial_probe {
        ProbeOutcome::Alive | ProbeOutcome::Timeout | ProbeOutcome::LockContended => {
            return Ok(RecoveryOutcome::Skipped);
        }
        ProbeOutcome::Dead => {}
    }

    // Test-only rendezvous (see `RECOVERY_RACE_BARRIER`): forces every
    // concurrent recoverer under test to reach "independently classified
    // Dead" at the same instant, so the recoverer lock below is what
    // actually determines mutual exclusion rather than scheduling order.
    #[cfg(test)]
    {
        let barrier = RECOVERY_RACE_BARRIER
            .lock()
            .expect("barrier mutex poisoned")
            .clone();
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
    }

    let recoverer_deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(RECOVERER_LOCK_TIMEOUT_MS);
    let recoverer_guard = match tokio::time::timeout(
        std::time::Duration::from_millis(RECOVERER_LOCK_TIMEOUT_MS),
        tokio::task::spawn_blocking(move || {
            khive_runtime::daemon::try_acquire_recoverer_lock_until(recoverer_deadline)
        }),
    )
    .await
    {
        Ok(Ok(Ok(Some(guard)))) => guard,
        Ok(Ok(Ok(None))) => {
            tracing::warn!(
                RECOVERER_LOCK_TIMEOUT_MS,
                "recoverer lock still contended past its deadline; a peer recoverer \
                 is likely still mid dead-confirmation/kill/spawn — skipping without \
                 a positive confirmation rather than risking a double-spawn"
            );
            return Ok(RecoveryOutcome::Uncertain);
        }
        Ok(Ok(Err(e))) => {
            tracing::warn!(error = %e, "failed to acquire recoverer lock");
            return Ok(RecoveryOutcome::Uncertain);
        }
        Ok(Err(_)) | Err(_) => {
            tracing::warn!("recoverer lock acquisition task failed or exceeded its deadline");
            return Ok(RecoveryOutcome::Uncertain);
        }
    };

    // The bridge may have stopped waiting while this task waited for a peer
    // recoverer. Its detached task must not now classify or kill a daemon.
    if khive_storage::request_read_is_cancelled() {
        return Err(RecoveryError::RequestExpired);
    }

    let outcome = match confirm_genuinely_dead(config_id, namespace).await {
        ProbeOutcome::Alive | ProbeOutcome::Timeout => Ok(RecoveryOutcome::Skipped),
        ProbeOutcome::LockContended => {
            tracing::warn!(
                "confirm_genuinely_dead could not establish quiescence within its \
                 bounded rounds; skipping kill+spawn without a positive confirmation"
            );
            Ok(RecoveryOutcome::Uncertain)
        }
        ProbeOutcome::Dead => {
            let boot_lock = acquire_recovery_lock();
            match probe_daemon_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await {
                ProbeOutcome::Alive | ProbeOutcome::Timeout => return Ok(RecoveryOutcome::Skipped),
                ProbeOutcome::LockContended => return Ok(RecoveryOutcome::Uncertain),
                ProbeOutcome::Dead => {}
            }
            if khive_storage::request_read_is_cancelled() {
                return Err(RecoveryError::RequestExpired);
            }
            let expected_snapshot = kill_stale_daemon_inner(exit_timeout, boot_lock).await?;
            // Keep the recoverer-only lock throughout, but let graceful exit
            // take the boot lock. An independently started successor can win
            // while we wait, so recheck under the boot lock before unlink/spawn.
            let _boot_lock = acquire_recovery_lock();
            match probe_daemon_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await {
                ProbeOutcome::Alive | ProbeOutcome::Timeout => Ok(RecoveryOutcome::Skipped),
                ProbeOutcome::LockContended => Ok(RecoveryOutcome::Uncertain),
                ProbeOutcome::Dead => {
                    if !remove_daemon_paths_if_still_stale(&pid_path(), &expected_snapshot) {
                        return Ok(RecoveryOutcome::Uncertain);
                    }
                    launcher
                        .launch()
                        .map(RecoveryOutcome::Spawned)
                        .map_err(RecoveryError::Spawn)
                }
            }
        }
    };
    drop(recoverer_guard);
    outcome
}

/// Build the hard error returned when the real request frame was fully
/// written to the daemon socket but no trustworthy response came back.
///
/// #644: once `write_frame` has completed for a real (non-probe) frame, the
/// daemon may already be dispatching or have finished dispatching it. There is
/// no way from the client side to tell "never received" apart from "received,
/// executed, response lost" — so this case must never be retried (against the
/// same daemon or a freshly-spawned one) and must never silently fall back to
/// local dispatch, either of which could execute a mutation a second time.
fn ambiguous_forward_error() -> McpError {
    daemon_mcp_error(
        "daemon response lost after request was sent; not retrying or locally \
         dispatching to avoid duplicate execution",
        None,
    )
}

fn daemon_unreachable_error(
    frame: &DaemonRequestFrame,
    kind: std::io::ErrorKind,
    os_error_code: Option<i32>,
) -> McpError {
    let config_id = opaque_config_id(&frame.config_id);
    tracing::error!(
        reason = "daemon_unreachable",
        config_id = %config_id,
        namespace = %frame.namespace,
        ?kind,
        ?os_error_code,
        "daemon socket is unreachable from this process; lifecycle recovery suppressed"
    );
    daemon_mcp_error(
        "cannot reach daemon socket from this process; refusing daemon lifecycle recovery \
         because the daemon may still be healthy",
        Some(serde_json::json!({
            "reason": "daemon_unreachable",
            "os_error_kind": format!("{kind:?}"),
            "os_error_code": os_error_code,
        })),
    )
}

// ── #898: loud, unambiguous respawn-failure error ───────────────────────────
//
// Root cause (2026-07-12 incident): `spawn_daemon` already resolves the spawn
// target deterministically via `std::env::current_exe()` (never ambient
// `PATH`), so a version-skewed binary reaching `mcp --daemon` means THIS
// process's own on-disk binary predates (or otherwise rejects) that flag —
// respawning via `current_exe()` faithfully relaunches the very same stale
// binary. That relaunch fails immediately with a clap parse error
// (`error: Unrecognized option: 'daemon'`) written only to `khived.log`.
// Because `spawn_daemon` was fire-and-forget (the `Child` was discarded) and
// `forward_or_spawn` cannot distinguish "our own respawn attempt definitely
// already died" from "no daemon is configured to run here at all", every
// request repeated the same failing respawn, burned the full connect
// deadline plus the boot-quiescence wait, and then quietly completed via
// local dispatch (or, in `KHIVE_DAEMON_STRICT=1`, rejected with the generic
// `no_socket` reason) — a silent, forever-repeating failure invisible to the
// caller and to every metric except a `khived.log` grep.
//
// The fix: `spawn_daemon` now returns the live `Child` (see
// `RecoveryOutcome::Spawned`), and `forward_or_spawn` checks — only at the
// point it would otherwise fall back locally, never earlier, so the existing
// connect-retry window and #667's boot-quiescence fence are unchanged —
// whether the respawn attempt IT made has already exited. A confirmed exit is
// unambiguous (this process spawned that exact child); it is never treated as
// the legitimate ADR-049 no-daemon case and never silently swallowed, in
// either strict or non-strict mode.

#[derive(Clone, Copy)]
enum RespawnFailure {
    SpawnError { os_error_code: Option<i32> },
    ExitedBeforeBind { exit_code: Option<i32> },
}

/// Build the caller-visible error for a respawn attempt this process made and
/// can now positively confirm failed. Both the caller error and bridge tracing
/// expose only stable classifications and non-sensitive numeric status codes.
/// The error is returned regardless of
/// `KHIVE_DAEMON_STRICT`: unlike the ordinary "no daemon reachable" fallback
/// (which may be the legitimate ADR-049 no-daemon deployment), a respawn WE
/// attempted and can prove failed is never a case for quietly completing the
/// request via local dispatch. See #898.
fn respawn_failed_error(failure: RespawnFailure) -> McpError {
    match failure {
        RespawnFailure::SpawnError { os_error_code } => tracing::error!(
            reason = "respawn_failed",
            failure_category = "spawn_error",
            ?os_error_code,
            "daemon respawn attempt confirmed failed"
        ),
        RespawnFailure::ExitedBeforeBind { exit_code } => tracing::error!(
            reason = "respawn_failed",
            failure_category = "exited_before_bind",
            ?exit_code,
            "daemon respawn attempt confirmed failed"
        ),
    }
    // Under strict mode this is also a pre-dispatch rejection, so preserve
    // #947's request-envelope contract by tagging it for `server::request`.
    // Non-strict callers still receive the raw, loud MCP error introduced by
    // #898; in neither mode may the request fall through to local dispatch.
    let data = if is_daemon_strict_mode() {
        serde_json::json!({
            STRICT_FALLBACK_MARKER: true,
            "reason": "respawn_failed",
        })
    } else {
        serde_json::json!({"reason": "respawn_failed"})
    };
    daemon_mcp_error(
        "daemon respawn failed (respawn_failed); rebuild with `make local` and retry",
        Some(data),
    )
}

fn incumbent_still_alive_error(pid: u32) -> McpError {
    tracing::error!(
        reason = "incumbent_still_alive",
        pid,
        "daemon recovery refused because the incumbent did not exit before the deadline"
    );
    let mut data = serde_json::json!({
        "reason": "incumbent_still_alive",
        "pid": pid,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(
        format!("daemon recovery refused: incumbent PID {pid} is still alive after the deadline"),
        Some(data),
    )
}

fn untrusted_pid_file_directory_error(message: String) -> McpError {
    tracing::error!(
        reason = "untrusted_pid_file_directory",
        error = %message,
        "daemon recovery refused because its PID-file directory is not trusted"
    );
    let mut data = serde_json::json!({
        "reason": "untrusted_pid_file_directory",
        "error": message,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(format!("daemon recovery refused: {message}"), Some(data))
}

/// A socket-less rendezvous claimed by a supervisor returns a retryable error
/// when the caller's startup budget ends. It never falls through to local
/// dispatch or spawns a competing daemon while the claim is held.
fn supervised_daemon_error(marker: &SupervisorMarker) -> McpError {
    let mut data = serde_json::json!({
        "reason": "supervised_daemon_starting",
        "retryable": true,
        "job": marker.job,
        "pid": marker.pid,
        "pid_alive": marker.pid_is_alive(),
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(
        format!(
            "supervised daemon starting: job \"{}\" (pid {}) has not bound the socket; \
             the request deadline expired or the request was cancelled before dispatch; retry",
            marker.job, marker.pid
        ),
        Some(data),
    )
}

/// No request bytes were sent while another launcher or client held the
/// marker lock. Retrying cannot duplicate dispatch.
fn supervisor_marker_lock_wait_error() -> McpError {
    let mut data = serde_json::json!({
        "reason": "supervised_daemon_starting",
        "retryable": true,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(
        "daemon startup ownership is in progress; the marker lock wait ended before dispatch; retry",
        Some(data),
    )
}

fn daemon_reconnect_expired_before_dispatch_error() -> McpError {
    daemon_mcp_error(
        "daemon reconnect deadline expired or request cancelled before dispatch; no lifecycle recovery attempted",
        Some(serde_json::json!({"reason": "daemon_reconnect_expired"})),
    )
}

/// Bounded probe timeout used by [`wait_for_boot_quiescence_then_reprobe`],
/// matching the 500ms bound already used by the identity probe inside
/// [`kill_and_respawn`].
const BOOT_FENCE_PROBE_TIMEOUT_MS: u64 = 500;

/// Outcome of waiting for a concurrent cold-boot (migrations + pack schema
/// plans / FTS DDL) to finish, then re-checking daemon liveness once quiescent.
enum BootFenceOutcome {
    /// Boot quiesced and a live, identity-matching daemon answered — keep
    /// sending the real frame to it.
    DaemonReady,
    /// Boot quiesced and the probe definitively found no daemon — safe to
    /// fall back to local dispatch.
    SafeLocalFallback,
    /// Daemon state is unknown (lock/join failure, or the post-quiescence
    /// probe itself timed out) — must not local-dispatch.
    HardError(McpError),
}

/// #667: the readiness-timeout branch of `forward_or_spawn`'s send loop used
/// to return `None` (silent local fallback) purely because a freshly spawned
/// daemon had not answered within the fixed deadline — including while that
/// daemon was still inside its boot guard running migrations/pack schema
/// plans (FTS DDL). A local writer/searcher racing in at exactly that moment
/// could observe or create a partially-initialized `notes`/`fts_notes` schema.
///
/// This blocks on the SAME boot guard the daemon holds across cold-boot
/// schema init (ADR-D3): acquiring it here can only succeed once no boot is
/// in progress, so acquiring-then-immediately-dropping it is a pure
/// quiescence wait. Only after that wait does it re-probe daemon identity —
/// distinguishing "was still booting, now ready" from "genuinely no daemon"
/// so the caller never has to guess which one caused the original timeout.
async fn wait_for_boot_quiescence_then_reprobe(frame: &DaemonRequestFrame) -> BootFenceOutcome {
    // `acquire_daemon_boot_guard` performs a blocking `flock`; run it on the
    // blocking pool rather than the async executor.
    let quiesced =
        tokio::task::spawn_blocking(khive_runtime::daemon::acquire_daemon_boot_guard).await;
    match quiesced {
        Ok(Ok(guard)) => {
            // The guard's only purpose here is proving quiescence; drop it
            // immediately so it does not itself block a real boot or the
            // re-probe below.
            drop(guard);
        }
        Ok(Err(e)) => {
            return BootFenceOutcome::HardError(daemon_mcp_error(
                format!(
                    "failed to acquire daemon boot/recovery lock while waiting for \
                     cold-boot quiescence: {e}"
                ),
                None,
            ));
        }
        Err(e) => {
            return BootFenceOutcome::HardError(daemon_mcp_error(
                format!("boot-quiescence wait task failed: {e}"),
                None,
            ));
        }
    }

    match probe_daemon_identity(
        &frame.config_id,
        &frame.namespace,
        BOOT_FENCE_PROBE_TIMEOUT_MS,
    )
    .await
    {
        ProbeOutcome::Alive => BootFenceOutcome::DaemonReady,
        ProbeOutcome::Dead => BootFenceOutcome::SafeLocalFallback,
        ProbeOutcome::Timeout => BootFenceOutcome::HardError(daemon_mcp_error(
            "daemon state uncertain after cold-boot quiescence; not falling back to \
             local dispatch to avoid racing a possibly still-initializing index",
            None,
        )),
        // `probe_daemon_identity` (unlike `quiesce_then_probe_identity`) never
        // constructs `LockContended` — it has no lock-acquisition step of its
        // own. Handled here only for match exhaustiveness over the shared
        // `ProbeOutcome` type; same fail-safe HardError as `Timeout` if it
        // were ever reached.
        ProbeOutcome::LockContended => BootFenceOutcome::HardError(daemon_mcp_error(
            "daemon state uncertain after cold-boot quiescence (lock probe unexpectedly \
             contended); not falling back to local dispatch",
            None,
        )),
    }
}

/// Forward a request to the daemon, auto-spawning it if absent.
///
/// Returns `None` only when nothing was ever written to the daemon and local
/// dispatch is therefore safe: `KHIVE_NO_DAEMON` is set, or the socket is
/// definitively absent/refused (`NoSocket`) — never when connection access is
/// denied (`Unreachable`) and never after the real frame has been written.
/// `Some(Ok)` / `Some(Err)` both mean the caller must not dispatch locally.
/// Under `KHIVE_DAEMON_STRICT=1` the `NoSocket` case instead becomes
/// `Some(Err(..))` (`KHIVE_NO_DAEMON` is unaffected — it is an explicit caller
/// opt-out, not a fallback).
///
/// This conservative entry point never replays a fully written frame.
/// The CLI/MCP policy-aware entry points additionally permit one classified
/// read replay after EOF/reset. A `NoSocket` outcome never writes anything,
/// so it is safe to recover the daemon and retry. Once the real frame IS
/// fully written (`ParseFailure`/`ProtocolMismatch`), this returns a hard
/// error immediately instead of killing/respawning/retrying or falling back
/// locally (#644). See `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
pub async fn forward_or_spawn(frame: &DaemonRequestFrame) -> Option<Result<String, McpError>> {
    forward_or_spawn_with(frame, &spawn_daemon).await
}

/// Forward a request while preserving an explicit config selection (and an
/// ephemeral `:memory:` database override) on a daemon this call may need to
/// spawn.
///
/// An already-running daemon is still matched exclusively by `config_id`.
/// `config` and `db` only supply construction inputs for a missing daemon;
/// without them, `kkernel exec --config <path>` would spawn `kkernel mcp
/// --daemon` against automatic discovery and immediately disagree with the
/// request it was spawned to serve, and `kkernel exec --db :memory:` would
/// bind the fresh daemon to the config's declared persistent files.
///
/// `packs`, when given, is the caller's already-resolved pack list (whatever
/// mix of CLI `--pack`, `KHIVE_PACKS`, or config-file `[runtime].packs`
/// produced it) forwarded verbatim as explicit `--pack` args on the spawned
/// daemon — see the doc comment on `spawn_daemon_with_exe_and_config`.
pub async fn forward_or_spawn_with_config_and_packs(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
) -> Option<Result<String, McpError>> {
    let replay_read_only = !frame.probe_only
        && !frame.metrics_only
        && !frame.plan
        && crate::request_policy::read_replay_safe(&frame.ops);
    forward_or_spawn_with_replay_policy(frame, config, db, packs, replay_read_only).await
}

pub(crate) async fn forward_or_spawn_with_replay_policy(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
    replay_read_only: bool,
) -> Option<Result<String, McpError>> {
    #[cfg(any(test, feature = "test-forward-seam"))]
    if let Some(intercepted) = test_forward_seam::intercept(frame, packs) {
        return intercepted;
    }
    let spawn = || {
        let exe = std::env::current_exe()?;
        spawn_daemon_with_exe_and_config(&exe, config, db, packs)
    };
    forward_or_spawn_with_policy(frame, &spawn, replay_read_only).await
}

/// Forward a request, spawning the daemon if needed, without an explicit
/// pack list: the spawned daemon falls back to its own pack resolution
/// (config-file `[runtime].packs` or the built-in default set), exactly as
/// it did before pack forwarding existed.
///
/// Thin compatibility wrapper over
/// [`forward_or_spawn_with_config_and_packs`] preserving the pre-existing
/// three-argument public signature, so external callers of the published
/// crate keep compiling unchanged.
pub async fn forward_or_spawn_with_config(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
) -> Option<Result<String, McpError>> {
    forward_or_spawn_with_config_and_packs(frame, config, db, None).await
}

/// Test-only capture hook armed at the real entry of
/// `forward_or_spawn_with_config_and_packs` — the actual adapter-boundary conversion
/// site (`packs.as_deref()` at each production call site) production code
/// runs through, as opposed to a `ForwardFnPtr` spy that stands in for the
/// whole adapter and never executes its conversion. Unarmed, `intercept`
/// costs one thread-local read and returns `None` immediately: the
/// production path is byte-for-byte unaffected.
///
/// Gated by `cfg(any(test, feature = "test-forward-seam"))` so both the
/// in-crate `khive-mcp` test suite (plain `cfg(test)`) and a cross-crate
/// `kkernel` test (which cannot see `khive-mcp`'s `cfg(test)` items, so it
/// enables the `test-forward-seam` feature via a `[dev-dependencies]`
/// re-declaration instead) can reach it.
#[cfg(any(test, feature = "test-forward-seam"))]
pub mod test_forward_seam {
    use super::{DaemonRequestFrame, McpError};
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static CAPTURED: std::cell::RefCell<Option<Option<Vec<String>>>> =
            const { std::cell::RefCell::new(None) };
        static CAPTURED_REQUEST_ID: std::cell::RefCell<Option<Option<u64>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Arm the one-shot hook. The next call into
    /// `forward_or_spawn_with_config_and_packs` on this thread records its `packs`
    /// argument and returns a canned success without touching a socket or
    /// spawning a process; the hook then disarms itself.
    pub fn arm() {
        CAPTURED.with(|c| *c.borrow_mut() = None);
        CAPTURED_REQUEST_ID.with(|c| *c.borrow_mut() = None);
        ARMED.with(|a| a.set(true));
    }

    /// Take the captured `packs` argument from the most recent armed call,
    /// if any, and disarm the hook. Disarming here means a hook armed for a
    /// call that never reached `intercept` cannot survive to intercept a
    /// later, unrelated call on the same thread.
    pub fn take_captured() -> Option<Option<Vec<String>>> {
        ARMED.with(|a| a.set(false));
        CAPTURED.with(|c| c.borrow_mut().take())
    }

    /// Take the captured `frame.request_id` of the most recent armed call,
    /// if any. Companion to `take_captured` for tests proving the bridge
    /// correlation id (khive#948, khive-oss#2337) reaches the real
    /// `forward_or_spawn_with_config_and_packs` adapter boundary unchanged.
    pub fn take_captured_request_id() -> Option<Option<u64>> {
        CAPTURED_REQUEST_ID.with(|c| c.borrow_mut().take())
    }

    pub(super) fn intercept(
        frame: &DaemonRequestFrame,
        packs: Option<&[String]>,
    ) -> Option<Option<Result<String, McpError>>> {
        let was_armed = ARMED.with(|a| a.replace(false));
        if !was_armed {
            return None;
        }
        CAPTURED.with(|c| *c.borrow_mut() = Some(packs.map(<[String]>::to_vec)));
        CAPTURED_REQUEST_ID.with(|c| *c.borrow_mut() = Some(frame.request_id));
        Some(Some(Ok(serde_json::json!({
            "results": [{"ok": true, "tool": "stats", "result": {}}],
            "summary": {"total": 1, "succeeded": 1, "failed": 0},
        })
        .to_string())))
    }
}

#[cfg(test)]
async fn forward_or_spawn_with_exe(
    frame: &DaemonRequestFrame,
    exe: &std::path::Path,
) -> Option<Result<String, McpError>> {
    let spawn = || spawn_daemon_with_exe(exe);
    forward_or_spawn_with(frame, &spawn).await
}

async fn forward_or_spawn_with<F>(
    frame: &DaemonRequestFrame,
    spawn: &F,
) -> Option<Result<String, McpError>>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    forward_or_spawn_with_policy(frame, spawn, false).await
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SupervisorDiscoveryPoint {
    UnmanagedRetry,
    BeforeRecovery,
    RecoveryAdmission,
    SupervisorWait,
}

#[cfg(test)]
type SupervisorDiscoveryHook = (SupervisorDiscoveryPoint, Box<dyn FnOnce() + Send>);

#[cfg(test)]
static SUPERVISOR_DISCOVERY_HOOK: std::sync::Mutex<Option<SupervisorDiscoveryHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn supervisor_discovery_hook(point: SupervisorDiscoveryPoint) {
    let action = {
        let mut hook = SUPERVISOR_DISCOVERY_HOOK.lock().expect("discovery hook");
        if hook.as_ref().is_some_and(|(at, _)| *at == point) {
            hook.take().map(|(_, action)| action)
        } else {
            None
        }
    };
    if let Some(action) = action {
        action();
    }
}

async fn forward_or_spawn_with_policy<F>(
    frame: &DaemonRequestFrame,
    spawn: &F,
    replay_read_only: bool,
) -> Option<Result<String, McpError>>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    if env_truthy("KHIVE_NO_DAEMON") {
        return None;
    }

    let request_started = tokio::time::Instant::now();
    let mut replay = ReadReplayBudget::new(
        replay_read_only && crate::request_policy::read_replay_safe(&frame.ops),
    );
    let mut first = try_forward_with_read_replay(frame, &mut replay, None).await;
    let mut initial_supervisor_marker: Option<SupervisorMarker> = None;
    let mut degraded_bootstrap = false;
    if matches!(first, ForwardOutcome::NoSocket) {
        if let Some(marker) = read_supervisor_marker() {
            let budget = initial_supervisor_marker.get_or_insert(marker).clone();
            let waited =
                match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                    Ok(outcome) => outcome,
                    Err(error) => return Some(Err(error)),
                };
            degraded_bootstrap |= waited.degraded_bootstrap;
            first = waited.outcome;
        } else if recorded_daemon_is_alive() {
            // Unmanaged daemon handover retains its existing reconnect grace.
            let deadline = bounded_retry_deadline();
            while matches!(first, ForwardOutcome::NoSocket)
                && tokio::time::Instant::now() < deadline
                && !khive_storage::request_read_is_cancelled()
            {
                sleep_until_retry(deadline).await;
                #[cfg(test)]
                supervisor_discovery_hook(SupervisorDiscoveryPoint::UnmanagedRetry);
                if let Some(marker) = read_supervisor_marker() {
                    let budget = initial_supervisor_marker.get_or_insert(marker).clone();
                    let waited = match wait_for_supervisor(
                        frame,
                        &mut replay,
                        request_started,
                        budget,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => return Some(Err(error)),
                    };
                    degraded_bootstrap |= waited.degraded_bootstrap;
                    first = waited.outcome;
                    break;
                }
                if tokio::time::Instant::now() >= deadline
                    || khive_storage::request_read_is_cancelled()
                {
                    break;
                }
                first = try_forward_with_read_replay(frame, &mut replay, Some(deadline)).await;
            }
        }
    }
    // A claim may arrive at the end of unmanaged grace or without a PID file.
    // Never restart a supervisor budget already consumed by this request.
    if matches!(first, ForwardOutcome::NoSocket) && !degraded_bootstrap {
        #[cfg(test)]
        supervisor_discovery_hook(SupervisorDiscoveryPoint::BeforeRecovery);
        if let Some(marker) = read_supervisor_marker() {
            let budget = initial_supervisor_marker.get_or_insert(marker).clone();
            let waited =
                match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                    Ok(outcome) => outcome,
                    Err(error) => return Some(Err(error)),
                };
            degraded_bootstrap |= waited.degraded_bootstrap;
            first = waited.outcome;
        }
    }
    let marker_guard = loop {
        if matches!(first, ForwardOutcome::NoSocket)
            && (khive_storage::request_read_is_cancelled()
                || khive_storage::capture_request_read_context()
                    .deadline()
                    .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at()))
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        match first {
            ForwardOutcome::Response(resp) => {
                return map_response(*resp, &frame.config_id, &frame.namespace)
            }
            ForwardOutcome::NoSocket => {
                // No claim yet, or its bounded startup grace expired.
            }
            ForwardOutcome::Unreachable {
                kind,
                os_error_code,
            } => return Some(Err(daemon_unreachable_error(frame, kind, os_error_code))),
            ForwardOutcome::ParseFailure | ForwardOutcome::ResponseLost => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    retry_suppressed = true,
                    "daemon connection lost after the request was fully written — \
                     not retrying or falling back locally to avoid duplicate dispatch"
                );
                return Some(Err(ambiguous_forward_error()));
            }
            ForwardOutcome::ProtocolMismatch {
                daemon_protocol_version,
            } => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    retry_suppressed = true,
                    "daemon protocol mismatch discovered after the request was fully \
                     written — not retrying or falling back locally to avoid duplicate dispatch"
                );
                // #714: the daemon is fine (it just rejected us) — this bridge
                // process itself is the stale one. Trigger self-heal alongside
                // the hard error below, never in place of it.
                trigger_bridge_self_heal();
                return Some(Err(protocol_mismatch_error(
                    protocol_mismatch_message(daemon_protocol_version),
                    None,
                )));
            }
            ForwardOutcome::RequestTooLarge { bytes } => {
                return Some(Err(request_too_large_error(bytes)));
            }
        }

        // This is after the last unlocked marker read and inside recovery
        // admission. A launcher that publishes first holds the same lock.
        #[cfg(test)]
        supervisor_discovery_hook(SupervisorDiscoveryPoint::RecoveryAdmission);
        let guard = match acquire_supervisor_marker_lock().await {
            Ok(guard) => guard,
            Err(error) => {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                ) {
                    return Some(Err(supervisor_marker_lock_wait_error()));
                }
                tracing::warn!(error = %error, "supervisor marker lock unavailable; suppressing lifecycle recovery");
                return Some(Err(daemon_mcp_error(
                    "supervisor marker ownership could not be established; retry the request",
                    Some(serde_json::json!({"reason": "supervisor_marker_lock_unavailable"})),
                )));
            }
        };
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if let Some(marker) = read_supervisor_marker() {
            if !degraded_bootstrap {
                drop(guard);
                let budget = initial_supervisor_marker.get_or_insert(marker).clone();
                let waited =
                    match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                        Ok(outcome) => outcome,
                        Err(error) => return Some(Err(error)),
                    };
                degraded_bootstrap |= waited.degraded_bootstrap;
                first = waited.outcome;
                continue;
            }
        }
        // The first supervisor's bounded grace has elapsed, or the locked
        // read found no declaration. Rewrites never restart that first bound.
        // Keep the lock through this client's spawn and readiness wait, never
        // through a supervisor wait (whose launcher must reacquire it).
        break guard;
    };

    // NoSocket: nothing has been written yet. Establish a live daemon — either
    // this is the first-ever spawn or a stale one needs replacing — under the
    // single recovery lock, using only `probe_only` frames for the identity
    // check. `kill_and_respawn` is a no-op kill when there is nothing stale to
    // remove, so first-spawn and recovery share this one path.
    //
    // #898: `spawned_child` holds the live handle from `RecoveryOutcome::Spawned`
    // (if THIS call actually spawned one) purely so the two "about to give up
    // and fall back locally" points below can check whether that specific
    // attempt already exited — never polled eagerly, never used to cut the
    // connect-retry window or the #667 boot-quiescence wait short.
    let mut spawned_child: Option<std::process::Child> = None;
    // Only a client that actually spawns keeps the marker lock through its
    // ADR-049 readiness wait. A skipped/uncertain recovery did not acquire a
    // child to protect and must let a waiting launcher publish immediately.
    let mut marker_guard = Some(marker_guard);
    let recovery = kill_and_respawn(&frame.config_id, &frame.namespace, spawn).await;
    match recovery {
        Err(RecoveryError::RequestExpired) => {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        Err(RecoveryError::Spawn(e)) => {
            // #898: `Command::spawn` itself failed to start the child at all —
            // an unambiguous, already-fully-diagnosed respawn failure. Loud in
            // both strict and non-strict mode; never a silent local fallback.
            return Some(Err(respawn_failed_error(RespawnFailure::SpawnError {
                os_error_code: e.raw_os_error(),
            })));
        }
        Err(RecoveryError::IncumbentStillAlive { pid }) => {
            return Some(Err(incumbent_still_alive_error(pid)));
        }
        Err(RecoveryError::PidFileDirectoryUntrusted(message)) => {
            return Some(Err(untrusted_pid_file_directory_error(message)));
        }
        Ok(RecoveryOutcome::Skipped) => {
            // A concurrent client already has a live matching daemon ready.
            drop(marker_guard.take());
        }
        Ok(RecoveryOutcome::Spawned(child)) => {
            // Give the kernel a moment to release the socket path and let the
            // spawned daemon process start.
            spawned_child = Some(child);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(RecoveryOutcome::Uncertain) => {
            drop(marker_guard.take());
            // Could not positively confirm the daemon's state within the
            // deadline (#838) — behave like `Skipped` (never
            // kill on an unconfirmed state) and let the forward loop below
            // discover the real state: it will either reach a daemon a peer
            // is spawning, or hit the readiness deadline and fall through to
            // `wait_for_boot_quiescence_then_reprobe`.
            tracing::debug!(
                "daemon recovery state uncertain; forwarding without a fresh kill+spawn"
            );
        }
    }

    // Send the real frame now that a daemon is confirmed ready
    // (or believed ready via Skipped). The connect attempt inside
    // `try_forward_before` doubles as the readiness check — a `NoSocket`
    // outcome here just means "not listening yet" (nothing written), so keep
    // retrying. Only the explicit read policy may replay a lost response;
    // every other post-write outcome is terminal and returned immediately.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut boot_fence_confirmed = false;
    loop {
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if tokio::time::Instant::now() >= deadline {
            if boot_fence_confirmed {
                return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
            }
            // #667: a bare timeout here does not mean "no daemon" — it may
            // mean "daemon is still inside cold-boot schema init". Wait for
            // that boot (if any) to quiesce and re-probe before deciding.
            match wait_for_boot_quiescence_then_reprobe(frame).await {
                BootFenceOutcome::DaemonReady => {
                    // This 500 ms identity probe already confirmed readiness.
                    // Release the launcher's marker lock before dispatch;
                    // repeating the shorter 100 ms probe can otherwise loop
                    // forever after the five-second retry bound.
                    drop(marker_guard.take());
                    boot_fence_confirmed = true;
                }
                BootFenceOutcome::SafeLocalFallback => {
                    // #898: only now — after the full connect-retry window AND
                    // the #667 boot-quiescence wait, exactly as before — check
                    // whether the respawn THIS call made has already exited.
                    // A confirmed exit is unambiguous (this process spawned
                    // that exact child) and is never treated as the
                    // legitimate ADR-049 no-daemon case.
                    if let Some(child) = spawned_child.as_mut() {
                        if let Ok(Some(status)) = child.try_wait() {
                            return Some(Err(respawn_failed_error(
                                RespawnFailure::ExitedBeforeBind {
                                    exit_code: status.code(),
                                },
                            )));
                        }
                    }
                    return fallback_or_reject(
                        FallbackReason::NoSocket,
                        &frame.config_id,
                        None,
                        &frame.namespace,
                    );
                }
                BootFenceOutcome::HardError(err) => return Some(Err(err)),
            }
        }
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if spawned_child.is_some() && marker_guard.is_some() {
            // The launcher may be waiting for this marker lock. Release it as
            // soon as our daemon answers an identity probe, before the real
            // request is dispatched (which may run for much longer).
            if matches!(
                probe_daemon_identity(&frame.config_id, &frame.namespace, 100).await,
                ProbeOutcome::Alive
            ) {
                drop(marker_guard.take());
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        }
        match try_forward_with_read_replay(frame, &mut replay, None).await {
            ForwardOutcome::Response(resp) => {
                return map_response(*resp, &frame.config_id, &frame.namespace)
            }
            ForwardOutcome::RequestTooLarge { bytes } => {
                return Some(Err(request_too_large_error(bytes)));
            }
            ForwardOutcome::ParseFailure | ForwardOutcome::ResponseLost => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    retry_suppressed = true,
                    "freshly-established daemon connection lost after the request \
                     was fully written — not retrying or falling back locally"
                );
                return Some(Err(ambiguous_forward_error()));
            }
            ForwardOutcome::ProtocolMismatch {
                daemon_protocol_version,
            } => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    "daemon protocol mismatch discovered on the post-recovery retry \
                     — not retrying again or falling back locally"
                );
                // #714: same self-heal trigger as the first-attempt arm above —
                // either arm can observe the mismatch depending on whether this
                // was the first probe or a retry after a kill/respawn.
                trigger_bridge_self_heal();
                return Some(Err(protocol_mismatch_error(
                    protocol_mismatch_message(daemon_protocol_version),
                    None,
                )));
            }
            ForwardOutcome::Unreachable {
                kind,
                os_error_code,
            } => return Some(Err(daemon_unreachable_error(frame, kind, os_error_code))),
            ForwardOutcome::NoSocket => {
                if boot_fence_confirmed {
                    return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Let sibling unit tests compose the real frame mapping with a captured dispatch.
#[cfg(all(unix, test))]
pub(crate) fn map_response_for_test(
    response: DaemonResponseFrame,
    expected_config_id: &str,
    namespace: &str,
) -> Option<Result<String, McpError>> {
    map_response(response, expected_config_id, namespace)
}

#[cfg(test)]
mod demand_launch_tests;

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
