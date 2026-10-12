//! Daemon startup, rendezvous validation, and serving lifecycle.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use khive_db::{run_checkpoint_task, CheckpointConfig};
use tokio::net::UnixListener;

use super::{
    acquire_daemon_boot_guard, acquire_recovery_lock, admit_or_refuse_busy, checkpoint_task_specs,
    cleanup_stale_daemon, daemon_shutdown_token, drain, drain_for_idle, drain_timeout,
    ensure_rendezvous_overrides_paired, finish_connection_tasks, handle_conn_with_lifecycle,
    khive_dir, next_accept_error_backoff, peer_uid, pid_file_names_a_reachable_daemon, pid_path,
    remove_pid_file_if_owned, shutdown_cleanup_if_owned, socket_identity, socket_path,
    spawn_connection_task, track_named_background_task, uid_is_permitted, wait_for_idle,
    write_pid_file_exclusive, DaemonDispatch, DaemonLifecycle, DaemonOptions, DaemonShutdownReason,
    DaemonStartupReport, Incumbent, INITIAL_FRAME_READ_TIMEOUT, PID_PATH_ENV, SOCKET_PATH_ENV,
};

/// Run the daemon: bind the socket, warm in the background, serve request
/// frames until SIGTERM/SIGINT.
///
/// Fatally acquires its own startup lock, which only protects
/// cleanup→pid-claim→bind — `dispatcher` has already run migrations and
/// applied pack schema plans while constructing itself, unguarded. Production
/// boot must go through [`run_daemon_with_boot_guard`] instead, which extends
/// the same lock back over construction. This entry point is for callers
/// (and tests) that build the dispatcher and start serving as one atomic
/// step with no separate boot-guard window to protect.
#[cfg(unix)]
pub async fn run_daemon<D: DaemonDispatch>(dispatcher: D) -> anyhow::Result<()> {
    let boot_guard = Some(acquire_daemon_boot_guard()?);
    run_daemon_with_boot_guard_inner(
        dispatcher,
        boot_guard,
        false,
        DaemonOptions::default(),
        |_| DaemonStartupReport::default(),
    )
    .await
}

/// Run a real daemon server for an in-process multi-launch test.
///
/// Separate production daemon candidates have distinct PIDs, so the boot fence
/// recognizes a live incumbent and makes later candidates exit. Parallel
/// test launchers share one OS process and therefore one PID; this explicit
/// fault-injection entry point preserves the production fence semantics by
/// allowing a live same-PID incumbent to win. Ordinary daemon startup
/// continues to treat a same-PID rendezvous as stale, protecting PID-reuse
/// cleanup behavior.
///
/// A losing candidate still follows the ordinary daemon-exit path and cancels
/// the process-wide component shutdown token. Callers must therefore use a
/// component-free dispatcher; this seam validates socket/PID ownership, not
/// multi-candidate component lifecycle.
#[cfg(all(unix, any(test, feature = "fault-injection")))]
#[doc(hidden)]
pub async fn run_daemon_in_process_test<D: DaemonDispatch>(dispatcher: D) -> anyhow::Result<()> {
    let boot_guard = Some(acquire_daemon_boot_guard()?);
    run_daemon_with_boot_guard_inner(
        dispatcher,
        boot_guard,
        true,
        DaemonOptions::default(),
        |_| DaemonStartupReport::default(),
    )
    .await
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RendezvousPathRole {
    Socket,
    PidFile,
}

#[cfg(unix)]
impl RendezvousPathRole {
    fn env_name(self) -> &'static str {
        match self {
            Self::Socket => SOCKET_PATH_ENV,
            Self::PidFile => PID_PATH_ENV,
        }
    }

    fn directory_name(self) -> &'static str {
        match self {
            Self::Socket => "socket directory",
            Self::PidFile => "PID-file directory",
        }
    }

    fn path_name(self) -> &'static str {
        match self {
            Self::Socket => "socket path",
            Self::PidFile => "PID-file path",
        }
    }

    fn path_component_name(self) -> &'static str {
        match self {
            Self::Socket => "socket-path",
            Self::PidFile => "PID-file-path",
        }
    }
}

