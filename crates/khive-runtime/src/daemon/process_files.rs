use super::*;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Liveness verdict for a `kill(pid, 0)` probe.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PidLiveness {
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
    pub(super) fn is_running(self) -> bool {
        !matches!(self, PidLiveness::Dead)
    }
}

/// Maps a `kill(pid, 0)` outcome (return code + errno) to a [`PidLiveness`].
/// Pure and side-effect-free so the errno mapping can be unit tested without
/// a real process probe.
#[cfg(unix)]
pub(super) fn classify_kill_result(rc: i32, errno: i32) -> PidLiveness {
    if rc == 0 {
        return PidLiveness::Alive;
    }
    match errno {
        libc::EPERM => PidLiveness::PermissionDenied,
        _ => PidLiveness::Dead,
    }
}

#[cfg(unix)]
pub(super) fn is_process_running(pid: u32) -> bool {
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

/// Whether a PID may identify an incumbent from this candidate's point of view.
///
/// Production rejects the current PID so a stale rendezvous left by a prior
/// process whose PID was reused cannot protect an unrelated socket. The
/// in-process daemon harness opts in to the same-PID case because all of its
/// otherwise independent boot candidates necessarily share one OS process.
#[cfg(unix)]
pub(super) fn pid_can_name_incumbent(
    pid: u32,
    current_pid: u32,
    allow_same_process_incumbent: bool,
) -> bool {
    allow_same_process_incumbent || pid != current_pid
}

/// Bounded timeout for the protocol-identity probe used by duplicate-daemon
/// detection. Short enough that a hung or foreign listener does not stall
/// startup; long enough for a live khived under normal load to answer a
/// `probe_only` frame.
#[cfg(unix)]
pub(super) const DUPLICATE_PROBE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Whether the listener at `sock` actually speaks the khived wire protocol
/// **as a khived this process can defer to** — identified by a configuration
/// compatible with `expected_config_id`.
///
/// A live PID plus an accepting Unix socket is not proof of khived: any
/// unrelated process that happens to have bound the same path also answers
/// `connect()`. Nor is any well-formed [`DaemonResponseFrame`] proof: a
/// `config_mismatch`/`version_mismatch` response, a `metrics_only` snapshot
/// response, or a legacy pre-probe daemon that falls through to normal
/// dispatch on the empty `ops` string, all deserialize cleanly without being
/// the unambiguous "yes, alive and identity-matching" answer this check
/// needs — the daemon's `metrics_only` arm in particular echoes the same
/// `ok=true, result=None, error=None`, all-mismatch-flags-false, matching
/// protocol version and `served_config_id` shape as the probe-ack arm, and
/// is distinguished only by carrying `metrics: Some(...)`. This sends a
/// bounded `probe_only` frame (the same identity probe the client-side
/// recovery path uses, `crates/khive-mcp/src/daemon.rs::probe_daemon_identity`)
/// carrying this process's own `config_id`, and requires a compatible
/// probe-branch shape back: `ok=true`, `result=None`, `error=None`,
/// `metrics=None`, `request_id=None` (this probe frame never sets one), no
/// mismatch flags, matching protocol version, and matching
/// `served_config_id` — mirroring the client probe's `is_probe_ack` check so
/// both sides of the protocol agree on what "alive" means. Connect, write,
/// and read are all inside the one bounded timeout: `UnixStream::connect`
/// itself awaits write readiness, so a listener with a saturated accept
/// backlog could otherwise hold this call open past the advertised bound.
/// A connect that succeeds but never answers, times out, or answers with
/// non-protocol bytes, a mismatched identity, a `metrics_only` snapshot, or
/// any other non-probe-shaped response is not treated as the same khived
/// and falls through to the stale-socket recovery path instead.
#[cfg(unix)]
pub(super) async fn socket_speaks_khived_protocol(
    sock: &std::path::Path,
    expected_config_id: &str,
) -> bool {
    let probe = DaemonRequestFrame {
        probe_only: true,
        protocol_version: PROTOCOL_VERSION,
        config_id: expected_config_id.to_string(),
        ..Default::default()
    };
    let Ok(payload) = serde_json::to_vec(&probe) else {
        return false;
    };
    let response = tokio::time::timeout(DUPLICATE_PROBE_TIMEOUT, async {
        let mut stream = UnixStream::connect(sock).await.ok()?;
        write_frame(&mut stream, &payload).await.ok()?;
        let raw = read_frame(&mut stream).await.ok()?;
        serde_json::from_slice::<DaemonResponseFrame>(&raw).ok()
    })
    .await
    .ok()
    .flatten();

    let Some(resp) = response else {
        return false;
    };
    let is_probe_ack = resp.ok
        && resp.result.is_none()
        && resp.error.is_none()
        && resp.metrics.is_none()
        && resp.request_id.is_none();
    is_probe_ack
        && !resp.version_mismatch
        && !resp.namespace_mismatch
        && !resp.config_mismatch
        && resp.daemon_protocol_version == PROTOCOL_VERSION
        && resp
            .served_config_id
            .as_deref()
            .is_some_and(|served| config_ids_compatible(expected_config_id, served))
}

/// Whether connecting to an existing socket path is definitely unreachable.
/// Timeouts and other errors remain ambiguous so cleanup fails closed.
#[cfg(unix)]
pub(super) async fn socket_is_unreachable(sock: &std::path::Path) -> bool {
    match tokio::time::timeout(DUPLICATE_PROBE_TIMEOUT, UnixStream::connect(sock)).await {
        Ok(Err(error)) => matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
        _ => false,
    }
}

/// What owns the daemon PID file, from the point of view of a process that wants
/// to start. A live owner is never cleaned up: a draining incumbent closes its
/// listener before it releases writers, so an unanswered socket is ambiguous and
/// deleting its PID file is how two daemons end up on one store.
#[cfg(unix)]
pub(super) enum Incumbent {
    /// A live process that answered the khived protocol on the socket.
    Serving(u32),
    /// A live PID still has an active or ambiguous rendezvous. Nothing removed.
    Live(u32),
    /// A rendezvous could be live or could not be safely inspected.
    Unknown,
    /// Nothing live owns the store; the socket and PID file were removed.
    Stale,
}

#[cfg(unix)]
fn read_pid_contents(file: &std::fs::File) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "daemon PID file is not a regular file",
        ));
    }
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    let mut contents = String::new();
    (&mut file).take(4097).read_to_string(&mut contents)?;
    if contents.len() > 4096 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "daemon PID file exceeds the inspection limit",
        ));
    }
    Ok(contents)
}

