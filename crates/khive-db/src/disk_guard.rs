use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use khive_storage::CapacityUnavailablePhase;
use parking_lot::{Condvar, Mutex};
use sha2::{Digest, Sha256};

use crate::error::SqliteError;

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_SYMLINK_DEPTH: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum VolumeKey {
    #[cfg(unix)]
    UnixDevice(u64),
    #[cfg(windows)]
    WindowsSerial(u32),
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

#[derive(Debug, Clone)]
pub(crate) struct VolumeIdentity {
    key: VolumeKey,
    probe_path: PathBuf,
    volume_root: Option<PathBuf>,
}

impl PartialEq for VolumeIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for VolumeIdentity {}

impl Hash for VolumeIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl VolumeIdentity {
    pub(crate) fn resolve(db_path: &Path) -> Result<Self, SqliteError> {
        let probe_path = nearest_existing_canonical_ancestor(db_path)
            .map_err(|error| identity_error(format!("cannot resolve database volume: {error}")))?;
        let (key, volume_root) = volume_key(&probe_path)
            .map_err(|error| identity_error(format!("cannot identify database volume: {error}")))?;
        #[cfg(windows)]
        let probe_path = volume_root
            .clone()
            .ok_or_else(|| identity_error("Windows volume root is unavailable"))?;
        Ok(Self {
            key,
            probe_path,
            volume_root,
        })
    }

    pub(crate) fn diagnostic_key(&self) -> String {
        match self.key {
            #[cfg(unix)]
            VolumeKey::UnixDevice(device) => format!("unix-device:{device}"),
            #[cfg(windows)]
            VolumeKey::WindowsSerial(serial) => format!("windows-volume-serial:{serial}"),
            #[cfg(not(any(unix, windows)))]
            VolumeKey::Unsupported => "unsupported".to_string(),
        }
    }

    pub(crate) fn probe_path(&self) -> &Path {
        &self.probe_path
    }

    pub(crate) fn volume_root(&self) -> Option<&Path> {
        self.volume_root.as_deref()
    }

    #[cfg(all(test, any(unix, windows)))]
    pub(crate) fn different_volume_for_test(&self) -> Self {
        let mut other = self.clone();
        other.key = match self.key {
            #[cfg(unix)]
            VolumeKey::UnixDevice(device) => VolumeKey::UnixDevice(device ^ 1),
            #[cfg(windows)]
            VolumeKey::WindowsSerial(serial) => VolumeKey::WindowsSerial(serial ^ 1),
        };
        other
    }

    pub(crate) fn lock_filename(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"khive-sqlite-volume-lease-v1\0");
        match self.key {
            #[cfg(unix)]
            VolumeKey::UnixDevice(device) => {
                digest.update(b"unix\0");
                digest.update(device.to_be_bytes());
            }
            #[cfg(windows)]
            VolumeKey::WindowsSerial(serial) => {
                digest.update(b"windows\0");
                digest.update(serial.to_be_bytes());
            }
            #[cfg(not(any(unix, windows)))]
            VolumeKey::Unsupported => unreachable!("unsupported platforms cannot resolve a volume"),
        }
        let hash = digest.finalize();
        let suffix = hash[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("sqlite-volume-v1-{suffix}.lock")
    }

    #[track_caller]
    pub(crate) fn acquire(
        &self,
        deadline: Duration,
        lock_dir: Option<&Path>,
    ) -> Result<VolumeLease, LeaseRefusal> {
        let lock_dir = lock_dir.ok_or_else(|| {
            LeaseRefusal::Refused(lock_error(
                "writable SQLite pool has no configured volume-lock directory",
            ))
        })?;
        if !lock_dir.is_absolute() {
            return Err(LeaseRefusal::Refused(lock_error(
                "configured volume-lock directory is not absolute",
            )));
        }
        self.acquire_classified(deadline, lock_dir)
    }

    /// [`Self::acquire_classified`] with the refusal folded into its error,
    /// for the lease fixtures that assert on the typed error alone.
    #[cfg(test)]
    #[track_caller]
    fn acquire_in(&self, timeout: Duration, lock_dir: &Path) -> Result<VolumeLease, SqliteError> {
        self.acquire_classified(timeout, lock_dir)
            .map_err(SqliteError::from)
    }

    #[track_caller]
    fn acquire_classified(
        &self,
        timeout: Duration,
        lock_dir: &Path,
    ) -> Result<VolumeLease, LeaseRefusal> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| LeaseRefusal::Refused(lock_error("volume lease deadline overflow")))?;
        fs::create_dir_all(lock_dir).map_err(|error| {
            LeaseRefusal::Refused(lock_error(format!(
                "cannot create volume-lock directory: {error}"
            )))
        })?;
        let lock_path = lock_dir.join(self.lock_filename());
        let process = acquire_process_slot(
            ProcessSlotKey::new(self.key, lock_dir),
            deadline,
            Location::caller(),
        )?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                LeaseRefusal::Refused(lock_error(format!("cannot open volume-lock file: {error}")))
            })?;
        #[cfg(any(unix, windows))]
        loop {
            if Instant::now() >= deadline {
                return Err(LeaseRefusal::TimedOut(lock_error(format!(
                    "timed out after {} ms waiting for volume lease",
                    timeout.as_millis()
                ))));
            }
            match fs4::FileExt::try_lock(&file) {
                Ok(()) => {
                    return Ok(VolumeLease {
                        held: HeldLease {
                            file: Some(file),
                            process: Some(process),
                        },
                        _thread_bound: PhantomData,
                    });
                }
                Err(fs4::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(LeaseRefusal::TimedOut(lock_error(format!(
                            "timed out after {} ms waiting for volume lease",
                            timeout.as_millis()
                        ))));
                    }
                    std::thread::sleep(remaining.min(LOCK_POLL_INTERVAL));
                }
                Err(fs4::TryLockError::Error(error)) => {
                    return Err(LeaseRefusal::Refused(lock_error(format!(
                        "cannot acquire volume lease: {error}"
                    ))));
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (file, process, lock_path, deadline);
            Err(LeaseRefusal::Refused(lock_error(
                "this platform has no advisory file-lock support",
            )))
        }
    }
}

