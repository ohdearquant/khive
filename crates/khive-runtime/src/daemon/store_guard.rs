//! Daemon-lifetime store claims: path-sidecar locks and the database-file identity each one binds.

#[cfg(unix)]
use std::ffi::{CString, OsStr};
#[cfg(unix)]
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{FileExt as _, MetadataExt};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::path::PathBuf;

/// One daemon-lifetime path-sidecar claim and its bound database-file identity.
#[cfg(unix)]
#[derive(Debug)]
pub struct DaemonStoreGuard {
    /// Pins the directory in which the sidecar was claimed. Both the sidecar
    /// and the database are opened relative to this same descriptor.
    pub(super) parent_dir: std::fs::File,
    pub(super) _sidecar: std::fs::File,
    pub(super) database: PathBuf,
    claimed_identity: Option<(u64, u64)>,
    pub(super) _bound_database: Option<std::fs::File>,
}

#[cfg(unix)]
impl Drop for DaemonStoreGuard {
    fn drop(&mut self) {
        // An inherited or duplicated descriptor can outlive this guard.
        // Closing only this descriptor would keep its flock alive.
        if let Err(error) = self._sidecar.unlock() {
            tracing::warn!(
                database = %self.database.display(),
                %error,
                "cannot release daemon store lock"
            );
        }
    }
}

/// Placeholder for platforms where serving daemons and store claims are unavailable.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct DaemonStoreGuard;

#[cfg(unix)]
pub(super) fn regular_store_identity(
    database: &std::path::Path,
) -> anyhow::Result<Option<(u64, u64)>> {
    let metadata = match std::fs::symlink_metadata(database) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            anyhow::bail!(
                "cannot inspect claimed database {}: {error}",
                database.display()
            )
        }
    };
    anyhow::ensure!(
        metadata.is_file(),
        "claimed database {} is not a regular file",
        database.display()
    );
    Ok(Some((metadata.dev(), metadata.ino())))
}

/// Open a single directory entry without re-resolving any parent pathname.
#[cfg(unix)]
fn open_claimed_entry(
    parent: &std::fs::File,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<std::fs::File> {
    let name = CString::new(name.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store entry contains U+0000",
        )
    })?;
    // SAFETY: the directory fd and NUL-terminated entry name stay live through
    // openat. FromRawFd takes ownership of the returned fd exactly once.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            // Variadic arguments require C default promotion; mode_t is u16 on macOS.
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new, valid fd owned by this process.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Walk an already-canonical absolute parent path one component at a time.
/// A newly planted ancestor symlink cannot redirect the claim before the
/// sidecar is opened; each later component is relative to the held directory.
#[cfg(unix)]
fn open_claimed_directory(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::path::Component;

    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "claimed store directory must be absolute",
        ));
    }
    let mut directory = std::fs::File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory =
                    open_claimed_entry(&directory, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "claimed store directory must be canonical",
                ));
            }
        }
    }
    Ok(directory)
}

/// Refuse a stable retarget of the canonical parent pathname. This check is
/// useful before binding and after SQLite open, but is not an atomic proof of
/// the pathname SQLite itself traversed in between.
#[cfg(unix)]
pub(super) fn ensure_claimed_parent_identity(
    directory: &std::fs::File,
    database: &std::path::Path,
) -> anyhow::Result<()> {
    let parent = database.parent().expect("claimed path has a parent");
    let held = directory.metadata()?;
    let observed = std::fs::symlink_metadata(parent)?;
    anyhow::ensure!(
        observed.is_dir() && (held.dev(), held.ino()) == (observed.dev(), observed.ino()),
        "claimed database {} parent directory changed since claim",
        database.display()
    );
    Ok(())
}

/// Inspect the claimed directory entry, including a missing database, without
/// following a final-component symlink or reopening the directory by name.
#[cfg(unix)]
fn regular_store_identity_at(
    parent: &std::fs::File,
    name: &OsStr,
    database: &std::path::Path,
) -> anyhow::Result<Option<(u64, u64)>> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| anyhow::anyhow!("database name {} contains U+0000", database.display()))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to writable storage and name is NUL-terminated.
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        anyhow::bail!(
            "cannot inspect claimed database {}: {error}",
            database.display()
        );
    }
    // SAFETY: fstatat succeeded and initialized the complete stat structure.
    let stat = unsafe { stat.assume_init() };
    anyhow::ensure!(
        stat.st_mode & libc::S_IFMT == libc::S_IFREG,
        "claimed database {} is not a regular file",
        database.display()
    );
    #[cfg(target_os = "linux")]
    let device = stat.st_dev;
    #[cfg(not(target_os = "linux"))]
    let device = stat.st_dev as u64;
    Ok(Some((device, stat.st_ino)))
}