#[cfg(unix)]
fn read_pid_file(
    pid_file: &std::path::Path,
) -> std::io::Result<(std::fs::Metadata, String)> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(pid_file)?;
    let metadata = file.metadata()?;
    let contents = read_pid_contents(&file)?;
    Ok((metadata, contents))
}

/// Check whether `pid_file`/`sock` already name a live daemon and, if not,
/// remove the stale rendezvous files so the caller may bind fresh.
///
/// A live protocol responder, reachable socket, or live holder of the PID-file
/// lock means the caller must refuse to start. An unlocked PID with no listener
/// is a reused PID and may be reclaimed.
#[cfg(unix)]
pub(super) async fn cleanup_stale_daemon(
    sock: &std::path::Path,
    pid_file: &std::path::Path,
    allow_same_process_incumbent: bool,
    expected_config_id: &str,
) -> Incumbent {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let mut stale_pid_file_guard = None;
    let pid_snapshot = match read_pid_file(pid_file) {
        Ok(snapshot) => Some(snapshot),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Incumbent::Unknown,
    };
    let pid_contents = pid_snapshot.as_ref().map(|(_, contents)| contents);
    if let Some(pid_str) = pid_contents {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if pid_can_name_incumbent(pid, std::process::id(), allow_same_process_incumbent)
                && is_process_running(pid)
            {
                if sock.exists() && socket_speaks_khived_protocol(sock, expected_config_id).await {
                    return Incumbent::Serving(pid);
                }
                if sock.exists() && !socket_is_unreachable(sock).await {
                    return Incumbent::Live(pid);
                }
                match try_acquire_pid_file_lock(pid_file) {
                    Ok(Some(guard)) => stale_pid_file_guard = Some(guard),
                    Ok(None) => return Incumbent::Live(pid),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = ?pid_file,
                            "cannot check daemon PID-file lock"
                        );
                        return Incumbent::Live(pid);
                    }
                }
            }
        }
    }
    if pid_contents.is_some() && stale_pid_file_guard.is_none() {
        match try_acquire_pid_file_lock(pid_file) {
            Ok(Some(guard)) => stale_pid_file_guard = Some(guard),
            Ok(None) | Err(_) => return Incumbent::Unknown,
        }
    }
    if let (Some((observed, expected)), Some(guard)) = (&pid_snapshot, &stale_pid_file_guard) {
        let (Ok(owned), Ok(current), Ok(contents)) = (
            guard.metadata(),
            std::fs::symlink_metadata(pid_file),
            read_pid_contents(guard),
        ) else {
            return Incumbent::Unknown;
        };
        if !owned.is_file()
            || !current.is_file()
            || owned.dev() != observed.dev()
            || owned.ino() != observed.ino()
            || owned.dev() != current.dev()
            || owned.ino() != current.ino()
            || &contents != expected
        {
            return Incumbent::Unknown;
        }
    }

    let socket_metadata = match std::fs::symlink_metadata(sock) {
        Ok(metadata) if metadata.file_type().is_socket() => Some(metadata),
        Ok(_) => return Incumbent::Unknown,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Incumbent::Unknown,
    };
    if let Some(observed) = socket_metadata {
        if !socket_is_unreachable(sock).await {
            return Incumbent::Unknown;
        }
        let current = match std::fs::symlink_metadata(sock) {
            Ok(metadata) if metadata.file_type().is_socket() => metadata,
            _ => return Incumbent::Unknown,
        };
        if observed.dev() != current.dev() || observed.ino() != current.ino() {
            return Incumbent::Unknown;
        }
        if let Err(error) = std::fs::remove_file(sock) {
            tracing::warn!(%error, path = ?sock, "failed to remove stale socket");
            return Incumbent::Unknown;
        }
    }
    if let Some(guard) = &stale_pid_file_guard {
        let owned = match guard.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return Incumbent::Unknown,
        };
        let current = match std::fs::symlink_metadata(pid_file) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return Incumbent::Unknown,
        };
        if owned.dev() != current.dev() || owned.ino() != current.ino() {
            return Incumbent::Unknown;
        }
        if let Err(error) = std::fs::remove_file(pid_file) {
            tracing::warn!(%error, path = ?pid_file, "failed to remove stale PID file");
            return Incumbent::Unknown;
        }
    }
    drop(stale_pid_file_guard);
    Incumbent::Stale
}

