#![cfg(unix)]
use super::*;

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
pub(super) enum RendezvousPathRole<'a> {
    Socket,
    PidFile,
    Store(&'a std::path::Path),
}

#[cfg(unix)]
impl RendezvousPathRole<'_> {
    pub(super) fn refusal(
        self,
        rendezvous: std::fmt::Arguments<'_>,
        store: std::fmt::Arguments<'_>,
    ) -> anyhow::Error {
        match self {
            Self::Store(db_path) => anyhow::anyhow!(
                "refusing to open events database {}: {store}. Check the offending directory's \
                 ownership and permissions (remove group/other write access), or move the store \
                 to a location trusted end to end. Only the first offending component is \
                 reported; further untrusted ancestors or components may remain.",
                db_path.display()
            ),
            Self::Socket | Self::PidFile => anyhow::anyhow!("{rendezvous}"),
        }
    }

    pub(super) fn env_name(self) -> &'static str {
        match self {
            Self::Socket => SOCKET_PATH_ENV,
            Self::PidFile => PID_PATH_ENV,
            Self::Store(_) => "events database",
        }
    }

    pub(super) fn directory_name(self) -> &'static str {
        match self {
            Self::Socket => "socket directory",
            Self::PidFile => "PID-file directory",
            Self::Store(_) => "store directory",
        }
    }

    pub(super) fn path_name(self) -> &'static str {
        match self {
            Self::Socket => "socket path",
            Self::PidFile => "PID-file path",
            Self::Store(_) => "store path",
        }
    }

    pub(super) fn path_component_name(self) -> &'static str {
        match self {
            Self::Socket => "socket-path",
            Self::PidFile => "PID-file-path",
            Self::Store(_) => "store-path",
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

/// Apply the existing directory trust and default-root repair rules to an events store.
#[cfg(unix)]
pub(crate) fn ensure_store_dir_is_trusted(
    parent: &std::path::Path,
    db_path: &std::path::Path,
) -> anyhow::Result<()> {
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;
    ensure_rendezvous_dir_is_trusted(
        parent,
        RendezvousPathRole::Store(db_path),
        daemon_euid,
        true,
    )
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
    role: RendezvousPathRole<'_>,
    daemon_euid: u32,
    repair_owned_default: bool,
) -> anyhow::Result<()> {
    let env_name = role.env_name();
    let directory_name = role.directory_name();

    if repair_owned_default && parent == khive_dir() {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            role.refusal(
                format_args!(
                    "refusing to start: cannot chmod 0700 {}: {e}. The khive directory must be \
                 owner-only as the {directory_name} for {env_name}; it is part of the \
                 same-uid guarantee this daemon enforces.",
                    parent.display()
                ),
                format_args!(
                    "cannot chmod 0700 store directory {}: {e}",
                    parent.display()
                ),
            )
        })?;
        return ensure_rendezvous_path_is_swap_resistant(parent, daemon_euid, role);
    }

    // Fail closed on the stat itself: not being able to read the metadata is
    // not the same as the directory passing.
    let meta = std::fs::metadata(parent).map_err(|e| {
        role.refusal(
            format_args!(
                "refusing to start: cannot stat {directory_name} {} for {env_name}: {e}. \
             It gates rendezvous-path safety, and unreadable metadata is not a passing state.",
                parent.display()
            ),
            format_args!(
                "cannot stat store directory {}: {e}; unreadable metadata is not a passing state",
                parent.display()
            ),
        )
    })?;

    use std::os::unix::fs::MetadataExt;
    let owner = meta.uid();
    if owner != daemon_euid && owner != 0 {
        return Err(role.refusal(
            format_args!(
                "refusing to start: {directory_name} {} for {env_name} is owned by uid {owner}, \
             not this daemon's uid ({daemon_euid}) or root. A directory owner can replace \
             the rendezvous path regardless of mode bits. Point {env_name} at a directory \
             you own, or unset it for the default.",
                parent.display()
            ),
            format_args!(
                "store directory {} is owned by uid {owner}, not this process's uid \
                 ({daemon_euid}) or root; its owner can replace the database path",
                parent.display()
            ),
        ));
    }

    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        return Err(role.refusal(
            format_args!(
                "refusing to start: {directory_name} {} for {env_name} is mode {:04o} — writable \
             by group or other, so another local user could replace the rendezvous path. \
             Use a directory only you can write, or unset {env_name} for the default. \
             This daemon is not changing the permissions of a directory it does not own.",
                parent.display(),
                mode & 0o7777
            ),
            format_args!(
                "store directory {} is mode {:04o}, writable by group or other, so another local \
                 user could replace the database path",
                parent.display(),
                mode & 0o7777
            ),
        ));
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
pub(super) fn ensure_rendezvous_path_is_swap_resistant(
    parent: &std::path::Path,
    daemon_euid: u32,
    role: RendezvousPathRole<'_>,
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
                role.refusal(
                    format_args!(
                        "refusing to start: cannot resolve the working directory to absolutize \
                     {directory_name} {} for {env_name}: {e}.",
                        parent.display()
                    ),
                    format_args!(
                        "cannot resolve the working directory for store directory {}: {e}",
                        parent.display()
                    ),
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
            role.refusal(
                format_args!(
                    "refusing to start: cannot stat {component_name} component {} for {env_name}: \
                 {e}. An unreadable component is not a passing one.",
                    candidate.display()
                ),
                format_args!(
                    "cannot stat store-path component {}: {e}; unreadable metadata is not a \
                     passing state",
                    candidate.display()
                ),
            )
        })?;
        let owner = meta.uid();

        if meta.file_type().is_symlink() {
            symlinks_followed += 1;
            if symlinks_followed > 40 {
                return Err(role.refusal(
                    format_args!(
                        "refusing to start: {path_name} for {env_name} resolves through more than \
                     40 symlinks at {} — treating this as a loop.",
                        candidate.display()
                    ),
                    format_args!(
                        "store path resolves through more than 40 symlinks at {}; treating this as \
                         a loop",
                        candidate.display()
                    ),
                ));
            }
            if owner != daemon_euid && owner != 0 {
                return Err(role.refusal(
                    format_args!(
                        "refusing to start: {component_name} symlink component {} for {env_name} \
                     is owned by uid {owner}, not this daemon's uid ({daemon_euid}) or root — \
                     its owner could retarget it after this check and re-root the {path_name}. \
                     Point {env_name} somewhere trusted end to end, or unset it for the default.",
                        candidate.display()
                    ),
                    format_args!(
                        "store-path symlink component {} is owned by uid {owner}, not this \
                         process's uid ({daemon_euid}) or root; its owner could retarget the \
                         database path",
                        candidate.display()
                    ),
                ));
            }
            let target = std::fs::read_link(&candidate).map_err(|e| {
                role.refusal(
                    format_args!(
                        "refusing to start: cannot read {component_name} symlink component {} \
                     for {env_name}: {e}.",
                        candidate.display()
                    ),
                    format_args!(
                        "cannot read store-path symlink component {}: {e}",
                        candidate.display()
                    ),
                )
            })?;
            push_components(&mut stack, &target);
            continue;
        }

        if meta.is_dir() {
            let mode = meta.permissions().mode();
            let sticky = mode & 0o1000 != 0;
            if owner != daemon_euid && owner != 0 {
                return Err(role.refusal(
                    format_args!(
                    "refusing to start: {component_name} ancestor {} for {env_name} is owned by \
                     uid {owner}, not this daemon's uid ({daemon_euid}) or root — its owner \
                     could rename the next path component and re-root the {path_name}. Point \
                     {env_name} somewhere trusted end to end, or unset it for the default.",
                    candidate.display()
                    ),
                    format_args!(
                        "store-path ancestor {} is owned by uid {owner}, not this process's uid \
                         ({daemon_euid}) or root; its owner could rename the next path component",
                        candidate.display()
                    ),
                ));
            }
            if mode & 0o022 != 0 && !sticky {
                return Err(role.refusal(
                    format_args!(
                        "refusing to start: {component_name} ancestor {} for {env_name} is mode \
                     {:04o} — writable by group or other without the sticky bit, so another \
                     local user could rename the next path component and re-root the {path_name}. \
                     Point {env_name} somewhere trusted end to end, or unset it for the default.",
                        candidate.display(),
                        mode & 0o7777
                    ),
                    format_args!(
                        "store-path ancestor {} is mode {:04o}, writable by group or other without \
                         the sticky bit, so another local user could rename the next path \
                         component",
                        candidate.display(), mode & 0o7777
                    ),
                ));
            }
            resolved = candidate;
            continue;
        }

        return Err(role.refusal(
            format_args!(
                "refusing to start: {component_name} component {} for {env_name} is neither a \
             directory nor a symlink — the {path_name} cannot traverse it.",
                candidate.display()
            ),
            format_args!(
                "store-path component {} is neither a directory nor a symlink; the database path \
                 cannot traverse it",
                candidate.display()
            ),
        ));
    }

    Ok(())
}
