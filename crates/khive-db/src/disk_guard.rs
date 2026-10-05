use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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

    pub(crate) fn acquire(
        &self,
        deadline: Duration,
        lock_dir: Option<&Path>,
    ) -> Result<VolumeLease, SqliteError> {
        let lock_dir = lock_dir.ok_or_else(|| {
            lock_error("writable SQLite pool has no configured volume-lock directory")
        })?;
        if !lock_dir.is_absolute() {
            return Err(lock_error(
                "configured volume-lock directory is not absolute",
            ));
        }
        self.acquire_in(deadline, lock_dir)
    }

    fn acquire_in(&self, timeout: Duration, lock_dir: &Path) -> Result<VolumeLease, SqliteError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| lock_error("volume lease deadline overflow"))?;
        fs::create_dir_all(lock_dir)
            .map_err(|error| lock_error(format!("cannot create volume-lock directory: {error}")))?;
        let lock_path = lock_dir.join(self.lock_filename());
        let process = acquire_process_slot(self.key, deadline)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| lock_error(format!("cannot open volume-lock file: {error}")))?;
        #[cfg(any(unix, windows))]
        loop {
            if Instant::now() >= deadline {
                return Err(lock_error(format!(
                    "timed out after {} ms waiting for volume lease",
                    timeout.as_millis()
                )));
            }
            match fs4::FileExt::try_lock(&file) {
                Ok(()) => {
                    return Ok(VolumeLease {
                        file: Some(file),
                        process: Some(process),
                    });
                }
                Err(fs4::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(lock_error(format!(
                            "timed out after {} ms waiting for volume lease",
                            timeout.as_millis()
                        )));
                    }
                    std::thread::sleep(remaining.min(LOCK_POLL_INTERVAL));
                }
                Err(fs4::TryLockError::Error(error)) => {
                    return Err(lock_error(format!("cannot acquire volume lease: {error}")));
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (file, process, lock_path, deadline);
            Err(lock_error(
                "this platform has no advisory file-lock support",
            ))
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
    held: Mutex<HashSet<VolumeKey>>,
    available: Condvar,
}

fn process_registry() -> &'static ProcessRegistry {
    static REGISTRY: OnceLock<ProcessRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| ProcessRegistry {
        held: Mutex::new(HashSet::new()),
        available: Condvar::new(),
    })
}

struct ProcessSlot(VolumeKey);

impl Drop for ProcessSlot {
    fn drop(&mut self) {
        let registry = process_registry();
        registry.held.lock().remove(&self.0);
        registry.available.notify_all();
    }
}

fn acquire_process_slot(key: VolumeKey, deadline: Instant) -> Result<ProcessSlot, SqliteError> {
    let registry = process_registry();
    let mut held = registry.held.lock();
    loop {
        if Instant::now() >= deadline {
            return Err(lock_error("timed out waiting for in-process volume lease"));
        }
        if held.insert(key) {
            return Ok(ProcessSlot(key));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(lock_error("timed out waiting for in-process volume lease"));
        }
        registry.available.wait_for(&mut held, remaining);
    }
}

pub(crate) struct VolumeLease {
    file: Option<File>,
    process: Option<ProcessSlot>,
}

impl Drop for VolumeLease {
    fn drop(&mut self) {
        self.file.take();
        self.process.take();
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
        let error = second
            .acquire_in(Duration::from_millis(30), &lock_dir)
            .err()
            .expect("same-volume lease must time out");
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
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.lock_path)
                .unwrap();
            let status = match fs4::FileExt::try_lock(&file) {
                Err(fs4::TryLockError::WouldBlock) => 1,
                Ok(()) => 2,
                Err(_) => 3,
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