/// Create and lock `pid_file` exclusively (`O_EXCL`) and write this process's PID.
///
/// Uses `create_new(true)` rather than `create(true).truncate(true)` so
/// this can never silently overwrite a PID file another process created —
/// the held file lock also identifies a starting or draining daemon when its
/// socket is not yet reachable.
#[cfg(unix)]
pub(super) fn write_pid_file_exclusive(
    pid_file: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    let mut f = opts.open(pid_file)?;
    // SAFETY: flock is a POSIX advisory lock with no memory side effects.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    f.write_all(std::process::id().to_string().as_bytes())?;
    Ok(f)
}

/// Try to lock an existing PID file without creating it. `Some(file)` means
/// there is no daemon lock holder; `None` means a daemon still owns the file.
#[cfg(unix)]
pub(super) fn try_acquire_pid_file_lock(
    pid_file: &std::path::Path,
) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;

    let open = |write| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(pid_file)
    };
    let file = match open(true) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => open(false)?,
        result => result?,
    };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "daemon PID file is not a regular file",
        ));
    }
    // SAFETY: flock is a POSIX advisory lock with no memory side effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(libc::EWOULDBLOCK)
    {
        Ok(None)
    } else {
        Err(error)
    }
}

/// Remove the PID file on setup failure only while its path still names the
/// file this start attempt created.
#[cfg(unix)]
pub(super) fn remove_pid_file_if_owned(pid_file: &std::path::Path, guard: &std::fs::File) {
    let Ok(owned) = guard.metadata() else {
        return;
    };
    let Ok(current) = std::fs::metadata(pid_file) else {
        return;
    };
    if owned.dev() == current.dev() && owned.ino() == current.ino() {
        if let Err(e) = std::fs::remove_file(pid_file) {
            tracing::warn!(error = %e, path = ?pid_file, "failed to remove unbound PID file");
        }
    }
}

