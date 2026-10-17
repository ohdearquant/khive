use super::*;

/// Advisory lock guaranteeing at most one events daemon per socket path.
/// Held for the daemon's lifetime; a second daemon exits instead of stealing
/// the socket path from the live one.
#[cfg(unix)]
pub struct EventsDaemonGuard {
    pub(super) _file: std::fs::File,
}

/// Try to become the events daemon for `socket_path`, returning `None` for
/// contention or any refusal. Callers must validate the parent with
/// `ensure_socket_dir_is_trusted` before lock-path operations in that directory.
#[cfg(unix)]
pub fn try_acquire_events_daemon_guard(socket_path: &Path) -> Option<EventsDaemonGuard> {
    match acquire_events_daemon_guard_outcome(socket_path) {
        EventsDaemonGuardAcquisition::Held(guard) => Some(guard),
        _ => None,
    }
}

/// Why a non-blocking daemon-lock acquisition did or did not succeed.
/// Refusals retain the I/O error instead of treating every failure as contention.
#[cfg(unix)]
pub(super) enum EventsDaemonGuardAcquisition {
    Held(EventsDaemonGuard),
    Contended,
    OpenFailed(std::io::Error),
    HardeningRefused(std::io::Error),
}

#[cfg(unix)]
impl std::fmt::Debug for EventsDaemonGuardAcquisition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held(_) => f.write_str("Held"),
            Self::Contended => f.write_str("Contended"),
            Self::OpenFailed(error) => f.debug_tuple("OpenFailed").field(error).finish(),
            Self::HardeningRefused(error) => {
                f.debug_tuple("HardeningRefused").field(error).finish()
            }
        }
    }
}

/// Try to become the events daemon for `socket_path`. Every refusal means
/// the caller must not serve; only `Contended` identifies a held lock.
/// Callers that need the socket directory validated must run
/// `ensure_socket_dir_is_trusted` on the parent BEFORE calling this, so no
/// lock-path operation happens in an untrusted directory.
#[cfg(unix)]
pub(super) fn acquire_events_daemon_guard_outcome(
    socket_path: &Path,
) -> EventsDaemonGuardAcquisition {
    use EventsDaemonGuardAcquisition::{Contended, HardeningRefused, Held, OpenFailed};

    let lock_path = socket_path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return OpenFailed(error);
        }
    }
    // `O_NOFOLLOW` pins the open to the final component: a symlink planted
    // at the lock name is refused instead of redirecting the open (and the
    // chmod below) to an attacker-selected target.
    let opened = khive_fs::opened_file::open_writable_file_nofollow(
        &lock_path,
        khive_fs::fd_relative::OpenFileOptions {
            read_write: false,
            create: khive_fs::fd_relative::Create::IfMissing,
            nonblock: false,
            mode: 0o600,
        },
    );
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return HardeningRefused(error);
        }
        Err(error) => return OpenFailed(error),
    };
    // `mode` applies only at creation; tighten a pre-existing lock file too.
    // Descriptor-based (`fchmod` on the handle just opened), never a second
    // path lookup — and fail closed: with the inode pinned, a failed chmod
    // is abnormal, and serving behind a lock file another user can open is
    // exactly what the hardening exists to refuse.
    if let Err(error) = file.set_permissions(
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    ) {
        return HardeningRefused(error);
    }
    use std::os::fd::AsRawFd;
    // SAFETY: `fd` is a live descriptor owned by `file` for the duration of
    // the call; `flock` reads nothing else.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Held(EventsDaemonGuard { _file: file })
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK)
            || error.raw_os_error() == Some(libc::EAGAIN)
        {
            Contended
        } else {
            HardeningRefused(error)
        }
    }
}

pub(super) fn events_db_targets(db_path: &Path) -> [PathBuf; 3] {
    ["", "-wal", "-shm"].map(|suffix| {
        let mut name = db_path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    })
}