#[cfg(all(test, unix))]
thread_local! {
    static STORE_BIND_RACE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
pub(super) fn set_store_bind_race_hook(hook: impl FnOnce() + 'static) {
    STORE_BIND_RACE_HOOK.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(all(test, unix))]
fn take_store_bind_race_hook() -> Option<Box<dyn FnOnce()>> {
    STORE_BIND_RACE_HOOK.with(|cell| cell.borrow_mut().take())
}

/// Describe a failed bind. A writable bind refused on a file with no write bits
/// is a snapshot declared writable; name the declaration to change.
#[cfg(unix)]
fn bind_open_error(
    error: &std::io::Error,
    database: &std::path::Path,
    read_only: bool,
) -> anyhow::Error {
    if !read_only
        && error.kind() == std::io::ErrorKind::PermissionDenied
        && std::fs::metadata(database).is_ok_and(|metadata| metadata.permissions().readonly())
    {
        return anyhow::anyhow!(
            "claimed database {} has no filesystem write bits; declare `read_only = true` so \
             backend topology and daemon config identity describe the snapshot-inspection mode \
             explicitly",
            database.display()
        );
    }
    anyhow::anyhow!(
        "cannot open claimed database {}: {error}",
        database.display()
    )
}

/// Open each database relative to the directory that holds its sidecar claim.
/// A missing writable database is created here, after the claim; a missing
/// read-only database fails without creating it. The descriptor pins the
/// identity to compare against the later SQLite pathname check.
#[cfg(unix)]
pub fn bind_daemon_store_files(
    guards: &mut [DaemonStoreGuard],
    read_only_paths: &[PathBuf],
) -> anyhow::Result<()> {
    for guard in guards {
        let read_only = read_only_paths.contains(&guard.database);
        ensure_claimed_parent_identity(&guard.parent_dir, &guard.database)?;
        let filename = guard
            .database
            .file_name()
            .expect("claimed path has a file name");
        #[cfg(test)]
        if let Some(hook) = take_store_bind_race_hook() {
            hook();
        }
        let flags = if read_only {
            libc::O_RDONLY
        } else {
            libc::O_RDWR | libc::O_CREAT
        };
        // A created database gets SQLite's own default mode, subject to the umask.
        let file = open_claimed_entry(&guard.parent_dir, filename, flags, 0o644)
            .map_err(|error| bind_open_error(&error, &guard.database, read_only))?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file(),
            "claimed database {} is not a regular file",
            guard.database.display()
        );
        let opened_identity = (metadata.dev(), metadata.ino());
        if let Some(claimed_identity) = guard.claimed_identity {
            anyhow::ensure!(
                claimed_identity == opened_identity,
                "claimed database {} changed inode between claim and open",
                guard.database.display()
            );
        }
        guard.claimed_identity = Some(opened_identity);
        guard._bound_database = Some(file);
        anyhow::ensure!(
            regular_store_identity(&guard.database)? == Some(opened_identity),
            "claimed database {} changed inode while binding its open file",
            guard.database.display()
        );
    }
    Ok(())
}

/// Fail boot if a canonical path no longer names its descriptor-bound file.
/// Daemon backend constructors also pass each held descriptor's identity to
/// the pool, which checks the SQLite-opened file before identity initialization
/// and WAL setup. This final path check precedes schema preparation and serving.
#[cfg(unix)]
pub fn assert_daemon_store_identities(guards: &[DaemonStoreGuard]) -> anyhow::Result<()> {
    for guard in guards {
        ensure_claimed_parent_identity(&guard.parent_dir, &guard.database)?;
        let bound = guard._bound_database.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "claimed database {} was not bound",
                guard.database.display()
            )
        })?;
        let metadata = bound.metadata()?;
        let bound_identity = (metadata.dev(), metadata.ino());
        let observed = regular_store_identity(&guard.database)?;
        anyhow::ensure!(
            observed == Some(bound_identity) && observed == guard.claimed_identity,
            "claimed database {} changed inode after open (claimed {:?}, now {:?}); refusing daemon boot",
            guard.database.display(),
            guard.claimed_identity,
            observed
        );
    }
    Ok(())
}

/// Non-Unix hosts cannot run the daemon and have no store claims to verify.
#[cfg(not(unix))]
pub fn assert_daemon_store_identities(_guards: &[DaemonStoreGuard]) -> anyhow::Result<()> {
    Ok(())
}