/// Vet the socket's parent directory, re-permissioning it only when it is the
/// directory khive owns by convention.
///
/// `KHIVE_SOCKET` takes an arbitrary path, and the previous unconditional
/// chmod-0700 of its parent was wrong in both directions: pointed at a shared
/// parent like `/tmp` it either failed outright for an ordinary user, or —
/// worse — succeeded when privileged and stripped access for every other
/// process on the machine. A directory we did not create is never modified.
///
/// What the directory must actually prevent is a *takeover of the socket
/// path*: the connection gate is the 0600 socket plus the accept-time
/// peer-uid check, but both defend the daemon's own socket — neither helps
/// once another local user can put *their* listener at the path clients
/// resolve. There are two ways a shared directory allows that, and the
/// sticky bit closes neither: a writer can *pre-bind* the predictable path
/// before this daemon starts (the sticky bit restricts unlinking, not
/// creating), and the directory's *owner* can unlink and rebind even in a
/// 1777 directory (sticky exempts the directory owner). So a caller-chosen
/// directory is served only when it is trusted end to end: owned by this
/// daemon's euid or root, and not writable by group or other at all. The
/// umask-default 0755 stays acceptable; shared sticky directories like
/// `/tmp` do not.
#[cfg(unix)]
pub(crate) fn ensure_socket_dir_is_trusted(parent: &std::path::Path) -> anyhow::Result<()> {
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;
    ensure_rendezvous_dir_is_trusted(parent, RendezvousPathRole::Socket, daemon_euid, true)
}

/// Vet the parent directory of a PID file before reading, locking, or writing
/// it. The file's lock only protects the inode currently named by its path;
/// every directory component must therefore be as swap-resistant as the
/// socket rendezvous.
#[cfg(unix)]
pub fn ensure_pid_file_dir_is_trusted(pid_file: &std::path::Path) -> anyhow::Result<()> {
    let parent = pid_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;
    ensure_rendezvous_dir_is_trusted(parent, RendezvousPathRole::PidFile, daemon_euid, false)
}

#[cfg(unix)]
pub(super) fn ensure_rendezvous_dir_is_trusted(
    parent: &std::path::Path,
    role: RendezvousPathRole,
    daemon_euid: u32,
    repair_owned_default: bool,
) -> anyhow::Result<()> {
    let env_name = role.env_name();
    let directory_name = role.directory_name();

    if repair_owned_default && parent == khive_dir() {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            anyhow::anyhow!(
                "refusing to start: cannot chmod 0700 {}: {e}. The khive directory must be \
                 owner-only as the {directory_name} for {env_name}; it is part of the \
                 same-uid guarantee this daemon enforces.",
                parent.display()
            )
        })?;
        return ensure_rendezvous_path_is_swap_resistant(parent, daemon_euid, role);
    }

    // Fail closed on the stat itself: not being able to read the metadata is
    // not the same as the directory passing.
    let meta = std::fs::metadata(parent).map_err(|e| {
        anyhow::anyhow!(
            "refusing to start: cannot stat {directory_name} {} for {env_name}: {e}. \
             It gates rendezvous-path safety, and unreadable metadata is not a passing state.",
            parent.display()
        )
    })?;

    use std::os::unix::fs::MetadataExt;
    let owner = meta.uid();
    if owner != daemon_euid && owner != 0 {
        anyhow::bail!(
            "refusing to start: {directory_name} {} for {env_name} is owned by uid {owner}, \
             not this daemon's uid ({daemon_euid}) or root. A directory owner can replace \
             the rendezvous path regardless of mode bits. Point {env_name} at a directory \
             you own, or unset it for the default.",
            parent.display()
        );
    }

    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        anyhow::bail!(
            "refusing to start: {directory_name} {} for {env_name} is mode {:04o} — writable \
             by group or other, so another local user could replace the rendezvous path. \
             Use a directory only you can write, or unset {env_name} for the default. \
             This daemon is not changing the permissions of a directory it does not own.",
            parent.display(),
            mode & 0o7777
        );
    }

    ensure_rendezvous_path_is_swap_resistant(parent, daemon_euid, role)
}

/// Socket-role form of [`ensure_rendezvous_path_is_swap_resistant`], used by the
/// socket-path tests.
#[cfg(all(unix, test))]
pub(super) fn ensure_socket_path_is_swap_resistant(
    parent: &std::path::Path,
    daemon_euid: u32,
) -> anyhow::Result<()> {
    ensure_rendezvous_path_is_swap_resistant(parent, daemon_euid, RendezvousPathRole::Socket)
}