/// Refuse to serve an events database whose path — or whose `-wal`/`-shm`
/// sidecar path — is a pre-existing symlink. These paths are derived, never
/// user-chosen (`events_db_path_beside` canonicalizes the main database
/// spelling first), so a link here is a planted redirect, not an alias:
/// permission hardening and SQLite would otherwise follow it and tighten or
/// write event rows through to whatever file the link's author chose
/// (CWE-59). Runs before any open on both the embedded and daemon arms; a
/// link planted after admission is bounded by the daemon's trusted-directory
/// contract on the socket parent.
pub(super) fn refuse_events_db_symlinks(db_path: &Path) -> anyhow::Result<()> {
    for path in events_db_targets(db_path) {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => anyhow::bail!(
                "refusing to serve events: {} is a symlink; the events database and its \
                 sidecars must be regular files",
                path.display()
            ),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!(
                "refusing to serve events: cannot inspect {}: {e}",
                path.display()
            ),
        }
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EventsFileIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
}

#[cfg(unix)]
impl EventsFileIdentity {
    pub(super) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// Main/WAL/SHM identities, in `events_db_targets` order. None means the
/// target was absent when pre-open hardening tried to open it.
#[cfg(unix)]
pub(super) type EventsDbIdentities = [Option<EventsFileIdentity>; 3];

/// Require the directory holding the events database to be trusted before
/// anything opens a path inside it. The symlink pre-checks and the fd-pinned
/// chmod close the races they can see, but SQLite's own open is path-based
/// and cannot be inode-pinned from here — so the remaining defense is the
/// same one the daemon socket uses: no untrusted local user may be able to
/// write (or swap components of) the directory the path traverses. Delegates
/// to the shared trust walk with store-specific diagnostics, validating every
/// ancestor the kernel will visit with the same ownership and sticky-bit rules.
#[cfg(unix)]
pub(super) fn ensure_events_db_parent_trusted(db_path: &Path) -> anyhow::Result<()> {
    let db_path = absolutize(db_path);
    let parent = db_path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(dir) => crate::daemon::ensure_store_dir_is_trusted(dir, &db_path).map_err(|e| {
            anyhow::anyhow!("refusing to serve events from an untrusted directory: {e}")
        }),
        None => Ok(()),
    }
}

/// Create the events database file owner-only if it does not exist yet.
/// The 0600 socket and peer-uid admission bound who can *talk to* the
/// daemon; they bound nothing if the database file itself is readable by
/// other local users, so the daemon creates it 0600 before SQLite ever
/// opens it (SQLite would otherwise create it at the process umask).
#[cfg(unix)]
pub(super) fn ensure_events_db_owner_only(db_path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    refuse_events_db_symlinks(db_path)?;
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // After creation so a fresh parent can be validated; a directory this
    // process just created in a trusted ancestor passes by construction.
    ensure_events_db_parent_trusted(db_path)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(db_path)
    {
        // A zero-byte file is a valid empty SQLite database.
        Ok(_created) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "refusing to serve events: cannot create {} owner-only: {e}",
            db_path.display()
        )),
    }
}