fn nearest_existing_canonical_ancestor(db_path: &Path) -> std::io::Result<PathBuf> {
    let mut candidate = if db_path.is_absolute() {
        db_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(db_path)
    };
    let mut symlink_depth = 0;
    loop {
        match candidate.canonicalize() {
            Ok(canonical) => return Ok(canonical),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        if symlink_depth == MAX_SYMLINK_DEPTH {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "database path exceeds the symlink resolution limit",
                            ));
                        }
                        symlink_depth += 1;
                        let target = fs::read_link(&candidate)?;
                        candidate = if target.is_absolute() {
                            target
                        } else {
                            candidate.parent().unwrap_or(Path::new("/")).join(target)
                        };
                    }
                    Ok(_) => return Err(error),
                    Err(metadata_error)
                        if metadata_error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        candidate = candidate.parent().map(Path::to_path_buf).ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "database path has no existing ancestor",
                            )
                        })?;
                    }
                    Err(metadata_error) => return Err(metadata_error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn volume_key(probe_path: &Path) -> std::io::Result<(VolumeKey, Option<PathBuf>)> {
    use std::os::unix::fs::MetadataExt;

    let device = fs::metadata(probe_path)?.dev();
    let mut root = probe_path.to_path_buf();
    while let Some(parent) = root.parent() {
        match fs::metadata(parent) {
            Ok(metadata) if metadata.dev() == device => root = parent.to_path_buf(),
            Ok(_) => return Ok((VolumeKey::UnixDevice(device), Some(root))),
            Err(_) => return Ok((VolumeKey::UnixDevice(device), None)),
        }
    }
    Ok((VolumeKey::UnixDevice(device), Some(root)))
}

#[cfg(windows)]
fn volume_key(probe_path: &Path) -> std::io::Result<(VolumeKey, Option<PathBuf>)> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

    let path = probe_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut root = vec![0_u16; 32_768];
    // SAFETY: both buffers are valid and NUL-terminated where required.
    if unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let root_len = root.iter().position(|unit| *unit == 0).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Windows volume root was not NUL-terminated",
        )
    })?;
    let mut serial = 0_u32;
    // SAFETY: GetVolumePathNameW produced the NUL-terminated root buffer.
    if unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        VolumeKey::WindowsSerial(serial),
        Some(PathBuf::from(OsString::from_wide(&root[..root_len]))),
    ))
}

#[cfg(not(any(unix, windows)))]
fn volume_key(_probe_path: &Path) -> std::io::Result<(VolumeKey, Option<PathBuf>)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no stable volume identity",
    ))
}

struct ProcessRegistry {
    held: Mutex<HashMap<ProcessSlotKey, SlotHolder>>,
    available: Condvar,
}