/// Walk the socket directory path exactly as the kernel will traverse it at
/// bind time — component by component, following symlinks — and refuse any
/// node another local user could swap after this validation.
///
/// Checking only the canonicalized result is not enough: bind and every
/// client traverse the *original* path, so a symlink component owned by
/// another user can resolve somewhere trusted while it is being validated
/// and be retargeted before the socket is bound. Validating the nodes the
/// traversal actually visits closes that gap: a symlink component is
/// acceptable only when its owner — the only party besides root who can
/// retarget it — is this daemon's euid or root, and every directory
/// component is held to the ancestor rule below.
///
/// The directory rule is deliberately weaker than the immediate parent's.
/// The parent hosts the socket file, where the threat is *creation* of the
/// predictable path — the sticky bit does not restrict creating, so no
/// sticky exception is sound there. An ancestor only threatens via
/// *rename/unlink of an existing entry we own*, which the sticky bit does
/// restrict: in a sticky directory, only the entry's owner, the directory's
/// owner, or root may rename it. So a root-owned `/tmp` (1777) is an
/// acceptable ancestor of a user-owned 0700 socket directory, while a
/// non-sticky group/other-writable ancestor, or one owned by a third uid
/// (who could rename the entry, or chmod the directory first), is refused.
/// Every stat failure fails closed.
#[cfg(unix)]
fn ensure_rendezvous_path_is_swap_resistant(
    parent: &std::path::Path,
    daemon_euid: u32,
    role: RendezvousPathRole,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let env_name = role.env_name();
    let directory_name = role.directory_name();
    let path_name = role.path_name();
    let component_name = role.path_component_name();

    let absolute = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| {
                anyhow::anyhow!(
                    "refusing to start: cannot resolve the working directory to absolutize \
                     {directory_name} {} for {env_name}: {e}.",
                    parent.display()
                )
            })?
            .join(parent)
    };

    fn push_components(stack: &mut Vec<std::ffi::OsString>, path: &std::path::Path) {
        let components: Vec<_> = path
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        stack.extend(components.into_iter().rev());
    }

    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    push_components(&mut stack, &absolute);
    let mut resolved = std::path::PathBuf::new();
    let mut symlinks_followed = 0u32;

    while let Some(component) = stack.pop() {
        if component == "/" {
            resolved = std::path::PathBuf::from("/");
            continue;
        }
        if component == "." {
            continue;
        }
        if component == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&component);
        let meta = std::fs::symlink_metadata(&candidate).map_err(|e| {
            anyhow::anyhow!(
                "refusing to start: cannot stat {component_name} component {} for {env_name}: \
                 {e}. An unreadable component is not a passing one.",
                candidate.display()
            )
        })?;
        let owner = meta.uid();

        if meta.file_type().is_symlink() {
            symlinks_followed += 1;
            if symlinks_followed > 40 {
                anyhow::bail!(
                    "refusing to start: {path_name} for {env_name} resolves through more than \
                     40 symlinks at {} — treating this as a loop.",
                    candidate.display()
                );
            }
            if owner != daemon_euid && owner != 0 {
                anyhow::bail!(
                    "refusing to start: {component_name} symlink component {} for {env_name} \
                     is owned by uid {owner}, not this daemon's uid ({daemon_euid}) or root — \
                     its owner could retarget it after this check and re-root the {path_name}. \
                     Point {env_name} somewhere trusted end to end, or unset it for the default.",
                    candidate.display()
                );
            }
            let target = std::fs::read_link(&candidate).map_err(|e| {
                anyhow::anyhow!(
                    "refusing to start: cannot read {component_name} symlink component {} \
                     for {env_name}: {e}.",
                    candidate.display()
                )
            })?;
            push_components(&mut stack, &target);
            continue;
        }

        if meta.is_dir() {
            let mode = meta.permissions().mode();
            let sticky = mode & 0o1000 != 0;
            if owner != daemon_euid && owner != 0 {
                anyhow::bail!(
                    "refusing to start: {component_name} ancestor {} for {env_name} is owned by \
                     uid {owner}, not this daemon's uid ({daemon_euid}) or root — its owner \
                     could rename the next path component and re-root the {path_name}. Point \
                     {env_name} somewhere trusted end to end, or unset it for the default.",
                    candidate.display()
                );
            }
            if mode & 0o022 != 0 && !sticky {
                anyhow::bail!(
                    "refusing to start: {component_name} ancestor {} for {env_name} is mode \
                     {:04o} — writable by group or other without the sticky bit, so another \
                     local user could rename the next path component and re-root the {path_name}. \
                     Point {env_name} somewhere trusted end to end, or unset it for the default.",
                    candidate.display(),
                    mode & 0o7777
                );
            }
            resolved = candidate;
            continue;
        }

        anyhow::bail!(
            "refusing to start: {component_name} component {} for {env_name} is neither a \
             directory nor a symlink — the {path_name} cannot traverse it.",
            candidate.display()
        );
    }

    Ok(())
}