/// Tighten the events database and its SQLite sidecars to owner-only.
/// Fail closed, same contract as the socket chmod: a daemon that cannot
/// keep its database owner-only must not serve it.
///
/// PRECONDITION: no SQLite connection to `db_path` may be open in this
/// process. This function opens and closes a descriptor on the database and
/// on each sidecar, and POSIX advisory locks are per process and per inode,
/// released by the close of ANY descriptor for the inode (fcntl(2)): called
/// on a live database it silently drops the SHARED lock SQLite keeps in WAL
/// mode and the `-shm` DMS lock, while the connection believes it still
/// holds them. Both callers run it before their open; keep it that way. The
/// post-open check that opens nothing is `verify_events_db_owner_only_unopened`.
#[cfg(unix)]
pub(super) fn harden_events_db_sidecars(db_path: &Path) -> anyhow::Result<EventsDbIdentities> {
    use khive_fs::opened_file::ContainedOpenError;
    use std::os::unix::fs::PermissionsExt;
    let mut identities = [None; 3];
    for (index, path) in events_db_targets(db_path).into_iter().enumerate() {
        // Pin the inode before touching it: `O_NOFOLLOW` makes the open
        // itself refuse a symlink at the final component, and the chmod is
        // then issued on the returned handle (fchmod), so no path re-lookup
        // exists between validation and the permission change for a swapped
        // entry to exploit. A path-based lstat-then-chmod pair here would
        // re-open the exact race it is defending against.
        let file = match khive_fs::opened_file::open_regular_file_nofollow(&path) {
            Ok(file) => file,
            Err(ContainedOpenError::Open(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(ContainedOpenError::Open(e)) => anyhow::bail!(
                "refusing to serve events: cannot open {} without following symlinks: \
                 {e}. The events database and its sidecars must be regular files.",
                path.display()
            ),
            Err(ContainedOpenError::Metadata(e)) => return Err(e.into()),
            Err(_) => anyhow::bail!(
                "refusing to serve events: {} is not a regular file. The events \
                 database and its sidecars must be regular files.",
                path.display()
            ),
        };
        // The shared helper has checked the opened file's regular type.
        // Read this same descriptor's metadata for the pre-open identity
        // snapshot; no pathname is reopened before the chmod.
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            anyhow::bail!(
                "refusing to serve events: {} is not a regular file. The events \
                 database and its sidecars must be regular files.",
                path.display()
            );
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| {
                anyhow::anyhow!(
                    "refusing to serve events: cannot chmod 0600 {}: {e}. The events \
                     database and its sidecars must be owner-only.",
                    path.display()
                )
            })?;
        // The same descriptor supplied validation and fchmod; record its
        // identity before closing it, never by re-looking up the path.
        identities[index] = Some(EventsFileIdentity::from_metadata(&metadata));
    }
    Ok(identities)
}

/// Check, after SQLite has opened the database, that it and its sidecars are
/// owner-only regular files WITHOUT opening any of them: `lstat` takes no
/// descriptor, so it cannot release the advisory locks the open connections
/// hold (see the precondition on `harden_events_db_sidecars`). Sidecars
/// SQLite creates inherit the database file's mode, so a failure here means
/// something else changed the file set; refuse to serve rather than tighten
/// through a path-based chmod and its lookup race. Present paths must retain
/// their pre-open device/inode when one was observed. A missing main database
/// is refused: serving an unlinked database would detach writes from its path.
/// Sidecars may legitimately be absent or newly created by SQLite, so an absent
/// pre- or post-observation does not assert identity continuity for that sidecar.
///
/// This is a two-observation check, not a pin of SQLite's own descriptors. It
/// cannot detect swaps restored between observations, inode reuse, or changes
/// after the final lstat. Even a legitimate sidecar delete/recreate is refused
/// if both observations see different identities; a later startup can retry
/// with a fresh snapshot. Trusted parent directories remain required. Unix only.
#[cfg(unix)]
pub(super) fn verify_events_db_owner_only_unopened(
    db_path: &Path,
    before: &EventsDbIdentities,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for (index, path) in events_db_targets(db_path).into_iter().enumerate() {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && index != 0 => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
                "refusing to serve events: main database {} disappeared after SQLite open; \
                 refusing to serve an unlinked database",
                path.display()
            ),
            Err(e) => {
                anyhow::bail!(
                    "refusing to serve events: cannot stat {}: {e}",
                    path.display()
                )
            }
        };
        if !metadata.file_type().is_file() {
            anyhow::bail!(
                "refusing to serve events: {} is not a regular file. The events database \
                 and its sidecars must be regular files.",
                path.display()
            );
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "refusing to serve events: {} is mode {mode:03o}, not owner-only. The events \
                 database and its sidecars must be owner-only.",
                path.display()
            );
        }
        if before[index]
            .is_some_and(|identity| identity != EventsFileIdentity::from_metadata(&metadata))
        {
            anyhow::bail!(
                "refusing to serve events: {} changed identity between pre-open hardening \
                 and post-open verification",
                path.display()
            );
        }
    }
    Ok(())
}

/// How long a daemon holds requests at startup for the events daemon it
/// supervises to answer before it refuses them ([`EventsReadiness`]).
#[cfg(unix)]
pub const EVENTS_READINESS_WINDOW: Duration = Duration::from_secs(10);