/// The persistent claim sidecar for one canonical database pathname.
#[cfg(unix)]
pub(super) fn daemon_store_lock_path(database: &std::path::Path) -> anyhow::Result<PathBuf> {
    let filename = database
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("database path {} has no file name", database.display()))?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(filename);
    lock_name.push(".khived.lock");
    Ok(database.with_file_name(lock_name))
}

/// The longest pid record a claim writes into its sidecar.
#[cfg(unix)]
const STORE_LOCK_RECORD_MAX: u64 = 64;

/// PID text is diagnostic only. A malformed or oversized lock marker cannot
/// make a contender allocate or read without a fixed bound.
#[cfg(unix)]
pub(super) fn store_lock_holder_pid(file: &mut std::fs::File) -> Option<u32> {
    let mut holder_text = String::new();
    file.take(STORE_LOCK_RECORD_MAX)
        .read_to_string(&mut holder_text)
        .ok()?;
    holder_text.trim().parse::<u32>().ok()
}

/// A sidecar named like `.<name>.khived.lock` is a lock record for `<name>`,
/// so a database may not carry that file name.
#[cfg(unix)]
fn has_lock_sidecar_name(filename: &OsStr) -> bool {
    const SUFFIX: &[u8] = b".khived.lock";
    let name = filename.as_bytes();
    name.len() > SUFFIX.len() && name.starts_with(b".") && name.ends_with(SUFFIX)
}

/// An existing sidecar is reused only when it is a lock record: no longer than
/// the pid record a claim writes and not a SQLite database. Anything else is
/// another store's data and is refused before it can be truncated.
#[cfg(unix)]
fn ensure_sidecar_holds_only_a_lock_record(
    file: &std::fs::File,
    len: u64,
    lock_path: &std::path::Path,
    database: &std::path::Path,
) -> anyhow::Result<()> {
    const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
    let mut sqlite_header = false;
    if len >= 16 {
        let mut head = [0u8; 16];
        file.read_exact_at(&mut head, 0).map_err(|error| {
            anyhow::anyhow!(
                "cannot read daemon store lock {}: {error}",
                lock_path.display()
            )
        })?;
        sqlite_header = head == *SQLITE_HEADER;
    }
    anyhow::ensure!(
        len <= STORE_LOCK_RECORD_MAX && !sqlite_header,
        "refusing daemon store claim: existing file at lock sidecar {} for database {} is not a \
         daemon lock record (longer than {} bytes or a SQLite database); the file was left \
         untouched",
        lock_path.display(),
        database.display(),
        STORE_LOCK_RECORD_MAX
    );
    Ok(())
}

/// Claim every database in `database_paths` as a writable store; see
/// [`claim_stores`].
#[cfg(unix)]
pub fn acquire_daemon_store_guards(
    database_paths: impl IntoIterator<Item = PathBuf>,
) -> anyhow::Result<Vec<DaemonStoreGuard>> {
    claim_stores(&database_paths.into_iter().collect::<Vec<_>>(), &[])
}

