#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(unix)]
use super::{lock_path, recoverer_lock_path};

#[cfg(unix)]
pub(super) fn open_lock_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    open_mutating_daemon_lock(path, false)
}

/// Open a lock for a mutating boot or recovery. The default directory may be
/// prepared; caller-specified parent directories must already be trusted.
#[cfg(unix)]
pub fn open_mutating_daemon_lock(
    path: &std::path::Path,
    read_write: bool,
) -> std::io::Result<std::fs::File> {
    super::prepare_default_rendezvous_for_recovery().map_err(lock_error)?;
    checked_open_lock(path, true, read_write, true)?.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "created lock is absent")
    })
}

#[cfg(unix)]
fn lock_error(error: anyhow::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("{error:#}"))
}

/// Open a basename relative to an ancestry-checked directory fd. Existing
/// locks can be tightened without allowing creation.
#[cfg(unix)]
pub(super) fn checked_open_lock(
    path: &std::path::Path,
    create: bool,
    read_write: bool,
    tighten: bool,
) -> std::io::Result<Option<std::fs::File>> {
    checked_open_lock_with_hooks(path, create, read_write, tighten, || {}, || {})
}

#[cfg(unix)]
pub(super) fn checked_open_lock_with_hooks(
    path: &std::path::Path,
    create: bool,
    read_write: bool,
    tighten: bool,
    before_openat: impl FnOnce(),
    after_open: impl FnOnce(),
) -> std::io::Result<Option<std::fs::File>> {
    checked_open_lock_with_faults(path, create, read_write, tighten, before_openat, after_open, (false, false))
}

// Fault flags are used only by synthetic tests; production always passes false.
#[cfg(unix)]
pub(super) fn checked_open_lock_with_faults(
    path: &std::path::Path,
    create: bool,
    read_write: bool,
    tighten: bool,
    before_openat: impl FnOnce(),
    after_open: impl FnOnce(),
    faults: (bool, bool),
) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::ffi::OsStrExt;
    let (fail_metadata, fail_chmod) = faults;

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let leaf = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "lock path has no basename")
    })?;
    let leaf = std::ffi::CString::new(leaf.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "lock basename contains NUL")
    })?;
    let observed_parent = match std::fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(std::io::Error::new(
            error.kind(), format!("cannot inspect lock parent {}: {error}", parent.display()),
        )),
    };
    super::ensure_pid_file_dir_is_trusted(path).map_err(lock_error)?;
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent).map_err(|error| std::io::Error::new(
            error.kind(),
            format!("refusing lock parent {} (mode {:04o}): {error}", parent.display(), observed_parent.mode() & 0o7777),
        ))?;
    let opened_parent = directory.metadata()?;
    if !observed_parent.is_dir() || !opened_parent.is_dir()
        || observed_parent.dev() != opened_parent.dev() || observed_parent.ino() != opened_parent.ino()
    {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "lock parent changed during open"));
    }
    before_openat();
    let current_parent = std::fs::symlink_metadata(parent)?;
    if !current_parent.is_dir() || current_parent.dev() != opened_parent.dev()
        || current_parent.ino() != opened_parent.ino()
    {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "lock parent changed before openat"));
    }
    // SAFETY: `geteuid` takes no arguments and always succeeds.
    let uid = unsafe { libc::geteuid() } as u32;
    let observed = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.uid() != uid || metadata.nlink() != 1 {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,
                    format!("refusing lock {}: owner uid {}, mode {:04o}", path.display(), metadata.uid(), metadata.mode() & 0o7777)));
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if observed.is_none() && !create {
        return Ok(None);
    }
    let access = if read_write { libc::O_RDWR } else if tighten && !create { libc::O_RDONLY } else if create { libc::O_WRONLY } else { libc::O_RDONLY };
    let mut flags = access | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let created = observed.is_none();
    if created {
        flags |= libc::O_CREAT | libc::O_EXCL;
    }
    // SAFETY: the parent fd is live, and leaf is a NUL-terminated basename.
    let mut fd = unsafe { libc::openat(directory.as_raw_fd(), leaf.as_ptr(), flags, 0o600) };
    if fd < 0 && !create && tighten && !read_write
        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
    {
        flags = (flags & !libc::O_ACCMODE) | libc::O_WRONLY;
        // SAFETY: the same validated parent fd and basename remain live.
        fd = unsafe { libc::openat(directory.as_raw_fd(), leaf.as_ptr(), flags, 0o600) };
    }
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        let observed_mode = observed.as_ref()
            .map(|metadata| format!("{:04o}", metadata.mode() & 0o7777))
            .unwrap_or_else(|| "absent".to_owned());
        return Err(std::io::Error::new(
            error.kind(),
            format!("cannot safely open lock {} (observed mode {observed_mode}): {error}", path.display()),
        ));
    }
    // SAFETY: `openat` returned a new owned file descriptor.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let identity_changed = std::cell::Cell::new(false);
    let result = (|| -> std::io::Result<()> {
    if fail_metadata {
        return Err(std::io::Error::other("injected metadata failure"));
    }
    let opened = file.metadata()?;
    if !opened.is_file() || opened.uid() != uid || opened.nlink() != 1
        || observed.as_ref().is_some_and(|metadata| metadata.dev() != opened.dev() || metadata.ino() != opened.ino())
    {
        identity_changed.set(true);
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,
            format!("lock {} changed during open (mode {:04o})", path.display(), opened.mode() & 0o7777)));
    }
    after_open();
    let check_identity = || -> std::io::Result<()> {
        let current_parent = std::fs::symlink_metadata(parent)?;
        let current_lock = std::fs::symlink_metadata(path)?;
        if !current_parent.is_dir() || current_parent.dev() != opened_parent.dev() || current_parent.ino() != opened_parent.ino()
            || !current_lock.is_file() || current_lock.dev() != opened.dev() || current_lock.ino() != opened.ino()
        {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "lock or parent changed during open"));
        }
        Ok(())
    };
    check_identity().map_err(|error| { identity_changed.set(true); error })?;
    if tighten {
        let latest = file.metadata()?;
        if !latest.is_file() || latest.uid() != uid || latest.nlink() != 1 {
            identity_changed.set(true);
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "lock ownership changed"));
        }
        let mode = latest.mode() & 0o7777;
        let target = if created { 0o600 } else { mode & 0o600 };
        if target != mode {
            if fail_chmod {
                return Err(std::io::Error::other("injected chmod failure"));
            }
            file.set_permissions(std::fs::Permissions::from_mode(target))?;
        }
    }
    check_identity().map_err(|error| { identity_changed.set(true); error })?;
    Ok(())
    })();
    if let Err(error) = result {
        if created {
            let guidance = if identity_changed.get() {
                "the lock path or its parent changed; inspect the path and do not remove the replacement"
            } else {
                "after confirming no daemon is running and verifying the entry is the leftover lock, remove it and retry; do not remove an unverified entry"
            };
            return Err(std::io::Error::new(error.kind(), format!(
                "failed to finish creating lock {}: {error}; {guidance}", path.display()
            )));
        }
        return Err(error);
    }
    Ok(Some(file))
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