/// Run the daemon using a startup lock acquired by the caller *before*
/// building `dispatcher`, so a second process racing to boot (e.g. two
/// `kkernel mcp --daemon` spawns before either has bound its socket) cannot
/// run migrations/FTS DDL concurrently against the same database file.
/// `boot_guard` is only `None` on non-unix targets, where there is no
/// advisory boot lock to hold in the first place; every unix daemon-mode
/// caller passes `Some`.
///
/// The guard is held across cleanup → pid-claim → bind, then dropped. The
/// caller must not still be holding a *different* handle to the same lock
/// file when this function is entered — see the "Deadlock note" on the
/// `_startup_lock` binding below for why that would self-deadlock on `flock`.
#[cfg(unix)]
pub async fn run_daemon_with_boot_guard<D: DaemonDispatch>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
) -> anyhow::Result<()> {
    run_daemon_with_boot_guard_inner(
        dispatcher,
        boot_guard,
        false,
        DaemonOptions::default(),
        |_| DaemonStartupReport::default(),
    )
    .await
}

/// Run the daemon and start host-owned background work only after the socket
/// is bound, permissions are restricted, and this process owns the PID file.
/// The callback runs once while the startup lock and teardown guard are held;
/// setup failures never invoke it. Work started by the callback must use the
/// daemon shutdown token and tracked-task drain contract.
#[cfg(unix)]
pub async fn run_daemon_with_boot_guard_and_start<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) + Send,
{
    run_daemon_with_options_and_boot_guard_and_start(
        dispatcher,
        boot_guard,
        DaemonOptions::default(),
        |dispatcher| {
            start(dispatcher);
            DaemonStartupReport::default()
        },
    )
    .await
}

/// Start with an explicit launch mode and collect the host's startup inventory.
#[cfg(unix)]
pub async fn run_daemon_with_options_and_boot_guard_and_start<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    options: DaemonOptions,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) -> DaemonStartupReport + Send,
{
    anyhow::ensure!(
        !options.idle_interval.is_zero(),
        "daemon idle interval must be positive"
    );
    run_daemon_with_boot_guard_inner(dispatcher, boot_guard, false, options, start).await
}

