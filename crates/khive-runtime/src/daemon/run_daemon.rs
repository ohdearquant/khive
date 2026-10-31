#![cfg(unix)]
use super::*;

#[cfg(test)]
pub(super) static DEFAULT_RENDEZVOUS_PREBIND_HOOK: std::sync::Mutex<
    Option<Box<dyn FnOnce() + Send>>,
> = std::sync::Mutex::new(None);

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
    let default_rendezvous = super::lifecycle::path_override(SOCKET_PATH_ENV).is_none()
        && super::lifecycle::path_override(PID_PATH_ENV).is_none();
    ensure_rendezvous_dir_is_trusted(
        parent,
        RendezvousPathRole::Socket,
        daemon_euid,
        default_rendezvous,
    )
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

/// Secure the default rendezvous directory during a mutating recovery attempt.
/// Read-only daemon probes must use `ensure_pid_file_dir_is_trusted` instead.
#[cfg(unix)]
pub fn prepare_default_rendezvous_for_recovery() -> anyhow::Result<()> {
    let explicit_rendezvous = super::lifecycle::path_override(PID_PATH_ENV).is_some()
        || super::lifecycle::path_override(SOCKET_PATH_ENV).is_some();
    let parent = khive_dir();
    let mut marker_lock = supervisor_marker_path().into_os_string();
    marker_lock.push(".lock");
    let locks = [
        lock_path(),
        recoverer_lock_path(),
        std::path::PathBuf::from(marker_lock),
    ];
    for lock in &locks {
        if lock.file_name().is_none() {
            anyhow::bail!(
                "refusing recovery lock {} without a filename",
                lock.display()
            );
        }
    }
    if explicit_rendezvous
        && !locks
            .iter()
            .any(|lock| lock.parent() == Some(parent.as_path()))
    {
        return Ok(());
    }
    if !prepare_default_rendezvous_directory()? {
        return Ok(());
    }
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;
    for lock in locks {
        if lock.parent() == Some(parent.as_path()) {
            tighten_owned_recovery_lock(&lock, daemon_euid)?;
        }
    }
    Ok(())
}

/// Prepare the default parent only when this particular recovery lock needs it.
#[cfg(unix)]
pub fn prepare_default_rendezvous_for_lock(lock: &std::path::Path) -> anyhow::Result<()> {
    let parent = khive_dir();
    if lock.parent() != Some(parent.as_path()) {
        return Ok(());
    }
    if lock.file_name().is_none() {
        anyhow::bail!(
            "refusing recovery lock {} without a filename",
            lock.display()
        );
    }
    if prepare_default_rendezvous_directory()? {
        // SAFETY: `geteuid` is always successful and takes no arguments.
        let daemon_euid = unsafe { libc::geteuid() } as u32;
        tighten_owned_recovery_lock(lock, daemon_euid)?;
    }
    Ok(())
}

/// Prepare the default directory without touching any lock in it.
#[cfg(unix)]
pub(super) fn prepare_default_rendezvous_directory() -> anyhow::Result<bool> {
    use std::os::unix::fs::DirBuilderExt;

    let explicit_rendezvous = super::lifecycle::path_override(PID_PATH_ENV).is_some()
        || super::lifecycle::path_override(SOCKET_PATH_ENV).is_some();
    let parent = khive_dir();
    // An explicitly selected rendezvous is not ours to repair. If a default
    // lock still needs this parent, create it only when absent; an existing
    // parent must already pass the normal trust checks at the checked opener.
    if explicit_rendezvous {
        if let Ok(metadata) = std::fs::symlink_metadata(&parent) {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid takes no arguments and always succeeds.
            let uid = unsafe { libc::geteuid() } as u32;
            if metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o7777 == 0 {
                anyhow::bail!("cannot access default lock directory {} (mode 0000); verify ownership and set this directory to mode 0700 by hand, then retry", parent.display());
            }
            return Ok(false);
        }
    }
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let daemon_euid = unsafe { libc::geteuid() } as u32;
    let ancestors = parent
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    ensure_rendezvous_path_is_swap_resistant(ancestors, daemon_euid, RendezvousPathRole::PidFile)?;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    let created = match builder.create(&parent) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && explicit_rendezvous => {
            return Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(anyhow::anyhow!(
                "refusing to inspect default rendezvous directory {}: {error}",
                parent.display()
            ))
        }
    };
    repair_default_dir_with_recovery_guidance(&parent, daemon_euid, created)?;

    Ok(true)
}

#[cfg(unix)]
fn repair_default_dir_with_recovery_guidance(
    parent: &std::path::Path,
    daemon_euid: u32,
    created: bool,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    repair_owned_default_dir(parent, RendezvousPathRole::PidFile, daemon_euid).map_err(|error| {
        let owned_inaccessible = std::fs::symlink_metadata(parent).is_ok_and(|metadata| {
            metadata.is_dir() && metadata.uid() == daemon_euid && metadata.mode() & 0o7777 == 0
        });
        if owned_inaccessible {
            let context = if created { "newly created" } else { "existing" };
            anyhow::anyhow!(
                "failed to prepare {context} default directory {}: {error:#}; verify ownership and set this directory to mode 0700 by hand, then retry",
                parent.display()
            )
        } else {
            error
        }
    })
}