/// Startup readiness of the events daemon this process supervises (ADR-170),
/// as seen by the request path.
///
/// A daemon with events forwarding enabled commits every write's audit batch
/// through the supervised events daemon. That child is spawned in the
/// background after the main socket is bound, and until it binds its own
/// socket every audit commit fails: a write verb then reports a refusal
/// while its domain write is already committed. The daemon therefore holds
/// each request at this gate until the events socket has answered once.
///
/// The window is measured from the moment supervision starts, not per
/// request: once it has elapsed without an answer, requests are refused at
/// once instead of each waiting again, and a later answer (a respawned child
/// binding) admits them again. The gate is a startup barrier only; it stays
/// open after the first answer.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct EventsReadiness {
    pub(super) socket: PathBuf,
    pub(super) deadline: tokio::time::Instant,
    pub(super) ready: tokio::sync::watch::Receiver<bool>,
}

/// The supervisor's side of an [`EventsReadiness`] gate.
#[cfg(unix)]
#[derive(Debug)]
pub struct EventsReadinessSignal {
    pub(super) deadline: tokio::time::Instant,
    pub(super) ready: tokio::sync::watch::Sender<bool>,
}

/// Why an [`EventsReadiness`] gate refused a request.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventsNotReadyReason {
    /// The startup window elapsed without the events socket answering.
    WindowElapsed,
    /// The supervisor stopped before the events socket answered.
    SupervisorStopped,
}

/// A request refused by an [`EventsReadiness`] gate; names the socket that
/// did not answer.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventsNotReady {
    pub socket: PathBuf,
    pub reason: EventsNotReadyReason,
}

#[cfg(unix)]
impl std::fmt::Display for EventsNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            EventsNotReadyReason::WindowElapsed => write!(
                f,
                "events daemon at {} has not answered since this daemon started; \
                 the request was refused before dispatch",
                self.socket.display()
            ),
            EventsNotReadyReason::SupervisorStopped => write!(
                f,
                "events daemon supervision for {} stopped before the socket answered; \
                 the request was refused before dispatch",
                self.socket.display()
            ),
        }
    }
}

/// Create a readiness gate for the events daemon at `socket`, whose startup
/// window of `window` starts now.
#[cfg(unix)]
pub fn events_readiness(
    socket: PathBuf,
    window: Duration,
) -> (EventsReadinessSignal, EventsReadiness) {
    let deadline = tokio::time::Instant::now() + window;
    let (sender, receiver) = tokio::sync::watch::channel(false);
    (
        EventsReadinessSignal {
            deadline,
            ready: sender,
        },
        EventsReadiness {
            socket,
            deadline,
            ready: receiver,
        },
    )
}

#[cfg(unix)]
impl EventsReadinessSignal {
    /// Record that the events socket answered; opens the gate for good.
    pub fn mark_ready(&self) {
        self.ready
            .send_if_modified(|ready| !std::mem::replace(ready, true));
    }

    /// Whether requests may still be waiting on this gate: not yet ready and
    /// the startup window still open.
    pub fn awaiting(&self) -> bool {
        !*self.ready.borrow() && tokio::time::Instant::now() < self.deadline
    }

    /// Whether the events socket has answered.
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }
}

#[cfg(unix)]
impl EventsReadiness {
    /// The events socket this gate waits on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Whether the events socket has answered.
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// Wait until the events socket has answered, or refuse once the startup
    /// window has elapsed or the supervisor has stopped.
    pub async fn wait(&self) -> Result<(), EventsNotReady> {
        let mut ready = self.ready.clone();
        let outcome = tokio::time::timeout_at(self.deadline, ready.wait_for(|ready| *ready))
            .await
            .map(|answered| answered.map(|_| ()));
        let refused = |reason| EventsNotReady {
            socket: self.socket.clone(),
            reason,
        };
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(refused(EventsNotReadyReason::SupervisorStopped)),
            Err(_) if self.is_ready() => Ok(()),
            Err(_) => Err(refused(EventsNotReadyReason::WindowElapsed)),
        }
    }
}

