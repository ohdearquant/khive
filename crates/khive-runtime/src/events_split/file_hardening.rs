use std::path::{Path, PathBuf};

#[cfg(unix)]
use super::paths::absolutize;
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
