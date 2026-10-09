//! PID identity, stale rendezvous cleanup and daemon liveness probes.

use khive_runtime::daemon::{
    pid_path, read_frame, socket_path, write_frame, DaemonRequestFrame, DaemonResponseFrame,
    PROTOCOL_VERSION,
};
use tokio::net::UnixStream;

use super::forward::{
    pid_file_directory_is_trusted_if_present, try_forward_before, ForwardOutcome,
};
#[cfg(test)]
use super::test_harness::{FORCE_PID_IS_DAEMON, FORCE_PID_IS_FOREIGN, KILL_COUNT, SIGTERM_COUNT};

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
pub(super) fn argv_is_khive_daemon(args: &str) -> bool {
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

pub(super) const INCUMBENT_EXIT_TIMEOUT_SECS: u64 = 12;
const INCUMBENT_EXIT_POLL_MS: u64 = 25;

#[derive(Debug)]
pub(super) enum RecoveryError {
    Spawn(std::io::Error),
    IncumbentStillAlive { pid: u32 },
    RequestExpired,
    PidFileDirectoryUntrusted(String),
}

pub(super) fn process_is_alive(pid: u32) -> bool {
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
pub(super) enum PidFileSnapshot {
    Missing,
    Present(Vec<u8>),
    Unreadable,
}

impl PidFileSnapshot {
    pub(super) fn read(path: &std::path::Path) -> Self {
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
pub(super) async fn kill_stale_daemon_inner(
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
                tracing::warn!(target: "khive_mcp::daemon",
                    pid,
                    "PID in daemon file belongs to a foreign process — treating it as stale"
                );
                false
            }
            PidIdentity::Indeterminate => {
                tracing::warn!(target: "khive_mcp::daemon",
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
pub(super) fn remove_daemon_paths_if_still_stale(
    pid_file: &std::path::Path,
    expected_snapshot: &PidFileSnapshot,
) -> bool {
    let (current_snapshot, pid_directory_trusted) =
        match pid_file_directory_is_trusted_if_present(pid_file) {
            Ok(true) => (PidFileSnapshot::read(pid_file), true),
            Ok(false) => (PidFileSnapshot::Missing, false),
            Err(error) => {
                tracing::warn!(target: "khive_mcp::daemon",
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
        tracing::warn!(target: "khive_mcp::daemon",
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
            tracing::warn!(target: "khive_mcp::daemon", socket = ?sock, "live listener claimed rendezvous; skipping cleanup and launch");
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
pub(super) enum ProbeOutcome {
    /// A live, identity-matching daemon responded before the deadline.
    Alive,
    /// Daemon is absent, crashed, or identity-mismatched — safe to kill+spawn.
    Dead,
    /// Probe timed out — daemon may be alive but slow; do NOT kill.
    Timeout,
    /// The boot/recovery lock ([`khive_runtime::daemon::lock_path`]) stayed
    /// contended past its bounded acquisition deadline while
    /// [`super::quiesce_then_probe_identity`] was trying to confirm no peer boot is
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
pub(super) async fn probe_daemon_identity(
    config_id: &str,
    namespace: &str,
    timeout_ms: u64,
) -> ProbeOutcome {
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
            tracing::debug!(target: "khive_mcp::daemon",
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
                tracing::debug!(target: "khive_mcp::daemon", "under-lock probe: live matching daemon confirmed; skipping kill");
                ProbeOutcome::Alive
            } else {
                tracing::debug!(target: "khive_mcp::daemon",
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
            tracing::debug!(target: "khive_mcp::daemon",
                ?kind,
                ?os_error_code,
                "under-lock probe could not reach the daemon socket; treating its state as \
                 uncertain and suppressing lifecycle recovery"
            );
            ProbeOutcome::Timeout
        }
    }
}