/// The startup window for a new [`EventsReadiness`] gate:
/// [`EVENTS_READINESS_WINDOW`], or `KHIVE_TEST_EVENTS_READY_WINDOW_MS` under
/// `KHIVE_TEST_HARNESS=1` so integration tests can reach the refusal quickly.
#[cfg(unix)]
pub fn events_readiness_window() -> Duration {
    harness_duration_ms("KHIVE_TEST_EVENTS_READY_WINDOW_MS").unwrap_or(EVENTS_READINESS_WINDOW)
}

/// A test-only duration in milliseconds read from the variable `name`,
/// honoured only under `KHIVE_TEST_HARNESS=1`. Unset, malformed or outside
/// the harness it is `None`.
#[cfg(unix)]
pub(crate) fn harness_duration_ms(name: &str) -> Option<Duration> {
    if std::env::var("KHIVE_TEST_HARNESS").as_deref() != Ok("1") {
        return None;
    }
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_millis)
}

/// Supervise the events daemon from the main daemon process: probe the
/// socket periodically and (re)spawn the daemon subcommand when unreachable.
///
/// The spawned command contract is fixed here once: the current executable
/// re-invoked as `events-daemon --db <db> --socket <socket>` with the resolved
/// WAL ceiling bytes and source — the subcommand the kernel binary registers
/// for [`run_events_daemon`]. The child holds the per-socket advisory lock,
/// so a probe/spawn race resolves to one survivor.
///
/// Lifecycle: the loop observes the process-wide daemon shutdown token, so
/// `drain()` never waits on it forever, and it retains the handle of the
/// child it spawned — reaping it with `try_wait` on every probe (no zombie
/// accumulation during a persistent startup failure) and never stacking a
/// second spawn on a still-live child. On shutdown, a child this supervisor
/// spawned is killed and reaped; a pre-existing events daemon it never
/// spawned is left alone.
///
/// The reachability probe uses the peer-verified connect: a socket answered
/// by a foreign-uid process is treated as UNREACHABLE (and logged loudly),
/// so a pre-bound spoof socket triggers a real-daemon spawn instead of being
/// reported healthy.
///
/// This standalone entry resolves its environment once before supervision.
/// Hosts with an opened main backend pass its resolved policies through
/// [`supervise_events_daemon_with_policies`].
#[cfg(unix)]
pub async fn supervise_events_daemon(db_path: PathBuf, socket_path: PathBuf) {
    match standalone_daemon_policies(&db_path) {
        Ok((wal_ceiling, disk_guard, volume_lock_dir)) => {
            supervise_events_daemon_with_policies(
                db_path,
                socket_path,
                wal_ceiling,
                disk_guard,
                volume_lock_dir,
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(%error, "invalid events daemon policy; events supervisor not started");
        }
    }
}

/// Supervise using the main pool's captured writer policies and lock directory.
#[cfg(unix)]
pub async fn supervise_events_daemon_with_policies(
    db_path: PathBuf,
    socket_path: PathBuf,
    wal_ceiling: WalCeilingPolicy,
    disk_guard: khive_db::EffectiveDiskGuardConfig,
    volume_lock_dir: PathBuf,
) {
    let (readiness, _unobserved) = events_readiness(socket_path.clone(), EVENTS_READINESS_WINDOW);
    supervise_events_daemon_with_readiness(
        db_path,
        socket_path,
        wal_ceiling,
        disk_guard,
        volume_lock_dir,
        readiness,
    )
    .await;
}

/// [`supervise_events_daemon_with_policies`], reporting the first answer of
/// the events socket through `readiness` so the host can hold requests until
/// then ([`EventsReadiness`]).
///
/// While the gate is waiting, the socket is probed every
/// `FAST_PROBE_INTERVAL` instead of every `PROBE_INTERVAL`, so a request is
/// admitted within milliseconds of the child binding. Once the startup window
/// has elapsed without an answer, it is probed every
/// `UNANSWERED_PROBE_INTERVAL`, so a late bind reopens the gate within a
/// second instead of up to `PROBE_INTERVAL` later. A spawn still happens at
/// most once per `PROBE_INTERVAL`, so a child that keeps failing at startup
/// is respawned no faster than before.
#[cfg(unix)]
pub async fn supervise_events_daemon_with_readiness(
    db_path: PathBuf,
    socket_path: PathBuf,
    wal_ceiling: WalCeilingPolicy,
    disk_guard: khive_db::EffectiveDiskGuardConfig,
    volume_lock_dir: PathBuf,
    readiness: EventsReadinessSignal,
) {
    if let Err(error) = disk_guard.validate() {
        tracing::warn!(%error, "invalid disk policy; events supervisor not started");
        return;
    }
    if let Err(error) = wal_ceiling.validate_static(true, true, false) {
        tracing::warn!(error = %error, "invalid WAL ceiling; events supervisor not started");
        return;
    }
    const PROBE_INTERVAL: Duration = Duration::from_secs(15);
    const FAST_PROBE_INTERVAL: Duration = Duration::from_millis(20);
    const UNANSWERED_PROBE_INTERVAL: Duration = Duration::from_secs(1);
    let shutdown = crate::daemon::daemon_shutdown_token();
    let mut child: Option<std::process::Child> = None;
    let mut respawns: u64 = 0;
    let mut last_spawn: Option<std::time::Instant> = None;
    loop {
        // Reap first: a child that exited (crashed, lost the advisory lock
        // race, refused an untrusted directory) must not linger as a zombie,
        // and clearing the slot is what re-arms the spawn below.
        if let Some(c) = child.as_mut() {
            match c.try_wait() {
                Ok(Some(status)) => {
                    tracing::info!(%status, "events daemon child exited");
                    child = None;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "cannot poll events daemon child; dropping handle");
                    child = None;
                }
            }
        }

        let reachable = match connect_verified(&socket_path).await {
            Ok(_stream) => true,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::warn!(
                    socket = %socket_path.display(),
                    error = %error,
                    "events socket answered by a foreign uid; treating as unreachable"
                );
                false
            }
            Err(_) => false,
        };
        if reachable {
            readiness.mark_ready();
        }
        let spawn_due = last_spawn.is_none_or(|at| at.elapsed() >= PROBE_INTERVAL);
        if !reachable && child.is_none() && spawn_due {
            last_spawn = Some(std::time::Instant::now());
            match std::env::current_exe() {
                Ok(exe) => {
                    let spawned = events_daemon_command(
                        &exe,
                        &db_path,
                        &socket_path,
                        wal_ceiling,
                        disk_guard,
                        &volume_lock_dir,
                    )
                    .spawn();
                    match spawned {
                        Ok(spawned_child) => {
                            respawns += 1;
                            tracing::info!(
                                pid = spawned_child.id(),
                                respawns,
                                socket = %socket_path.display(),
                                "spawned events daemon"
                            );
                            child = Some(spawned_child);
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "failed to spawn events daemon");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "cannot resolve current executable for events daemon spawn");
                }
            }
        }
        let pause = if readiness.awaiting() {
            FAST_PROBE_INTERVAL
        } else if readiness.is_ready() {
            PROBE_INTERVAL
        } else {
            UNANSWERED_PROBE_INTERVAL
        };
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(pause) => {}
        }
    }
    if let Some(mut c) = child.take() {
        // Our child, our cleanup: the events daemon holds no volatile queue
        // state (SQLite is the durability), and the next daemon host's
        // supervisor respawns it.
        let _ = c.kill();
        let _ = c.wait();
        tracing::info!("events daemon child stopped with supervisor shutdown");
    }
}