#[cfg(unix)]
async fn run_daemon_with_boot_guard_inner<D, F>(
    dispatcher: D,
    boot_guard: Option<std::fs::File>,
    allow_same_process_incumbent: bool,
    options: DaemonOptions,
    start: F,
) -> anyhow::Result<()>
where
    D: DaemonDispatch,
    F: FnOnce(&D) -> DaemonStartupReport + Send,
{
    // Cancel on every exit, including setup failure and unwinding from the
    // post-ownership startup callback. The guard precedes all fallible work
    // so even a process that never becomes the daemon relinquishes its
    // process-lifetime shutdown token; restarting requires exec.
    struct ComponentTeardown;
    impl Drop for ComponentTeardown {
        fn drop(&mut self) {
            daemon_shutdown_token().cancel();
        }
    }
    let _component_teardown = ComponentTeardown;

    // Placed after the teardown guard so this refusal keeps the ADR-119
    // contract every other pre-bind error path has, and before the paths are
    // resolved so a split rendezvous never reaches cleanup/bind/pid-write.
    ensure_rendezvous_overrides_paired()?;

    let sock = socket_path();
    let pid_file = pid_path();
    let socket_parent = sock.parent();
    let pid_parent = pid_file.parent();

    if let Some(parent) = socket_parent {
        std::fs::create_dir_all(parent)?;
        ensure_socket_dir_is_trusted(parent)?;
    }
    // Identical parent paths traverse the same components, so the socket
    // check above also vets the PID-file parent. Aliased paths are checked
    // independently because each original path is traversed by file access.
    if pid_parent != socket_parent {
        ensure_pid_file_dir_is_trusted(&pid_file)?;
    }

    // Hold the startup lock across cleanup → pid-claim → bind so a concurrent
    // client's kill_and_respawn (which also holds this lock) cannot remove the
    // rendezvous paths during setup. The PID file's own lock is retained after
    // this shared startup lock is released, including while the listener drains
    // during shutdown.
    //
    // Deadlock note: the client holds this lock only during kill+spawn and
    // releases it before the spawned daemon process starts (the lock guard is
    // dropped when kill_and_respawn returns, before the readiness probe loop).
    // The daemon holds exactly one handle to this lock for its whole boot
    // sequence (received as `boot_guard`, extended from before `dispatcher`
    // was constructed) — never a second, independently-acquired handle in the
    // same process, which would self-deadlock on `flock`.
    let _startup_lock = boot_guard;

    // A second daemon must refuse loudly rather than exit successfully while
    // another daemon owns this rendezvous. Only `Stale` lets the caller proceed.
    match cleanup_stale_daemon(
        &sock,
        &pid_file,
        allow_same_process_incumbent,
        dispatcher.config_id(),
    )
    .await
    {
        Incumbent::Serving(incumbent_pid) => {
            tracing::error!(
                pid = incumbent_pid,
                socket = ?sock,
                "refusing to start: a khived instance is already serving this socket"
            );
            anyhow::bail!(
                "refusing to start: khived is already running as pid {incumbent_pid}, \
                 serving socket {}. Stop that instance first if you intend to replace it.",
                sock.display()
            );
        }
        Incumbent::Live(incumbent_pid) => {
            tracing::error!(
                pid = incumbent_pid,
                socket = ?sock,
                "refusing to start: a live process owns the PID file but no khived answered"
            );
            anyhow::bail!(
                "refusing to start: pid {incumbent_pid} owns the daemon PID file and is alive, \
                 but nothing answered the khived protocol on {}. It may be draining. Nothing \
                 was removed; stop that process first if you intend to replace it.",
                sock.display()
            );
        }
        Incumbent::Stale => {}
    }

    // Install signal streams before publishing either rendezvous file. A
    // supervisor may stop us as soon as connect/pid checks succeed, before
    // the accept loop or shutdown future has been polled. Keep these streams
    // alive so a signal during the rest of startup reaches normal cleanup.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let pid_file_guard = match write_pid_file_exclusive(&pid_file) {
        Ok(guard) => guard,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // A PID file appeared between cleanup and our claim. Never touch
            // the winner's files; defer only if it already answers as khived.
            if pid_file_names_a_reachable_daemon(
                &pid_file,
                &sock,
                allow_same_process_incumbent,
                dispatcher.config_id(),
            )
            .await
            {
                tracing::info!(
                    "a replacement khived already claimed the pid/socket rendezvous; exiting"
                );
                return Ok(());
            }
            anyhow::bail!(
                "failed to claim daemon pid file at {pid_file:?}: it already exists \
                 and does not name a reachable daemon"
            );
        }
        Err(e) => return Err(e.into()),
    };

    let listener = match UnixListener::bind(&sock) {
        Ok(listener) => listener,
        Err(e) => {
            remove_pid_file_if_owned(&pid_file, &pid_file_guard);
            return Err(e.into());
        }
    };
    // Fail closed, same reason as the directory above. If this chmod fails the
    // socket is world-reachable in a way the accepted design never covered, so
    // the bound listener is dropped and the entry removed rather than served.
    if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let _ = std::fs::remove_file(&sock);
        remove_pid_file_if_owned(&pid_file, &pid_file_guard);
        return Err(anyhow::anyhow!(
            "refusing to start: cannot chmod 0600 {}: {e}. The daemon socket must be owner-only \
             — it is half of the single-principal guarantee this daemon enforces.",
            sock.display()
        ));
    }
    // Captured while still holding the startup lock, immediately after
    // bind, so shutdown cleanup can later prove "this is still the same socket
    // I bound" rather than trusting the path alone.
    let bound_identity = socket_identity(&sock);

    let lifecycle = Arc::new(DaemonLifecycle::new(options, start(&dispatcher)));

    // Release the shared startup lock now that the listener is bound. The
    // locked PID file continues to identify this daemon through shutdown.
    drop(_startup_lock);
    tracing::info!(
        socket = ?sock,
        pid = std::process::id(),
        source_revision = crate::BUILD_INFO.source_revision,
        build_time = crate::BUILD_INFO.build_time,
        "khived listening"
    );

    {
        let warm = dispatcher.clone();
        track_named_background_task("daemon_warmup", async move {
            warm.warm_all().await;
        });
    }

    // The checkpoint task's own strong-count-based exit is unreachable
    // whenever `event_store_for_checkpoint()` returns `Some` (the ordinary
    // production shape), because the `SqlEventStore` it wraps retains its
    // own clone of the same pool. An explicit watch channel replaces that
    // mechanism: the sender is held for the remainder of this function's
    // scope and signalled as the first action once shutdown is observed,
    // below.
    let (checkpoint_shutdown_tx, checkpoint_shutdown_rx) = tokio::sync::watch::channel(());
    // ADR-091 Amendment 3: one checkpoint task per file-backed backend the
    // dispatcher wired — the primary pool plus every entry
    // `secondary_pools_for_checkpoint` returns — sharing this one shutdown
    // channel (the sender broadcasts to every receiver clone), so the single
    // send below stops every spawned task before `drain()`.
    let checkpoint_tasks = checkpoint_task_specs(
        dispatcher.pool_for_checkpoint(),
        dispatcher.secondary_pools_for_checkpoint(),
        dispatcher.event_store_for_checkpoint(),
        dispatcher.namespace().to_string(),
    );
    if !checkpoint_tasks.is_empty() {
        let cfg = CheckpointConfig::from_env();
        let checkpoint_task_count = checkpoint_tasks.len();
        for task in checkpoint_tasks {
            track_named_background_task(
                "wal_checkpoint",
                run_checkpoint_task(
                    task.pool,
                    cfg.clone(),
                    task.lifecycle_owner,
                    checkpoint_shutdown_rx.clone(),
                    task.is_main,
                ),
            );
        }
        tracing::info!(checkpoint_task_count, "WAL checkpoint task(s) started");
    }

    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connection_tasks = Arc::new(std::sync::Mutex::new(
        Vec::<tokio::task::JoinHandle<()>>::new(),
    ));
    let (request_shutdown_tx, request_shutdown_rx) = tokio::sync::watch::channel(false);

    let shutdown = async {
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
        }
        // Tokio retains its process-wide handlers after the streams are dropped.
        // This daemon cannot restart without exec; a repeat signal must terminate
        // even if shutdown is blocked in synchronous recovery-lock acquisition.
        for signal in [libc::SIGTERM, libc::SIGINT] {
            // SAFETY: setting SIG_DFL for these valid signals needs no handler
            // pointer or shared Rust state and applies to the whole process.
            if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::pin!(shutdown);
    lifecycle.ready();

    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;

    let reason = tokio::select! {
        _ = async {
            let mut accept_error_backoff = None;
            let mut last_accept_error_log: Option<std::time::Instant> = None;
            loop {
                match listener.accept().await {
                    Ok((mut stream, _)) => {
                        let initial_frame_deadline =
                            tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT;
                        accept_error_backoff = None;
                        last_accept_error_log = None;
                        // Refuse a foreign uid before any frame is read.
                        // Fails CLOSED: an error reading peer credentials is
                        // "cannot prove same-uid", which is the same answer as
                        // "is not same-uid" — never a pass.
                        //
                        // Scope: this is a same-UID check, which is strictly
                        // weaker than same-PRINCIPAL. Several distinct actors
                        // running under one uid all pass here, so this does not
                        // by itself establish that the daemon serves a single
                        // principal, and nothing downstream refuses or degrades
                        // on observing more than one. Do not describe it as a
                        // multi-principal guard — it bounds the process boundary,
                        // not the identity one.
                        match peer_uid(&stream) {
                            Ok(peer) if uid_is_permitted(peer, daemon_euid) => {}
                            Ok(peer) => {
                                tracing::error!(
                                    peer_uid = peer,
                                    daemon_euid,
                                    "refusing connection from a foreign uid: this daemon accepts \
                                     only peers running as its own uid"
                                );
                                drop(stream);
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "refusing connection: cannot read peer credentials, so \
                                     same-uid cannot be proven"
                                );
                                drop(stream);
                                continue;
                            }
                        }
                        // One permit per connection, taken before the task is
                        // spawned. Past the cap the peer is answered with a busy
                        // error and the stream is dropped; connections already
                        // admitted keep their permits and are not affected.
                        let Some(permit) = admit_or_refuse_busy(
                            &lifecycle.connections,
                            &mut stream,
                            dispatcher.config_id(),
                        )
                        .await
                        else {
                            continue;
                        };
                        // Keep the acceptance-time deadline across the
                        // credential check and connection-task scheduling.
                        let d = dispatcher.clone();
                        let shutdown = request_shutdown_rx.clone();
                        let lifecycle = Arc::clone(&lifecycle);
                        let handle = spawn_connection_task(Arc::clone(&active), async move {
                            // Released when the handler ends, whether it returns,
                            // panics or is aborted at shutdown.
                            let _permit = permit;
                            handle_conn_with_lifecycle(
                                stream,
                                d,
                                Some(shutdown),
                                initial_frame_deadline,
                                Some(lifecycle),
                            )
                            .await;
                        });
                        let mut tasks = connection_tasks
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        tasks.retain(|task| !task.is_finished());
                        tasks.push(handle);
                    }
                    Err(e) => {
                        let delay = next_accept_error_backoff(accept_error_backoff);
                        accept_error_backoff = Some(delay);
                        let capacity_exhausted = matches!(
                            e.raw_os_error(),
                            Some(libc::EMFILE) | Some(libc::ENFILE)
                        );
                        if last_accept_error_log.is_none_or(|last| {
                            last.elapsed() >= std::time::Duration::from_secs(30)
                        }) {
                            tracing::error!(
                                error = %e,
                                capacity_exhausted,
                                retry_ms = delay.as_millis(),
                                "daemon accept failed; retrying with bounded backoff"
                            );
                            last_accept_error_log = Some(std::time::Instant::now());
                        }
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        } => DaemonShutdownReason::Signal,
        result = &mut shutdown => { result?; DaemonShutdownReason::Signal },
        _ = wait_for_idle(&dispatcher, &lifecycle) => DaemonShutdownReason::Idle,
    };

    lifecycle.draining(reason);

    // A listening backlog is not admitted work. Close it before draining so
    // new clients cannot finish writing to a socket nobody will accept.
    drop(listener);

    // Signal the checkpoint task to exit before draining, so `drain()`
    // actually waits on it via `track_background_task` rather than the
    // task outliving the drain window (or the process) unsignalled.
    let _ = checkpoint_shutdown_tx.send(());

    // Per-run signal: read scopes stop promptly, admitted writes ignore it and
    // retain the rest of the configured drain window to commit or roll back.
    if reason == DaemonShutdownReason::Signal {
        let _ = request_shutdown_tx.send(true);
    }

    // Same ordering contract for ADR-119 daemon components: cancel before
    // drain, so each component's supervisor (itself a tracked task) can run
    // its bounded shutdown inside the drain wait.
    daemon_shutdown_token().cancel();

    let drained = if reason == DaemonShutdownReason::Idle {
        tokio::select! {
            _ = drain_for_idle(&active, drain_timeout()) => true,
            result = &mut shutdown => {
                result?;
                lifecycle.draining(DaemonShutdownReason::Signal);
                let _ = request_shutdown_tx.send(true);
                drain(&active).await
            }
        }
    } else {
        drain(&active).await
    };
    let tasks = {
        let mut retained = connection_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *retained)
    };
    finish_connection_tasks(tasks, drained).await;

    // A concurrent client's `kill_and_respawn` may have already decided
    // this daemon looked stale, killed it, and spawned a replacement that
    // bound the same socket/PID paths while this daemon was draining above.
    // Reacquire the recovery lock (the same one that serializes startup) and
    // only unlink if the PID file still names this process AND the socket at
    // `sock` is still the exact one this daemon bound — otherwise a
    // replacement daemon owns those paths now and unlinking would delete its
    // live socket/PID out from under it.
    match acquire_recovery_lock() {
        Some(_shutdown_lock) => {
            shutdown_cleanup_if_owned(&sock, &pid_file, bound_identity);
        }
        None => {
            tracing::warn!(
                "could not acquire recovery lock for shutdown cleanup; \
                 skipping unlink to avoid deleting a replacement daemon's paths"
            );
        }
    }
    lifecycle.stopped();
    tracing::info!("khived stopped");
    Ok(())
}