#[cfg(unix)]
fn tighten_owned_recovery_lock(lock: &std::path::Path, daemon_euid: u32) -> anyhow::Result<()> {
    tighten_owned_recovery_lock_with_hook(lock, daemon_euid, || {})
}

#[cfg(unix)]
fn tighten_owned_recovery_lock_with_hook(
    lock: &std::path::Path,
    daemon_euid: u32,
    after_validation: impl FnOnce(),
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let observed = match std::fs::symlink_metadata(lock) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => anyhow::bail!("cannot inspect recovery lock {}: {error}", lock.display()),
    };
    let mode = observed.permissions().mode() & 0o7777;
    if !observed.is_file() || observed.uid() != daemon_euid || observed.nlink() != 1 {
        anyhow::bail!(
            "refusing recovery lock {}: expected a single-link regular file owned by uid {daemon_euid}, found uid {} and mode {mode:04o}",
            lock.display(),
            observed.uid()
        );
    }
    super::locks::checked_open_lock_with_hooks(lock, false, false, true, || {}, after_validation)?
        .ok_or_else(|| anyhow::anyhow!("recovery lock {} disappeared", lock.display()))?;
    Ok(())
}

#[cfg(unix)]
fn repair_owned_default_dir(
    parent: &std::path::Path,
    role: RendezvousPathRole<'_>,
    daemon_euid: u32,
) -> anyhow::Result<()> {
    repair_owned_default_dir_with_hooks(parent, role, daemon_euid, || {}, || {})
}