#[cfg(unix)]
pub(super) fn events_daemon_command(
    executable: &Path,
    db_path: &Path,
    socket_path: &Path,
    wal_ceiling: WalCeilingPolicy,
    disk_guard: khive_db::EffectiveDiskGuardConfig,
    volume_lock_dir: &Path,
) -> std::process::Command {
    let source = match wal_ceiling.source {
        khive_db::WalCeilingSource::BackendField => "backend_field",
        khive_db::WalCeilingSource::Environment => "environment",
        khive_db::WalCeilingSource::Default => "default",
    };
    let mut command = std::process::Command::new(executable);
    command
        .arg("events-daemon")
        .arg("--db")
        .arg(db_path)
        .arg("--socket")
        .arg(socket_path)
        .arg("--wal-ceiling-bytes")
        .arg(wal_ceiling.bytes.to_string())
        .arg("--wal-ceiling-source")
        .arg(source)
        .arg("--disk-reserve-bytes")
        .arg(disk_guard.reserve_bytes.to_string())
        .arg("--disk-guard-deadline-ms")
        .arg(disk_guard.guard_deadline_ms.to_string())
        .arg("--disk-reserve-source")
        .arg(disk_guard.reserve_source.as_str())
        .arg("--disk-deadline-source")
        .arg(disk_guard.deadline_source.as_str())
        .arg("--disk-legacy-environment-present")
        .arg(disk_guard.legacy_environment_present.to_string())
        .arg("--volume-lock-dir")
        .arg(volume_lock_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// Serve the events daemon loop on `socket_path`, owning `db_path`.
///
/// Binds the socket (removing a stale path first), then accepts connections
/// for the process lifetime. Each connection is served sequentially:
/// same-uid admission, then a read-frame → dispatch → write-frame loop until
/// the peer disconnects. All storage goes through `SqlEventStore` on a
/// backend opened read-write against `db_path`; the events schema is ensured
/// once at boot.
///
/// Bind-path trust mirrors the main daemon socket: the socket directory must
/// pass the same ownership/mode/swap-resistance validation, and the bound
/// socket entry is chmod'd 0600 fail-closed. Pathname reachability is not
/// identity — clients additionally verify the peer uid on every connect —
/// but a hardened bind path is what keeps the *bind* itself out of another
/// user's hands.
///
/// This standalone entry resolves its environment once before opening the
/// daemon. Hosts with resolved policies use [`run_events_daemon_with_policies`].
#[cfg(unix)]
pub async fn run_events_daemon(db_path: &Path, socket_path: &Path) -> anyhow::Result<()> {
    let (wal_ceiling, disk_guard, volume_lock_dir) = standalone_daemon_policies(db_path)?;
    run_events_daemon_with_policies(
        db_path,
        socket_path,
        wal_ceiling,
        disk_guard,
        volume_lock_dir,
    )
    .await
}

/// Serve using the exact policies inherited from the opened main backend.
#[cfg(unix)]
pub async fn run_events_daemon_with_policies(
    db_path: &Path,
    socket_path: &Path,
    wal_ceiling: WalCeilingPolicy,
    disk_guard: khive_db::EffectiveDiskGuardConfig,
    volume_lock_dir: PathBuf,
) -> anyhow::Result<()> {
    disk_guard.validate()?;
    wal_ceiling
        .validate_static(true, true, false)
        .map_err(crate::error::RuntimeError::from)?;
    // The subcommand's `--db`/`--socket` arrive from argv and may be
    // relative; anchor them before anything derives a parent from them.
    let db_path = &absolutize(db_path);
    let socket_path = &absolutize(socket_path);
    validate_events_socket_path(socket_path)?;
    // Directory trust comes FIRST: the lock guard below opens and chmods a
    // path in this directory, and validating only before the later bind
    // would let those operations run in a directory another user controls.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        crate::daemon::ensure_socket_dir_is_trusted(parent)?;
    }
    let _guard = match acquire_events_daemon_guard_outcome(socket_path) {
        EventsDaemonGuardAcquisition::Held(guard) => guard,
        refusal => {
            tracing::info!(
                socket = %socket_path.display(),
                reason = ?refusal,
                "events daemon lock unavailable; exiting"
            );
            return Ok(());
        }
    };
    ensure_events_db_owner_only(db_path)?;
    // Tighten a pre-existing database and any `-wal`/`-shm` an earlier
    // process left behind BEFORE SQLite opens the database. The order is
    // load-bearing: hardening opens and closes its own descriptor on each of
    // these files, and POSIX advisory locks are per process and per inode,
    // released by the close of ANY descriptor for the inode (fcntl(2)). Run
    // after the open, it silently dropped the SHARED lock SQLite keeps on the
    // database in WAL mode and the shared lock on the `-shm` DMS byte, so the
    // next external connection to close (a backup tool, an inspection shell)
    // took itself for the last connection, checkpointed, and unlinked the
    // sidecars underneath this daemon, which kept writing to the unlinked
    // inodes. Sidecars SQLite creates from here on inherit the database
    // file's mode; the check after the open below opens nothing.
    let before_open = harden_events_db_sidecars(db_path)?;
    let backend = Arc::new(
        StorageBackend::sqlite_with_max_readers_and_policies(
            db_path,
            None,
            wal_ceiling,
            disk_guard,
            volume_lock_dir,
        )
        .map_err(crate::error::RuntimeError::from)?,
    );
    // Ensure the schema once, loudly, before accepting traffic.
    backend.events()?;
    verify_events_db_owner_only_unopened(db_path, &before_open)?;

    // Integration tests widen the window between the main daemon accepting
    // requests and this socket answering, which a slow host produces.
    if let Some(delay) = harness_duration_ms("KHIVE_TEST_EVENTS_DAEMON_BIND_DELAY_MS") {
        tracing::info!(
            delay_ms = delay.as_millis() as u64,
            "test harness: delaying events socket bind"
        );
        tokio::time::sleep(delay).await;
    }
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        // Fail closed, same contract as the main daemon socket: if the entry
        // cannot be made owner-only, drop the listener and remove it rather
        // than serve a world-reachable socket the design never covered.
        if let Err(e) =
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        {
            drop(listener);
            let _ = std::fs::remove_file(socket_path);
            anyhow::bail!(
                "refusing to serve events: cannot chmod 0600 {}: {e}. The events socket must \
                 be owner-only.",
                socket_path.display()
            );
        }
    }
    let daemon_euid = unsafe { libc::geteuid() };
    // Per-namespace store cache: `events_for_namespace` takes a writer-lane
    // checkout and re-runs the schema DDL on every call, so paying it once
    // per namespace instead of once per request keeps the writer lane for
    // actual writes. The namespace is client-supplied, so the cache is
    // bounded (`MAX_CACHED_NAMESPACE_STORES`) rather than trusted small.
    let stores: NamespaceStores = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_EVENTS_CONNECTIONS));
    let frame_budget = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_REQUEST_BYTES));
    tracing::info!(
        socket = %socket_path.display(),
        db = %db_path.display(),
        wal_ceiling_configured_bytes = wal_ceiling.bytes,
        wal_ceiling_effective_bytes = wal_ceiling.effective_bytes(backend.is_read_only()),
        wal_ceiling_source = ?wal_ceiling.source,
        "events daemon listening"
    );

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(error = %error, "events daemon accept failed");
                continue;
            }
        };
        match crate::daemon::peer_uid(&stream) {
            Ok(uid) if crate::daemon::uid_is_permitted(uid, daemon_euid) => {}
            Ok(uid) => {
                tracing::warn!(peer_uid = uid, "events daemon rejected foreign-uid peer");
                continue;
            }
            Err(error) => {
                tracing::warn!(error = %error, "events daemon could not read peer credentials");
                continue;
            }
        }
        let permit = match Arc::clone(&connections).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // At the cap: drop the stream (peer sees EOF and retries via
                // its own reconnect path) instead of queueing unbounded work.
                tracing::warn!(
                    cap = MAX_EVENTS_CONNECTIONS,
                    "events daemon at connection cap; dropping new connection"
                );
                continue;
            }
        };
        let backend = Arc::clone(&backend);
        let stores = Arc::clone(&stores);
        let frame_budget = Arc::clone(&frame_budget);
        crate::daemon::spawn_named_tracked_task("events_connection", async move {
            serve_events_conn(stream, backend, stores, frame_budget).await;
            drop(permit);
        });
    }
}