/// The in-process slot a lease takes. Outside the workspace test marker it is
/// the volume alone, so every database on a volume serializes in this process
/// whatever its lock directory (ADR-154 section 3). Under
/// `KHIVE_TEST_HARNESS=1` it also carries the lock directory: a test that takes
/// its own lock namespace is then isolated from the rest of its test binary in
/// process, as its own lock file already isolates it across processes.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ProcessSlotKey {
    volume: VolumeKey,
    harness_namespace: Option<PathBuf>,
}

impl ProcessSlotKey {
    fn new(volume: VolumeKey, lock_dir: &Path) -> Self {
        Self {
            volume,
            harness_namespace: harness_scoped_process_leases().then(|| lock_dir.to_path_buf()),
        }
    }
}

/// Whether in-process volume leases are scoped by lock directory, which only
/// the workspace test marker turns on. Read once per process.
pub fn harness_scoped_process_leases() -> bool {
    static SCOPED: OnceLock<bool> = OnceLock::new();
    *SCOPED.get_or_init(|| std::env::var(crate::pool::TEST_HARNESS_ENV).as_deref() == Ok("1"))
}

/// Who holds a volume's in-process slot: the thread, so a request from that
/// same thread is recognised as re-entry (`None` once the lease travels with
/// an operation across threads), and the call site, so the refusal can name
/// both ends.
struct SlotHolder {
    thread: Option<ThreadId>,
    site: &'static Location<'static>,
}

fn process_registry() -> &'static ProcessRegistry {
    static REGISTRY: OnceLock<ProcessRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| ProcessRegistry {
        held: Mutex::new(HashMap::new()),
        available: Condvar::new(),
    })
}

struct ProcessSlot(ProcessSlotKey);

impl ProcessSlot {
    fn detach_from_thread(&self) {
        if let Some(holder) = process_registry().held.lock().get_mut(&self.0) {
            holder.thread = None;
        }
    }
}

impl Drop for ProcessSlot {
    fn drop(&mut self) {
        let registry = process_registry();
        registry.held.lock().remove(&self.0);
        registry.available.notify_all();
    }
}

/// A thread that already holds a volume's lease can never be granted it a
/// second time by waiting: the holder is the caller. That nesting fails at
/// once and says so. Another thread of this process holding the slot is
/// ordinary contention and waits for the deadline.
fn acquire_process_slot(
    key: ProcessSlotKey,
    deadline: Instant,
    requester: &'static Location<'static>,
) -> Result<ProcessSlot, LeaseRefusal> {
    let registry = process_registry();
    let current = std::thread::current().id();
    let mut held = registry.held.lock();
    loop {
        if Instant::now() >= deadline {
            return Err(LeaseRefusal::TimedOut(lock_error(
                "timed out waiting for in-process volume lease",
            )));
        }
        match held.get(&key) {
            None => {
                held.insert(
                    key.clone(),
                    SlotHolder {
                        thread: Some(current),
                        site: requester,
                    },
                );
                return Ok(ProcessSlot(key));
            }
            Some(holder) if holder.thread == Some(current) => {
                return Err(LeaseRefusal::Refused(SqliteError::VolumeLeaseReentry {
                    holder_site: holder.site.to_string(),
                    requester_site: requester.to_string(),
                }));
            }
            Some(_) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(LeaseRefusal::TimedOut(lock_error(
                "timed out waiting for in-process volume lease",
            )));
        }
        registry.available.wait_for(&mut held, remaining);
    }
}

/// A volume lease bound to the thread that took it. Re-entry detection keys
/// on that thread, so the lease must not move: it is `!Send`, which makes
/// holding it across an `.await` in a `Send` future, or handing it to another
/// thread, a compile error. A lease that has to travel goes through
/// [`VolumeLease::detach_from_thread`].
pub(crate) struct VolumeLease {
    held: HeldLease,
    _thread_bound: PhantomData<*const ()>,
}

/// A volume lease released from the thread that took it, for an operation
/// whose steps run on more than one thread, such as a manual atomic unit held
/// across awaits. A later request from the original thread is ordinary
/// contention, not re-entry.
pub(crate) struct DetachedVolumeLease {
    _held: HeldLease,
}

impl VolumeLease {
    pub(crate) fn detach_from_thread(self) -> DetachedVolumeLease {
        if let Some(process) = &self.held.process {
            process.detach_from_thread();
        }
        DetachedVolumeLease { _held: self.held }
    }