#[cfg(unix)]
fn repair_owned_default_dir_with_hooks(
    parent: &std::path::Path,
    role: RendezvousPathRole<'_>,
    daemon_euid: u32,
    before_open: impl FnOnce(),
    after_validation: impl FnOnce(),
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let ancestors = parent
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    ensure_rendezvous_path_is_swap_resistant(ancestors, daemon_euid, role)?;

    let observed = std::fs::symlink_metadata(parent).map_err(|error| {
        anyhow::anyhow!(
            "refusing to stat default rendezvous directory {}: {error}",
            parent.display()
        )
    })?;
    let observed_mode = observed.permissions().mode() & 0o7777;
    if !observed.is_dir() {
        anyhow::bail!(
            "refusing non-directory or symlink default rendezvous path {} (mode {observed_mode:04o})",
            parent.display()
        );
    }
    before_open();
    // The metadata check and permission change operate on this exact opened
    // directory; opening the leaf with NOFOLLOW refuses a symlink.
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)
        .map_err(|error| anyhow::anyhow!(
            "refusing to open default rendezvous directory {} (observed mode {observed_mode:04o}) without following a symlink: {error}",
            parent.display()
        ))?;
    let metadata = directory.metadata().map_err(|error| {
        anyhow::anyhow!(
            "refusing to stat default rendezvous directory {}: {error}",
            parent.display()
        )
    })?;
    if observed.dev() != metadata.dev() || observed.ino() != metadata.ino() {
        anyhow::bail!(
            "default rendezvous directory {} changed before it was opened",
            parent.display()
        );
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if !metadata.is_dir() || metadata.uid() != daemon_euid {
        anyhow::bail!(
            "refusing to repair default rendezvous directory {}: owner uid {}, mode {mode:04o}; expected uid {daemon_euid}",
            parent.display(),
            metadata.uid()
        );
    }
    after_validation();
    let latest = directory.metadata().map_err(|error| {
        anyhow::anyhow!(
            "refusing to restat default rendezvous directory {}: {error}",
            parent.display()
        )
    })?;
    let latest_mode = latest.permissions().mode() & 0o7777;
    if !latest.is_dir() || latest.uid() != daemon_euid {
        anyhow::bail!(
            "refusing to repair default rendezvous directory {}: owner uid {}, mode {latest_mode:04o}; expected uid {daemon_euid}",
            parent.display(),
            latest.uid()
        );
    }
    let tightened = latest_mode & 0o700;
    if tightened != latest_mode {
        directory
            .set_permissions(std::fs::Permissions::from_mode(tightened))
            .map_err(|error| anyhow::anyhow!(
                "refusing to tighten default rendezvous directory {} from mode {latest_mode:04o}: {error}",
                parent.display()
            ))?;
    }
    let current = std::fs::symlink_metadata(parent).map_err(|error| {
        anyhow::anyhow!(
            "refusing changed default rendezvous directory {}: {error}",
            parent.display()
        )
    })?;
    if !current.is_dir() || metadata.dev() != current.dev() || metadata.ino() != current.ino() {
        anyhow::bail!(
            "default rendezvous directory {} changed during repair",
            parent.display()
        );
    }
    ensure_rendezvous_path_is_swap_resistant(parent, daemon_euid, role)
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
        return repair_owned_default_dir(parent, role, daemon_euid).map_err(|error| {
            role.refusal(
                format_args!("{error:#}"),
                format_args!(
                    "cannot safely repair default store directory {}: {error:#}",
                    parent.display()
                ),
            )
        });
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
        if role == RendezvousPathRole::PidFile
            && super::lifecycle::path_override(PID_PATH_ENV).is_none()
            && parent == khive_dir()
        {
            anyhow::bail!(
                "refusing to start: default PID-file directory {} is mode {:04o} — writable by group or other",
                parent.display(),
                mode & 0o7777
            );
        }
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

#[cfg(all(test, unix))]
mod default_directory_repair_tests {
    use super::{
        ensure_pid_file_dir_is_trusted, ensure_socket_dir_is_trusted,
        repair_default_dir_with_recovery_guidance, repair_owned_default_dir,
        repair_owned_default_dir_with_hooks, tighten_owned_recovery_lock,
        tighten_owned_recovery_lock_with_hook, RendezvousPathRole,
    };
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::io::AsRawFd;

    fn hold_lock(file: &std::fs::File) {
        // SAFETY: the fixture owns this live file descriptor.
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[derive(Clone)]
    struct EmptyDispatcher;

    #[async_trait::async_trait]
    impl super::super::DaemonDispatch for EmptyDispatcher {
        fn plan(&self, _ops: &str) -> String {
            String::new()
        }

        async fn dispatch(
            &self,
            _ops: String,
            _presentation: Option<String>,
            _presentation_per_op: Option<Vec<Option<String>>>,
            _format: Option<String>,
            _format_per_op: Option<Vec<Option<String>>>,
            _from_wire: bool,
            _identity: Option<super::super::RequestIdentity>,
        ) -> Result<String, String> {
            Ok("{}".to_owned())
        }

        async fn warm_all(&self) {}
        fn namespace(&self) -> &str {
            "test"
        }
        fn config_id(&self) -> &str {
            "test"
        }
    }

    #[test]
    #[serial_test::serial]
    fn pid_directory_diagnostic_depends_on_the_checked_path_and_override() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-pid-diagnostic-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("home");
        let default = home.join(".khive");
        let other = root.path().join("other");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::env::set_var("HOME", &home);
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_SOCKET");

        let other_message = ensure_pid_file_dir_is_trusted(&other.join("daemon.pid"))
            .unwrap_err()
            .to_string();
        assert!(
            other_message.contains("PID-file directory") && other_message.contains("for KHIVE_PID"),
            "{other_message}"
        );
        let default_message = ensure_pid_file_dir_is_trusted(&default.join("khived.pid"))
            .unwrap_err()
            .to_string();
        assert!(
            default_message.contains("default PID-file directory"),
            "{default_message}"
        );

        std::env::set_var("KHIVE_PID", default.join("khived.pid"));
        let explicit_message = ensure_pid_file_dir_is_trusted(&default.join("khived.pid"))
            .unwrap_err()
            .to_string();
        assert!(
            explicit_message.contains("for KHIVE_PID"),
            "{explicit_message}"
        );
        assert!(
            !explicit_message.contains("default PID-file directory"),
            "{explicit_message}"
        );
        assert_eq!(
            std::fs::metadata(&other).unwrap().permissions().mode() & 0o777,
            0o775
        );
        assert_eq!(
            std::fs::metadata(&default).unwrap().permissions().mode() & 0o777,
            0o775
        );
    }

    #[test]
    #[serial_test::serial]
    fn explicit_boot_lock_does_not_prepare_an_unrelated_default_home() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-explicit-boot-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("missing-home");
        let lock = root.path().join("boot.lock");
        std::env::set_var("HOME", &home);
        std::env::set_var("KHIVE_PID", root.path().join("daemon.pid"));
        std::env::set_var("KHIVE_SOCKET", root.path().join("daemon.sock"));
        std::env::set_var("KHIVE_LOCK", &lock);
        std::env::remove_var("KHIVE_RECOVERER_LOCK");
        std::env::remove_var("KHIVE_SUPERVISOR_MARKER");
        let opened = super::super::locks::open_mutating_daemon_lock(&lock, false)
            .expect("the explicit lock parent exists and is trusted");
        assert!(lock.exists());
        assert!(
            !home.exists(),
            "an unrelated default HOME must remain absent"
        );
        drop(opened);

        let unsafe_parent = root.path().join("unsafe-parent");
        std::fs::create_dir(&unsafe_parent).unwrap();
        std::fs::set_permissions(&unsafe_parent, std::fs::Permissions::from_mode(0o775)).unwrap();
        let unsafe_lock = unsafe_parent.join("boot.lock");
        assert!(super::super::locks::open_mutating_daemon_lock(&unsafe_lock, false).is_err());
        assert!(
            !unsafe_lock.exists(),
            "an untrusted explicit parent must remain refused"
        );
        assert!(!home.exists());
    }

    #[test]
    #[serial_test::serial]
    fn explicit_boot_lock_creates_its_missing_pid_parent() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-explicit-missing-parent-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("missing-home");
        let parent = root.path().join("new").join("rendezvous");
        let lock = parent.join("boot.lock");
        std::env::set_var("HOME", &home);
        std::env::set_var("KHIVE_PID", parent.join("daemon.pid"));
        std::env::set_var("KHIVE_SOCKET", parent.join("daemon.sock"));
        std::env::set_var("KHIVE_LOCK", &lock);
        std::env::remove_var("KHIVE_RECOVERER_LOCK");
        std::env::remove_var("KHIVE_SUPERVISOR_MARKER");
        assert!(!parent.exists());
        // SAFETY: this fixture runs alone in a dedicated child process.
        let previous_umask = unsafe { libc::umask(0o077) };
        let opened = super::super::locks::open_mutating_daemon_lock(&lock, false);
        // SAFETY: restore the process umask immediately after the operation.
        unsafe { libc::umask(previous_umask) };
        let opened = opened.expect("the explicitly selected lock parent is created");
        assert!(parent.is_dir());
        assert!(lock.is_file());
        assert!(!home.exists());
        drop(opened);
    }

    #[test]
    #[serial_test::serial]
    fn private_lock_with_default_rendezvous_does_not_prepare_home() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-private-lock-default-rendezvous-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let lock = root.path().join("boot.lock");
        std::env::set_var("HOME", &home);
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_SOCKET");
        std::env::set_var("KHIVE_LOCK", &lock);
        std::env::remove_var("KHIVE_RECOVERER_LOCK");
        std::env::remove_var("KHIVE_SUPERVISOR_MARKER");
        let opened = super::super::locks::open_mutating_daemon_lock(&lock, false)
            .expect("the private lock must open");
        hold_lock(&opened);
        assert!(lock.is_file());
        assert!(
            std::fs::read_dir(&home).unwrap().next().is_none(),
            "opening a private lock must not prepare the default rendezvous"
        );
        drop(opened);
    }

    #[test]
    #[serial_test::serial]
    fn default_lock_preparation_only_inspects_the_selected_lock() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-selected-default-lock-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("home");
        let default = home.join(".khive");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::env::set_var("HOME", &home);
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_SOCKET");
        std::env::remove_var("KHIVE_LOCK");
        std::env::remove_var("KHIVE_RECOVERER_LOCK");
        std::env::remove_var("KHIVE_SUPERVISOR_MARKER");
        let unrelated = default.join("khived.recoverer.lock");
        std::os::unix::fs::symlink(root.path().join("missing"), &unrelated).unwrap();
        let selected = default.join("khived.recovery.lock");
        super::prepare_default_rendezvous_for_lock(&selected)
            .expect("unrelated recovery lock must not be examined");
        let opened = super::super::locks::open_mutating_daemon_lock(&selected, false)
            .expect("selected default lock must open");
        hold_lock(&opened);
        assert_eq!(
            opened.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(std::fs::symlink_metadata(unrelated)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn default_rendezvous_is_prepared_with_an_external_boot_lock() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-default-with-external-lock-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let lock = root.path().join("boot.lock");
        std::env::set_var("HOME", &home);
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_SOCKET");
        std::env::set_var("KHIVE_LOCK", &lock);
        std::env::remove_var("KHIVE_RECOVERER_LOCK");
        std::env::remove_var("KHIVE_SUPERVISOR_MARKER");
        // SAFETY: the test runs in a dedicated child process.
        let previous_umask = unsafe { libc::umask(0o002) };
        let default = home.join(".khive");
        let guard = super::super::acquire_daemon_boot_guard().expect("external boot guard");
        assert_eq!(
            std::fs::metadata(&default).unwrap().permissions().mode() & 0o777,
            0o700,
            "the default rendezvous must be private before boot"
        );
        let daemon = tokio::spawn(super::super::run_daemon_with_boot_guard_and_start(
            EmptyDispatcher,
            Some(guard),
            |_| {},
        ));
        let pid = default.join("khived.pid");
        let socket = default.join("khived.sock");
        let ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                assert!(!daemon.is_finished(), "daemon exited before boot");
                if lock.exists() && pid.exists() && socket.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        // SAFETY: restore the process umask immediately after the boot witness.
        unsafe { libc::umask(previous_umask) };
        ready.expect("default daemon must bind with its external boot lock");
        assert_eq!(
            std::fs::metadata(default).unwrap().permissions().mode() & 0o777,
            0o700
        );
        daemon.abort();
        assert!(daemon
            .await
            .expect_err("daemon task must be cancelled")
            .is_cancelled());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn supplied_external_guard_boots_with_private_default_rendezvous() {
        use std::os::unix::fs::FileTypeExt;

        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-supplied-guard-default-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let lock = root.path().join("boot.lock");
        std::env::set_var("HOME", &home);
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_SOCKET");
        std::env::set_var("KHIVE_LOCK", &lock);
        std::env::remove_var("KHIVE_RECOVERER_LOCK");
        std::env::remove_var("KHIVE_SUPERVISOR_MARKER");
        let guard = super::super::locks::open_mutating_daemon_lock(&lock, false)
            .expect("external guard file must open");
        hold_lock(&guard);
        assert!(std::fs::read_dir(&home).unwrap().next().is_none());

        let default = home.join(".khive");
        let pid = default.join("khived.pid");
        let socket = default.join("khived.sock");
        let entered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entered_at_hook = std::sync::Arc::clone(&entered);
        let observed_mode = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
        let mode_at_hook = std::sync::Arc::clone(&observed_mode);
        let socket_absent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let socket_absent_at_hook = std::sync::Arc::clone(&socket_absent);
        let directory_at_hook = default.clone();
        let socket_at_hook = socket.clone();
        *super::DEFAULT_RENDEZVOUS_PREBIND_HOOK.lock().unwrap() = Some(Box::new(move || {
            let mode = std::fs::metadata(&directory_at_hook)
                .map(|metadata| metadata.permissions().mode() & 0o777)
                .unwrap_or(u32::MAX);
            mode_at_hook.store(mode, std::sync::atomic::Ordering::SeqCst);
            socket_absent_at_hook.store(
                !socket_at_hook.exists(),
                std::sync::atomic::Ordering::SeqCst,
            );
            entered_at_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        // SAFETY: the test runs in a dedicated child process.
        let previous_umask = unsafe { libc::umask(0o002) };
        let daemon = tokio::spawn(super::super::run_daemon_with_boot_guard_and_start(
            EmptyDispatcher,
            Some(guard),
            |_| {},
        ));
        let hook_reached = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !entered.load(std::sync::atomic::Ordering::SeqCst) && !daemon.is_finished() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(hook_reached.is_ok(), "daemon must reach pre-bind observer");
        assert!(
            entered.load(std::sync::atomic::Ordering::SeqCst),
            "pre-bind observer must execute"
        );
        assert_eq!(
            observed_mode.load(std::sync::atomic::Ordering::SeqCst),
            0o700,
            "default rendezvous must be private before binding; u32::MAX means absent or unreadable"
        );
        assert!(
            socket_absent.load(std::sync::atomic::Ordering::SeqCst),
            "witness must precede socket bind"
        );
        let ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                assert!(!daemon.is_finished(), "daemon exited before boot");
                if pid.exists() && socket.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        // SAFETY: restore the process umask immediately after the boot witness.
        unsafe { libc::umask(previous_umask) };
        ready.expect("daemon must bind with the caller-supplied guard");
        assert!(std::fs::metadata(&socket).unwrap().file_type().is_socket());
        assert_eq!(
            std::fs::metadata(default).unwrap().permissions().mode() & 0o777,
            0o700
        );
        daemon.abort();
        assert!(daemon
            .await
            .expect_err("daemon task must be cancelled")
            .is_cancelled());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn explicit_daemon_boot_keeps_private_home_empty() {
        use std::os::unix::fs::FileTypeExt;

        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-explicit-daemon-")
            .tempdir_in("/tmp")
            .expect("private synthetic root");
        let home = root.path().join("home");
        let rendezvous = root.path().join("run");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&rendezvous).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&rendezvous, std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = rendezvous.join("boot.lock");
        let pid = rendezvous.join("daemon.pid");
        let socket = rendezvous.join("daemon.sock");
        std::env::set_var("HOME", &home);
        std::env::set_var("KHIVE_LOCK", &lock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::set_var("KHIVE_SOCKET", &socket);
        std::env::remove_var("KHIVE_RECOVERER_LOCK");
        std::env::remove_var("KHIVE_SUPERVISOR_MARKER");
        assert!(std::fs::read_dir(&home).unwrap().next().is_none());

        let daemon = tokio::spawn(super::run_daemon(EmptyDispatcher));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                assert!(
                    !daemon.is_finished(),
                    "daemon exited before the boot witness"
                );
                if lock.exists() && pid.exists() && socket.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("daemon must create its explicit boot lock, PID and socket");
        assert!(std::fs::metadata(&socket).unwrap().file_type().is_socket());
        assert!(
            std::fs::read_dir(&home).unwrap().next().is_none(),
            "explicit daemon boot must leave its private HOME empty"
        );
        daemon.abort();
        let error = daemon.await.expect_err("the daemon task must be cancelled");
        assert!(
            error.is_cancelled(),
            "daemon task failed before cancellation: {error}"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn direct_daemon_boot_refuses_unsafe_ancestor_before_lock_creation() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-boot-unsafe-")
            .tempdir_in("/tmp")
            .expect("private synthetic root");
        let unsafe_ancestor = root.path().join("unsafe");
        let home = unsafe_ancestor.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::set_permissions(&unsafe_ancestor, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::env::set_var("HOME", &home);
        for key in [
            "KHIVE_PID",
            "KHIVE_SOCKET",
            "KHIVE_LOCK",
            "KHIVE_RECOVERER_LOCK",
            "KHIVE_SUPERVISOR_MARKER",
        ] {
            std::env::remove_var(key);
        }
        let directory = home.join(".khive");
        assert!(!directory.exists());

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::run_daemon(EmptyDispatcher),
        )
        .await;

        assert!(
            matches!(outcome, Ok(Err(_))),
            "unsafe direct boot must refuse promptly"
        );
        assert!(
            !directory.exists(),
            "no lock directory may be created under an unsafe ancestor"
        );
        assert_eq!(
            std::fs::metadata(&unsafe_ancestor)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o775
        );

        std::fs::set_permissions(&unsafe_ancestor, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let existing_lock = directory.join("khived.recovery.lock");
        std::fs::write(&existing_lock, b"").unwrap();
        std::fs::set_permissions(&existing_lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let guard = super::super::acquire_daemon_boot_guard().expect("trusted boot-lock baseline");
        assert_eq!(
            guard.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&existing_lock)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(guard);
    }

    #[test]
    #[serial_test::serial]
    fn foreign_owner_is_refused_before_permissions_change() {
        let directory = tempfile::Builder::new()
            .prefix("khive-foreign-owner-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
        let actual_owner = std::fs::metadata(directory.path()).unwrap().uid();
        let other_owner = actual_owner.wrapping_add(1);

        let error =
            repair_owned_default_dir(directory.path(), RendezvousPathRole::PidFile, other_owner)
                .expect_err("directory owned by a different uid must not be repaired");

        let message = error.to_string();
        assert!(message.contains("owner uid"), "{message}");
        assert!(message.contains("0775"), "{message}");
        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o775
        );
    }

    #[test]
    #[serial_test::serial]
    fn repair_does_not_chmod_a_replacement_path() {
        let root = tempfile::Builder::new()
            .prefix("khive-default-swap-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        let directory = root.path().join("rendezvous");
        let original = root.path().join("original");
        let replacement = root.path().join("replacement");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
        let owner = std::fs::metadata(&directory).unwrap().uid();

        let outcome = repair_owned_default_dir_with_hooks(
            &directory,
            RendezvousPathRole::PidFile,
            owner,
            || {},
            || {
                std::fs::rename(&directory, &original).unwrap();
                std::fs::rename(&replacement, &directory).unwrap();
            },
        );

        assert!(outcome.is_err(), "a path replacement must be reported");
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&original).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    #[serial_test::serial]
    fn repair_does_not_follow_symlink_replacing_the_leaf_before_open() {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new()
            .prefix("khive-default-symlink-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        let directory = root.path().join("rendezvous");
        let target = root.path().join("target");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o775)).unwrap();
        let owner = std::fs::metadata(&directory).unwrap().uid();

        let outcome = repair_owned_default_dir_with_hooks(
            &directory,
            RendezvousPathRole::PidFile,
            owner,
            || {
                std::fs::remove_dir(&directory).unwrap();
                symlink(&target, &directory).unwrap();
            },
            || {},
        );

        assert!(outcome.is_err(), "a replacement symlink must be refused");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o775
        );
    }

    #[test]
    #[serial_test::serial]
    fn repair_refuses_directory_replaced_before_open_without_chmod() {
        let root = tempfile::Builder::new()
            .prefix("khive-default-before-open-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        let directory = root.path().join("rendezvous");
        let original = root.path().join("original");
        let replacement = root.path().join("replacement");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o775)).unwrap();
        let owner = std::fs::metadata(&directory).unwrap().uid();

        let outcome = repair_owned_default_dir_with_hooks(
            &directory,
            RendezvousPathRole::PidFile,
            owner,
            || {
                std::fs::rename(&directory, &original).unwrap();
                std::fs::rename(&replacement, &directory).unwrap();
            },
            || {},
        );

        assert!(
            outcome.is_err(),
            "a directory swapped before open must be refused"
        );
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o775
        );
        assert_eq!(
            std::fs::metadata(&original).unwrap().permissions().mode() & 0o777,
            0o775
        );
    }

    #[test]
    #[serial_test::serial]
    fn repair_does_not_restore_owner_bits_removed_after_validation() {
        let root = tempfile::Builder::new()
            .prefix("khive-default-chmod-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        let directory = root.path().join("rendezvous");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
        let owner = std::fs::metadata(&directory).unwrap().uid();

        let outcome = repair_owned_default_dir_with_hooks(
            &directory,
            RendezvousPathRole::PidFile,
            owner,
            || {},
            || {
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500))
                    .unwrap()
            },
        );

        let mode = std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            outcome.is_ok(),
            "already restricted directory should remain usable"
        );
        assert_eq!(mode, 0o500);
    }

    #[test]
    #[serial_test::serial]
    fn newly_created_inaccessible_default_dir_reports_manual_recovery() {
        let root = tempfile::Builder::new()
            .prefix("khive-dir-recovery-")
            .tempdir_in("/tmp")
            .unwrap();
        let directory = root.path().join("rendezvous");
        std::fs::create_dir(&directory).unwrap();
        let uid = std::fs::metadata(&directory).unwrap().uid();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o000)).unwrap();
        // This is a fixture qualification, not semantic evidence on a privileged host.
        let denied = std::fs::File::open(&directory)
            .expect_err("fixture requires an unprivileged owner unable to open mode 0000");
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
        let error = repair_default_dir_with_recovery_guidance(&directory, uid, true).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&directory.display().to_string()));
        assert!(message.contains("0700") && message.contains("by hand"));
        let retry = repair_default_dir_with_recovery_guidance(&directory, uid, false).unwrap_err();
        let retry_message = format!("{retry:#}");
        assert!(retry_message.contains(&directory.display().to_string()));
        assert!(retry_message.contains("0700") && retry_message.contains("by hand"));
        let foreign =
            repair_default_dir_with_recovery_guidance(&directory, uid.wrapping_add(1), false)
                .unwrap_err();
        assert!(!format!("{foreign:#}").contains("by hand"));
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0
        );
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        repair_default_dir_with_recovery_guidance(&directory, uid, false).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn concurrent_lock_creation_reopens_the_validated_existing_entry() {
        let root = tempfile::Builder::new()
            .prefix("khive-lock-create-race-")
            .tempdir_in("/tmp")
            .unwrap();
        let lock = root.path().join("recovery.lock");

        let opened = super::super::locks::checked_open_lock_with_observe_hook(
            &lock,
            true,
            false,
            true,
            || {},
            || {},
            (false, false, || {
                std::fs::write(&lock, b"incumbent").unwrap();
                std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
            }),
        )
        .expect("concurrent valid lock must be reopened")
        .unwrap();

        let on_disk = std::fs::metadata(&lock).unwrap();
        assert_eq!(opened.metadata().unwrap().ino(), on_disk.ino());
        assert_eq!(on_disk.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&lock).unwrap(), b"incumbent");
    }

    #[test]
    #[serial_test::serial]
    fn concurrent_lock_symlink_is_refused_before_opening_its_target() {
        use std::os::unix::fs::symlink;
        let root = tempfile::Builder::new()
            .prefix("khive-lock-create-symlink-")
            .tempdir_in("/tmp")
            .unwrap();
        let lock = root.path().join("recovery.lock");
        let target = root.path().join("target");
        std::fs::write(&target, b"untouched").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = super::super::locks::checked_open_lock_with_observe_hook(
            &lock,
            true,
            false,
            true,
            || {},
            || {},
            (false, false, || symlink(&target, &lock).unwrap()),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    #[serial_test::serial]
    fn created_lock_failures_leave_entry_and_report_manual_recovery() {
        let root = tempfile::Builder::new()
            .prefix("khive-lock-failure-")
            .tempdir_in("/tmp")
            .unwrap();
        for (label, fail_metadata, fail_chmod) in
            [("metadata", true, false), ("chmod", false, true)]
        {
            let lock = root.path().join(label);
            let error = super::super::locks::checked_open_lock_with_faults(
                &lock,
                true,
                false,
                true,
                || {},
                || {
                    if fail_chmod {
                        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000))
                            .unwrap();
                    }
                },
                (fail_metadata, fail_chmod),
            )
            .unwrap_err();
            let message = error.to_string();
            assert!(message.contains(&lock.display().to_string()));
            assert!(
                message.contains("confirming no daemon")
                    && message.contains("verifying the entry is the leftover lock")
            );
            assert!(lock.exists(), "failure must not unlink the created entry");
            std::fs::remove_file(&lock).unwrap();
            let file = super::super::locks::checked_open_lock(&lock, true, false, true)
                .unwrap()
                .unwrap();
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    #[serial_test::serial]
    fn created_lock_replacement_reports_inspection_without_removal_advice() {
        let root = tempfile::Builder::new()
            .prefix("khive-lock-created-swap-")
            .tempdir_in("/tmp")
            .unwrap();
        let lock = root.path().join("lock");
        let original = root.path().join("original");
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"replacement").unwrap();
        let error = super::super::locks::checked_open_lock_with_hooks(
            &lock,
            true,
            false,
            true,
            || {},
            || {
                std::fs::rename(&lock, &original).unwrap();
                std::fs::rename(&replacement, &lock).unwrap();
            },
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&lock.display().to_string()));
        assert!(
            message.contains("inspect the path")
                && message.contains("do not remove the replacement")
        );
        assert!(!message.contains("remove it and retry"));
        assert_eq!(std::fs::read(&lock).unwrap(), b"replacement");
        assert!(original.exists());
    }

    #[test]
    #[serial_test::serial]
    fn newly_created_recovery_lock_is_owner_only_under_restrictive_umask() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("khive-lock-mode-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        std::env::set_var("HOME", root.path());
        std::env::set_var("KHIVE_PID", root.path().join("khived.pid"));
        std::env::set_var("KHIVE_SOCKET", root.path().join("khived.sock"));
        let lock = root.path().join("recovery.lock");
        // SAFETY: this test runs in an isolated child process.
        let old_umask = unsafe { libc::umask(0o777) };
        let opened = super::super::locks::open_lock_file(&lock);
        // SAFETY: restore the umask returned in this isolated process.
        unsafe { libc::umask(old_umask) };

        let file = opened.expect("create new recovery lock");
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    #[serial_test::serial]
    fn recovery_lock_opener_refuses_a_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new()
            .prefix("khive-lock-symlink-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        if crate::test_process::run_in_child() {
            return;
        }
        std::env::set_var("HOME", root.path());
        std::env::set_var("KHIVE_PID", root.path().join("khived.pid"));
        std::env::set_var("KHIVE_SOCKET", root.path().join("khived.sock"));
        let lock = root.path().join("lock");
        let target = root.path().join("not-a-lock");
        symlink(&target, &lock).unwrap();

        let error = super::super::locks::open_lock_file(&lock).unwrap_err();

        assert!(error.to_string().contains(&lock.display().to_string()));
        assert!(
            !target.exists(),
            "opening a lock must not create a symlink target"
        );
    }

    #[test]
    #[serial_test::serial]
    fn foreign_owned_lock_is_refused_without_chmod() {
        let root = tempfile::Builder::new()
            .prefix("khive-foreign-lock-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        let lock = root.path().join("lock");
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let owner = std::fs::metadata(&lock).unwrap().uid();

        let error = tighten_owned_recovery_lock(&lock, owner.wrapping_add(1)).unwrap_err();

        assert!(error.to_string().contains(&lock.display().to_string()));
        assert!(error.to_string().contains("0644"));
        assert_eq!(
            std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o644
        );
        tighten_owned_recovery_lock(&lock, owner).unwrap();
        assert_eq!(
            std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[serial_test::serial]
    fn lock_tightening_does_not_chmod_a_replacement_path() {
        let root = tempfile::Builder::new()
            .prefix("khive-lock-swap-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        let lock = root.path().join("lock");
        let original = root.path().join("original");
        let replacement = root.path().join("replacement");
        std::fs::write(&lock, b"").unwrap();
        std::fs::write(&replacement, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o644)).unwrap();
        let owner = std::fs::metadata(&lock).unwrap().uid();

        let outcome = tighten_owned_recovery_lock_with_hook(&lock, owner, || {
            std::fs::rename(&lock, &original).unwrap();
            std::fs::rename(&replacement, &lock).unwrap();
        });

        assert!(outcome.is_err());
        assert_eq!(
            std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(&original).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    #[serial_test::serial]
    fn lock_open_refuses_parent_replaced_after_directory_fd_check() {
        let root = tempfile::Builder::new()
            .prefix("khive-lock-parent-swap-")
            .tempdir_in("/tmp")
            .expect("private synthetic directory");
        let directory = root.path().join("parent");
        let original = root.path().join("original");
        let replacement = root.path().join("replacement");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = directory.join("lock");

        let outcome = super::super::locks::checked_open_lock_with_hooks(
            &lock,
            true,
            false,
            true,
            || {
                std::fs::rename(&directory, &original).unwrap();
                std::fs::rename(&replacement, &directory).unwrap();
            },
            || {},
        );

        assert!(outcome.is_err());
        assert!(!lock.exists(), "replacement parent must not receive a lock");
        assert!(
            !original.join("lock").exists(),
            "detected change must refuse before create"
        );
        std::fs::rename(&directory, &replacement).unwrap();
        std::fs::rename(&original, &directory).unwrap();
        let positive = super::super::locks::checked_open_lock(&lock, true, false, true)
            .expect("trusted-parent positive control")
            .expect("mutating opener creates the lock");
        assert_eq!(
            positive.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[serial_test::serial]
    fn explicitly_selected_default_socket_directory_is_not_repaired_on_boot() {
        if crate::test_process::run_in_child() {
            return;
        }
        let home = tempfile::Builder::new()
            .prefix("khive-explicit-default-")
            .tempdir_in("/tmp")
            .expect("private synthetic home");
        std::env::set_var("HOME", home.path());
        let directory = home.path().join(".khive");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::env::set_var("KHIVE_PID", directory.join("khived.pid"));
        std::env::set_var("KHIVE_SOCKET", directory.join("khived.sock"));

        let outcome = ensure_socket_dir_is_trusted(&directory);

        assert!(outcome.is_err());
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o775
        );
    }
}