/// Hold one exclusive daemon-lifetime lock for each resolved SQLite file.
/// Unlike the HOME-bound boot/recovery lock, these locks follow the storage
/// topology. Callers acquire them before SQLite opens any store and retain
/// them until daemon shutdown. Never unlink a lock file: unlinking a held
/// inode would let a second boot lock a new one.
///
/// `database_paths` must contain canonical absolute paths. This function
/// sorts and deduplicates them so overlapping multi-backend topologies acquire
/// in one order and aliases of one path use one lock. It refuses a claim set
/// whose derived lock sidecar is a configured database, or in which a database
/// is named like a sidecar, before opening any sidecar for writing. An
/// existing sidecar that holds anything but a short pid record is refused,
/// never truncated.
///
/// `read_only_paths` names the databases this daemon opens read-only. A
/// read-only claim never creates its parent directory: when one is missing the
/// whole claim set is refused before any directory or sidecar is created.
#[cfg(unix)]
pub fn claim_stores(
    database_paths: &[PathBuf],
    read_only_paths: &[PathBuf],
) -> anyhow::Result<Vec<DaemonStoreGuard>> {
    let mut database_paths = database_paths.to_vec();
    database_paths.sort();
    database_paths.dedup();
    let lock_paths: Vec<PathBuf> = database_paths
        .iter()
        .map(|database| daemon_store_lock_path(database))
        .collect::<anyhow::Result<_>>()?;
    for (database, lock_path) in database_paths.iter().zip(&lock_paths) {
        if database_paths.binary_search(lock_path).is_ok() {
            anyhow::bail!(
                "refusing daemon store claim: lock sidecar {} for database {} is itself a \
                 configured database; no store lock was opened",
                lock_path.display(),
                database.display()
            );
        }
    }
    for database in &database_paths {
        if database.file_name().is_some_and(has_lock_sidecar_name) {
            anyhow::bail!(
                "refusing daemon store claim: database {} is named like a store lock sidecar \
                 (`.<name>.khived.lock`); no store lock was opened",
                database.display()
            );
        }
        if read_only_paths.contains(database) {
            if let Some(parent) = database.parent() {
                if let Err(error) = std::fs::metadata(parent) {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        anyhow::bail!(
                            "refusing daemon store claim: parent directory {} of read-only \
                             database {} does not exist; nothing was created",
                            parent.display(),
                            database.display()
                        );
                    }
                }
            }
        }
    }
    let mut configured_identities = Vec::new();
    for database in &database_paths {
        if let Some(identity) = regular_store_identity(database)? {
            configured_identities.push((database.clone(), identity));
        }
    }
    let mut guards = Vec::with_capacity(database_paths.len());

    for (database, lock_path) in database_paths.into_iter().zip(lock_paths) {
        let filename = database
            .file_name()
            .expect("store lock preflight required a database file name");
        let parent = lock_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("store lock {} has no parent", lock_path.display()))?;
        if !read_only_paths.contains(&database) {
            std::fs::create_dir_all(parent).map_err(|error| {
                anyhow::anyhow!(
                    "cannot create store lock directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let parent_dir = open_claimed_directory(parent).map_err(|error| {
            anyhow::anyhow!(
                "cannot open daemon store directory {} for {}: {error}",
                parent.display(),
                database.display()
            )
        })?;
        ensure_claimed_parent_identity(&parent_dir, &database)?;
        let mut file = open_claimed_entry(
            &parent_dir,
            lock_path.file_name().expect("sidecar path has a file name"),
            libc::O_RDWR | libc::O_CREAT,
            0o600,
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "cannot open daemon store lock {} for {}: {error}",
                lock_path.display(),
                database.display()
            )
        })?;
        let sidecar_metadata = file.metadata()?;
        if !sidecar_metadata.is_file() {
            anyhow::bail!(
                "daemon store lock {} is not a regular file",
                lock_path.display()
            );
        }
        let sidecar_identity = (sidecar_metadata.dev(), sidecar_metadata.ino());
        if let Some((matching_database, _)) = configured_identities
            .iter()
            .find(|(_, identity)| *identity == sidecar_identity)
        {
            anyhow::bail!(
                "refusing daemon store claim: opened lock sidecar {} for database {} is the same \
                 file as configured database {}; no sidecar lock was acquired or truncated",
                lock_path.display(),
                database.display(),
                matching_database.display()
            );
        }
        anyhow::ensure!(
            sidecar_metadata.nlink() <= 1,
            "refusing daemon store claim: opened lock sidecar {} for database {} has {} hard \
             links; no sidecar lock was acquired or truncated",
            lock_path.display(),
            database.display(),
            sidecar_metadata.nlink()
        );
        ensure_sidecar_holds_only_a_lock_record(
            &file,
            sidecar_metadata.len(),
            &lock_path,
            &database,
        )?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                // The winning process writes its pid immediately after locking.
                // A contender can race that write, so an empty/unreadable pid
                // is an unknown holder, never permission to proceed.
                let holder = store_lock_holder_pid(&mut file)
                    .map(|pid| format!("pid {pid}"))
                    .unwrap_or_else(|| "an unknown pid".to_string());
                anyhow::bail!(
                    "refusing to start: khived is already running as {holder} for database {}; \
                     daemon store lock {} is held",
                    database.display(),
                    lock_path.display()
                );
            }
            Err(std::fs::TryLockError::Error(error)) => {
                anyhow::bail!(
                    "cannot acquire daemon store lock {} for {}: {error}",
                    lock_path.display(),
                    database.display()
                );
            }
        }
        file.set_len(0)?;
        file.write_all(std::process::id().to_string().as_bytes())?;
        file.sync_data()?;
        let claimed_identity = regular_store_identity_at(&parent_dir, filename, &database)?;
        guards.push(DaemonStoreGuard {
            parent_dir,
            _sidecar: file,
            database,
            claimed_identity,
            _bound_database: None,
        });
    }
    Ok(guards)
}