    /// A lease that holds neither the advisory file lock nor the in-process
    /// slot: the no-lease arm of the lease-latency measurement.
    #[cfg(test)]
    pub(crate) fn unheld_for_measurement() -> Self {
        Self {
            held: HeldLease {
                file: None,
                process: None,
            },
            _thread_bound: PhantomData,
        }
    }
}

/// Set only in a child process of the lease-latency measurement, for its
/// no-lease arm; every lease that process requests is then unheld.
#[cfg(test)]
pub(crate) static SKIP_VOLUME_LEASE_FOR_MEASUREMENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

struct HeldLease {
    file: Option<File>,
    process: Option<ProcessSlot>,
}

impl Drop for HeldLease {
    fn drop(&mut self) {
        // The advisory lock goes first, then the in-process slot, so a
        // waiter woken by the slot finds the file lock already free.
        self.file.take();
        self.process.take();
    }
}

/// Why a volume lease was not granted. `TimedOut` is contention: another
/// writer held the volume until the guard deadline passed. `Refused` is every
/// lease that could not be attempted or was refused outright: no lock
/// directory, an unopenable lock file, a lock error, same-thread re-entry.
/// Both carry the typed error the caller returns; only the first is counted
/// and sunk as a writer timeout.
#[derive(Debug)]
pub(crate) enum LeaseRefusal {
    TimedOut(SqliteError),
    Refused(SqliteError),
}

impl From<LeaseRefusal> for SqliteError {
    fn from(refusal: LeaseRefusal) -> Self {
        match refusal {
            LeaseRefusal::TimedOut(error) | LeaseRefusal::Refused(error) => error,
        }
    }
}

fn identity_error(message: impl Into<String>) -> SqliteError {
    SqliteError::CapacityUnavailable {
        phase: CapacityUnavailablePhase::Identity,
        message: message.into(),
    }
}

