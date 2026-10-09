#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(unix)]
use super::{lock_path, recoverer_lock_path};

#[cfg(unix)]
pub(super) fn open_lock_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
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
            tracing::warn!(target: "khive_runtime::daemon", error = %e, path = ?path, "cannot open {label} lock file");
            return None;
        }
    };
    // SAFETY: flock is a POSIX advisory lock with no memory side-effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        tracing::warn!(target: "khive_runtime::daemon", "flock LOCK_EX failed on {label} lock");
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
pub(super) struct SocketIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
pub(super) fn socket_identity(path: &std::path::Path) -> Option<SocketIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(SocketIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

// ── connection principal ──────────────────────────────────────────────────────

/// The uid on the other end of an accepted connection, read from the kernel.
///
/// This is the only identity on this socket the caller cannot choose. Every
/// identity field on the request frame — `namespace`, `actor_id`,
/// `visible_namespaces`, `config_id` — is supplied by the connecting process,
/// so none of them can answer "who is this". A check reading self-asserted
/// fields is not a weak gate, it is not a gate: anyone who wants to pass it
/// asserts the passing values.
///
/// `getpeereid(2)` on macOS/BSD, `SO_PEERCRED` on Linux. Both report the peer's
/// credentials as recorded by the kernel at connect time.
#[cfg(unix)]
pub(crate) fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();

    #[cfg(any(target_os = "macos", target_os = "ios", target_vendor = "apple"))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `fd` is a live connected socket owned by `stream` for the
        // duration of this call; both out-params are valid initialized locals.
        let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(uid as u32)
    }

    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `fd` is a live connected socket owned by `stream`; `cred` is
        // an initialized local of exactly `len` bytes, which is what
        // SO_PEERCRED writes.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(cred.uid)
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_vendor = "apple"
    )))]
    {
        let _ = fd;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "peer-credential capture is not implemented for this platform",
        ))
    }
}

/// Whether a connection from `uid` may be served by this daemon.
///
/// ADR-096 accepted per-request identity threading **for the single-principal
/// owner-only socket only**, and rested that on three things: the `0600`
/// socket, all connections being the same uid, and the database being already
/// same-uid-accessible. The socket mode is asserted at bind. This asserts the
/// second, which previously had no representation in the code at all — nothing
/// read peer identity, so nothing could notice when it stopped being true.
///
/// **Principal is not attribution.** Many `actor_id`s over one socket is
/// exactly what ADR-096 shipped and what every seat on a normal host does;
/// refusing a second distinct actor would break the accepted design. The
/// principal is the uid, and this refuses only a genuinely foreign one.
///
/// **There is deliberately no configuration escape hatch.** A flag permitting
/// other uids would not weaken this assertion, it would delete it, in the way
/// hardest to notice later: the check still exists, its tests still pass, and
/// the deployment that matters has it off. A deployment that genuinely needs
/// multiple uids needs a code change and a gated ADR — which is precisely the
/// decision that should be impossible to make by accident.
#[cfg(unix)]
pub(crate) fn uid_is_permitted(peer: u32, daemon_euid: u32) -> bool {
    peer == daemon_euid
}