/// Return `true` if `pid_file` currently names an eligible live process that
/// still answers on `sock` — i.e. a daemon already owns this rendezvous and it
/// is safe to defer to it rather than treat the `AlreadyExists` PID-file
/// collision as a boot failure. Eligibility requires a different PID in
/// production; the explicit in-process harness may allow the current PID.
#[cfg(unix)]
pub(super) async fn pid_file_names_a_reachable_daemon(
    pid_file: &std::path::Path,
    sock: &std::path::Path,
    allow_same_process_incumbent: bool,
    expected_config_id: &str,
) -> bool {
    let Ok(pid_str) = std::fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        return false;
    };
    pid_can_name_incumbent(pid, std::process::id(), allow_same_process_incumbent)
        && is_process_running(pid)
        && sock.exists()
        && socket_speaks_khived_protocol(sock, expected_config_id).await
}

#[cfg(all(unix, test))]
pub(super) async fn drain(active: &std::sync::atomic::AtomicUsize) -> bool {
    drain_with_timeout(active, drain_timeout()).await
}

/// Voluntary retirement keeps admitted workers and rendezvous ownership alive.
#[cfg(unix)]
pub(super) async fn drain_for_idle(
    active: &std::sync::atomic::AtomicUsize,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut warned = false;
    while active.load(std::sync::atomic::Ordering::SeqCst) + background_task_count() != 0 {
        if !warned && tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "idle drain interval elapsed; retaining workers and rendezvous until settled"
            );
            warned = true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
pub(super) async fn drain_with_timeout(
    active: &std::sync::atomic::AtomicUsize,
    timeout: std::time::Duration,
) -> bool {
    use std::sync::atomic::Ordering;
    // One sequentially-consistent order spans connection handoff and tracked
    // task publication. Drain must not observe an ended connection and a
    // stale pre-publication background count as two simultaneous zeroes.
    let remaining = || active.load(Ordering::SeqCst) + background_task_count();
    if remaining() == 0 {
        return true;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while remaining() > 0 {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                remaining_connections = active.load(Ordering::SeqCst),
                remaining_background_tasks = background_task_count(),
                outstanding_background_tasks = %background_task_names().join(", "),
                "drain timeout reached; forcing shutdown"
            );
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
    true
}

#[cfg(unix)]
pub(super) async fn finish_connection_tasks(
    tasks: Vec<tokio::task::JoinHandle<()>>,
    drained: bool,
) {
    if !drained {
        for task in &tasks {
            if !task.is_finished() {
                task.abort();
            }
        }
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// The bound `drain()` waits for tracked background tasks at daemon shutdown
/// (`KHIVE_DRAIN_TIMEOUT_SECS`, default 10s). Public so component supervision
/// can clamp per-component shutdown timeouts against it — a component timeout
/// longer than the drain bound could never complete its abort/state
/// transition before the daemon returns.
pub fn drain_timeout() -> std::time::Duration {
    let secs = khive_db::env::env_parse_or("KHIVE_DRAIN_TIMEOUT_SECS", DEFAULT_DRAIN_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}