fn lock_error(message: impl Into<String>) -> SqliteError {
    SqliteError::CapacityUnavailable {
        phase: CapacityUnavailablePhase::Lock,
        message: message.into(),
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::process::Command;

    fn hash(identity: &VolumeIdentity) -> u64 {
        let mut hasher = DefaultHasher::new();
        identity.hash(&mut hasher);
        hasher.finish()
    }

    // The process registry intentionally serializes every writable database
    // on a physical volume. Isolate these short-deadline lease tests from
    // unrelated parallel database tests while keeping each pair's key equal.
    fn isolated_test_key(which: u64) -> VolumeKey {
        #[cfg(unix)]
        {
            VolumeKey::UnixDevice(u64::MAX - which)
        }
        #[cfg(windows)]
        {
            VolumeKey::WindowsSerial(u32::MAX - which as u32)
        }
    }

    #[test]
    fn identity_and_lock_name_depend_only_on_volume_key() {
        let dir = tempfile::tempdir().unwrap();
        let first_parent = dir.path().join("first");
        let second_parent = dir.path().join("second");
        fs::create_dir(&first_parent).unwrap();
        fs::create_dir(&second_parent).unwrap();
        let first = VolumeIdentity::resolve(&first_parent.join("a.db")).unwrap();
        let second = VolumeIdentity::resolve(&second_parent.join("b.db")).unwrap();

        assert_ne!(first.probe_path(), second.probe_path());
        assert_eq!(first.key, second.key);
        assert_eq!(first, second);
        assert_eq!(hash(&first), hash(&second));
        assert_eq!(first.lock_filename(), second.lock_filename());

        let mut altered_metadata = first.clone();
        altered_metadata.probe_path = PathBuf::from("/diagnostic/path/only");
        altered_metadata.volume_root = None;
        assert_eq!(altered_metadata, first);
        assert_eq!(hash(&altered_metadata), hash(&first));
        assert_eq!(altered_metadata.lock_filename(), first.lock_filename());
    }

    #[test]
    fn same_volume_different_parents_share_one_bounded_lease() {
        let dir = tempfile::tempdir().unwrap();
        let first_parent = dir.path().join("first");
        let second_parent = dir.path().join("second");
        fs::create_dir(&first_parent).unwrap();
        fs::create_dir(&second_parent).unwrap();
        let mut first = VolumeIdentity::resolve(&first_parent.join("a.db")).unwrap();
        let mut second = VolumeIdentity::resolve(&second_parent.join("b.db")).unwrap();
        assert_eq!(first.key, second.key);
        first.key = isolated_test_key(1);
        second.key = first.key;
        let lock_dir = dir.path().join("locks");

        let held = first
            .acquire_in(Duration::from_millis(100), &lock_dir)
            .unwrap();
        // Another thread's hold is contention. The holder's own thread is
        // re-entry, covered by `same_thread_reentry_fails_before_the_deadline`.
        let error = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    second
                        .acquire_in(Duration::from_millis(30), &lock_dir)
                        .map(drop)
                })
                .join()
                .unwrap()
        })
        .expect_err("same-volume lease must time out");
        assert!(matches!(
            error,
            SqliteError::CapacityUnavailable {
                phase: CapacityUnavailablePhase::Lock,
                ..
            }
        ));
        drop(held);
        second
            .acquire_in(Duration::from_millis(100), &lock_dir)
            .expect("dropping the first lease releases both lock layers");
    }

    // MUST-FAIL: keying the in-process slot by the volume alone under the test
    // marker makes the second namespace wait out its deadline.
    #[test]
    fn harness_lock_namespaces_do_not_share_the_in_process_slot() {
        assert!(
            harness_scoped_process_leases(),
            "cargo runs library tests under KHIVE_TEST_HARNESS=1"
        );
        let dir = tempfile::tempdir().unwrap();
        let mut identity = VolumeIdentity::resolve(&dir.path().join("a.db")).unwrap();
        identity.key = isolated_test_key(5);
        let held = identity
            .acquire_in(Duration::from_millis(100), &dir.path().join("first-locks"))
            .unwrap();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    identity
                        .acquire_in(Duration::from_millis(100), &dir.path().join("second-locks"))
                        .map(drop)
                })
                .join()
                .unwrap()
        })
        .expect("under the test marker a second lock namespace takes its own in-process slot");
        drop(held);
    }

    // Without the marker the slot is the volume alone: a second lock namespace
    // on the same volume still waits in this process (ADR-154 section 3).
    #[test]
    fn without_the_marker_lock_namespaces_share_the_in_process_slot() {
        if crate::test_process::run_in_child(|command| {
            command.env_remove(crate::pool::TEST_HARNESS_ENV);
        }) {
            return;
        }
        assert!(!harness_scoped_process_leases());
        let dir = tempfile::tempdir().unwrap();
        let mut identity = VolumeIdentity::resolve(&dir.path().join("a.db")).unwrap();
        identity.key = isolated_test_key(6);
        let held = identity
            .acquire_in(Duration::from_millis(100), &dir.path().join("first-locks"))
            .unwrap();
        let error = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    identity
                        .acquire_in(Duration::from_millis(30), &dir.path().join("second-locks"))
                        .map(drop)
                })
                .join()
                .unwrap()
        })
        .expect_err("a second lock namespace must wait on the volume's in-process slot");
        assert!(
            matches!(
                &error,
                SqliteError::CapacityUnavailable {
                    phase: CapacityUnavailablePhase::Lock,
                    message,
                } if message.contains("in-process")
            ),
            "{error}"
        );
        drop(held);
    }

    // A thread-bound lease must not be `Send`: if it were, both impls below
    // would apply and the call would be ambiguous, so the crate would not
    // compile. The detached form must stay `Send` to travel with a unit.
    trait AmbiguousIfSend<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
    const _: fn() = || {
        <VolumeLease as AmbiguousIfSend<_>>::check();
        fn travels<T: Send>() {}
        travels::<DetachedVolumeLease>();
    };

    #[test]
    fn same_thread_reentry_fails_before_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut identity = VolumeIdentity::resolve(&dir.path().join("a.db")).unwrap();
        identity.key = isolated_test_key(3);
        let lock_dir = dir.path().join("locks");
        let held = identity
            .acquire_in(Duration::from_secs(5), &lock_dir)
            .unwrap();
        let started = Instant::now();
        let error = identity
            .acquire_in(Duration::from_secs(5), &lock_dir)
            .err()
            .expect("nesting on one thread must be refused");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "re-entry waited {:?} instead of failing at once",
            started.elapsed()
        );
        let (holder_site, requester_site) = match &error {
            SqliteError::VolumeLeaseReentry {
                holder_site,
                requester_site,
            } => (holder_site.clone(), requester_site.clone()),
            other => panic!("re-entry must be its own typed error, not contention: {other}"),
        };
        assert!(holder_site.contains(file!()), "{holder_site}");
        assert!(requester_site.contains(file!()), "{requester_site}");
        assert_ne!(holder_site, requester_site);
        drop(held);
        identity
            .acquire_in(Duration::from_millis(100), &lock_dir)
            .expect("the refused request must not disturb the holder's release");
    }

    #[test]
    fn cross_process_child() {
        let Some(db_path) = std::env::var_os("KHIVE_DISK_GUARD_TEST_CHILD_DB") else {
            return;
        };
        let lock_dir =
            PathBuf::from(std::env::var_os("KHIVE_DISK_GUARD_TEST_CHILD_LOCKS").unwrap());
        let expected = std::env::var("KHIVE_DISK_GUARD_TEST_CHILD_EXPECT").unwrap();
        let mut identity = VolumeIdentity::resolve(Path::new(&db_path)).unwrap();
        identity.key = isolated_test_key(2);
        let result = identity.acquire_in(Duration::from_millis(75), &lock_dir);
        match expected.as_str() {
            "timeout" => assert!(matches!(
                result,
                Err(SqliteError::CapacityUnavailable {
                    phase: CapacityUnavailablePhase::Lock,
                    ..
                })
            )),
            "acquired" => assert!(result.is_ok()),
            other => panic!("unexpected child expectation: {other}"),
        }
    }

    #[test]
    fn advisory_lease_is_shared_across_processes_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");
        let lock_dir = dir.path().join("locks");
        let mut identity = VolumeIdentity::resolve(&db_path).unwrap();
        identity.key = isolated_test_key(2);
        let held = identity
            .acquire_in(Duration::from_millis(100), &lock_dir)
            .unwrap();

        let child = |expected: &str| {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "disk_guard::tests::cross_process_child",
                    "--nocapture",
                ])
                .env("KHIVE_DISK_GUARD_TEST_CHILD_DB", &db_path)
                .env("KHIVE_DISK_GUARD_TEST_CHILD_LOCKS", &lock_dir)
                .env("KHIVE_DISK_GUARD_TEST_CHILD_EXPECT", expected)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child {expected} failed: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "child test did not run: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        };

        child("timeout");
        drop(held);
        child("acquired");
    }

    #[cfg(unix)]
    #[test]
    fn existing_database_file_is_the_identity_and_capacity_probe_target() {
        use std::os::unix::fs::{symlink, MetadataExt};
        let fixture = tempfile::tempdir().unwrap();
        let database = fixture.path().join("existing.db");
        fs::write(&database, []).unwrap();
        let alias = fixture.path().join("alias.db");
        symlink(&database, &alias).unwrap();
        let identity = VolumeIdentity::resolve(&alias).unwrap();
        assert_eq!(identity.probe_path(), database.canonicalize().unwrap());
        assert_eq!(
            identity.key,
            VolumeKey::UnixDevice(fs::metadata(&database).unwrap().dev())
        );
        assert_ne!(identity.probe_path(), database.parent().unwrap());
        let missing = VolumeIdentity::resolve(&fixture.path().join("missing/new.db")).unwrap();
        assert_eq!(missing.probe_path(), fixture.path().canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_file_symlink_resolves_target_volume() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        symlink(target.join("new.db"), source.join("alias.db")).unwrap();
        let aliased = VolumeIdentity::resolve(&source.join("alias.db")).unwrap();
        let direct = VolumeIdentity::resolve(&target.join("new.db")).unwrap();
        assert_eq!(aliased.probe_path(), direct.probe_path());
    }
}

#[cfg(test)]
pub(crate) fn observe_close_with_lease(
    conn: &rusqlite::Connection,
    lock_path: PathBuf,
) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct CloseWitness {
        lock_path: PathBuf,
        observed: Arc<AtomicUsize>,
    }
    impl Drop for CloseWitness {
        fn drop(&mut self) {
            // This runs in SQLite's function destructor, where a panic aborts
            // the whole test process; a missing lock file (no lease was ever
            // taken) is recorded as its own status instead.
            let status = match OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.lock_path)
            {
                Err(_) => 4,
                Ok(file) => match fs4::FileExt::try_lock(&file) {
                    Err(fs4::TryLockError::WouldBlock) => 1,
                    Ok(()) => 2,
                    Err(_) => 3,
                },
            };
            self.observed.store(status, Ordering::SeqCst);
        }
    }
    let observed = Arc::new(AtomicUsize::new(0));
    let witness = CloseWitness {
        lock_path,
        observed: Arc::clone(&observed),
    };
    conn.create_scalar_function(
        "lease_close_witness",
        0,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        move |_| {
            let _ = &witness;
            Ok(0_i64)
        },
    )
    .unwrap();
    observed
}
