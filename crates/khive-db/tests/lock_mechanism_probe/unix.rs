//! A fresh parent descriptor used to inspect database bytes would itself
//! release this process's locks when closed. While a lock is held, use
//! fstat/stat or the independent child, never parent-side content inspection.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rusqlite::{ffi, Connection, OpenFlags};

use super::{initialize_database, observe_from_child, Observation, ProbeRequest};

const PENDING_BYTE: i64 = 0x4000_0000;
const SHARED_FIRST: i64 = PENDING_BYTE + 2;
const SHARED_SIZE: i64 = 510;
const OUTSIDE_SQLITE_LOCK_RANGE: i64 = 0;
// libc uses c_short constants on BSD and c_int on Linux; flock stores c_short.
const READ_LOCK: libc::c_short = libc::F_RDLCK as libc::c_short;
const WRITE_LOCK: libc::c_short = libc::F_WRLCK as libc::c_short;
const UNLOCK: libc::c_short = libc::F_UNLCK as libc::c_short;

fn lock_request(kind: libc::c_short, offset: i64, length: i64) -> libc::flock {
    assert!(offset >= 0 && length > 0, "invalid byte-lock range");
    // Zero initialization also covers platform-specific flock fields.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = offset;
    lock.l_len = length;
    lock
}

fn set_lock(file: &File, kind: libc::c_short, offset: i64, length: i64) {
    let lock = lock_request(kind, offset, length);
    // F_SETLK borrows a valid descriptor and an initialized flock for this call.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
    assert_eq!(
        rc,
        0,
        "F_SETLK type={kind} offset={offset} length={length}: {}",
        io::Error::last_os_error()
    );
}

pub(super) fn hold_lock(path: &Path, offset: i64, length: i64) -> File {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open raw lock holder without truncating");
    let identity = file.metadata().expect("fstat raw lock holder");
    set_lock(&file, WRITE_LOCK, offset, length);
    println!(
        "raw-holder pid={} path={} dev={} ino={} offset={offset} length={length}",
        std::process::id(),
        path.display(),
        identity.dev(),
        identity.ino()
    );
    file
}

pub(super) fn observe(request: &ProbeRequest) -> Observation {
    let observer_pid = std::process::id();
    assert_ne!(
        observer_pid, request.holder_pid,
        "F_GETLK must run outside the holder process"
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&request.path)
        .expect("child opens the observed inode");
    let mut lock = lock_request(WRITE_LOCK, request.offset, request.length);
    // Only the independent child opens/closes this observation descriptor.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) };
    if rc != 0 {
        let error = io::Error::last_os_error();
        let errno = error.raw_os_error().expect("F_GETLK failure has errno");
        eprintln!(
            "F_GETLK failed case={} phase={} observer_pid={observer_pid}: {error}",
            request.case, request.phase
        );
        return Observation {
            observer_pid,
            holder_pid: None,
            lock_type: "error".into(),
            offset: request.offset,
            length: request.length,
            native_error: Some(errno),
        };
    }
    let lock_type = match lock.l_type {
        WRITE_LOCK => "write",
        READ_LOCK => "read",
        UNLOCK => "unlocked",
        other => panic!("F_GETLK returned unexpected lock type {other}"),
    };
    Observation {
        observer_pid,
        holder_pid: (lock_type != "unlocked")
            .then(|| u32::try_from(lock.l_pid).expect("conflicting lock has a valid owner pid")),
        lock_type: lock_type.into(),
        offset: lock.l_start,
        length: lock.l_len,
        native_error: None,
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    case: String,
}

impl Fixture {
    fn new(name: &str, second_client: bool) -> Self {
        let directory = tempfile::tempdir().expect("plain temporary database directory");
        let path = directory.path().join("lock-probe.db");
        initialize_database(&path);
        Self {
            _directory: directory,
            path,
            case: format!("{name}/{}", if second_client { "loss" } else { "survival" }),
        }
    }

    fn expect(&self, offset: i64, phase: &str, expected: &str) -> Observation {
        let observation = observe_from_child(&self.path, offset, 1, &self.case, phase);
        assert_ne!(observation.observer_pid, std::process::id());
        assert_eq!(observation.native_error, None, "F_GETLK failed at {phase}");
        assert_eq!(
            observation.lock_type, expected,
            "{} phase={phase} native_error={:?}",
            self.case, observation.native_error
        );
        if expected == "unlocked" {
            assert_eq!(observation.holder_pid, None);
        } else {
            assert_eq!(observation.holder_pid, Some(std::process::id()));
            assert!(observation.offset <= offset, "conflict must cover the byte");
            assert!(
                observation.length == 0
                    || observation
                        .offset
                        .checked_add(observation.length)
                        .is_some_and(|end| end > offset),
                "actual conflict range must cover the requested byte"
            );
        }
        observation
    }

    fn recover(&self, holder: File, offset: i64) {
        set_lock(&holder, UNLOCK, offset, 1);
        self.expect(offset, "after-explicit-raw-unlock", "unlocked");
        close_file(holder);
        self.expect(offset, "after-final-holder-close", "unlocked");
    }
}

fn close_file(file: File) {
    let fd = file.into_raw_fd();
    // Ownership is consumed once; an ambiguous close error is never retried.
    let rc = unsafe { libc::close(fd) };
    assert_eq!(rc, 0, "close failed: {}", io::Error::last_os_error());
}

fn open_warmed_default_reader(path: &Path, case: &str) -> Connection {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open plain SQLite client through the default VFS");
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read rollback journal mode");
    assert_eq!(mode, "delete", "probe must not transition to WAL");
    assert_eq!(
        connection
            .query_row("SELECT value FROM lock_probe", [], |row| row
                .get::<_, i64>(0))
            .expect("warm the second client's schema before the raw lock"),
        1
    );
    assert!(connection.is_autocommit());

    let mut actual_name: *mut libc::c_char = std::ptr::null_mut();
    // SQLite allocates this name; copy it before sqlite3_free. No file is opened.
    let rc = unsafe {
        ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_VFSNAME,
            (&mut actual_name as *mut *mut libc::c_char).cast(),
        )
    };
    assert_eq!(rc, ffi::SQLITE_OK, "obtain actual default-client VFS name");
    assert!(!actual_name.is_null(), "VFS name allocation must succeed");
    let actual = unsafe { CStr::from_ptr(actual_name) }
        .to_string_lossy()
        .into_owned();
    unsafe { ffi::sqlite3_free(actual_name.cast()) };
    let default_vfs = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
    assert!(!default_vfs.is_null(), "SQLite must have a default VFS");
    let default = unsafe { CStr::from_ptr((*default_vfs).zName) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(actual, default, "the client must use the default VFS");
    assert_eq!(actual, "unix", "the probe requires the stock Unix VFS");
    assert_eq!(rusqlite::version(), "3.53.2", "bundled SQLite pin changed");
    println!(
        "sqlite-client case={case} pid={} sqlite={} default_vfs={default} actual_vfs={actual} journal_mode={mode} schema_warmed=true",
        std::process::id(),
        rusqlite::version()
    );
    connection
}

fn begin_reader(connection: &Connection) {
    connection
        .execute_batch("BEGIN DEFERRED")
        .expect("begin default reader transaction");
    assert_eq!(
        connection
            .query_row("SELECT value FROM lock_probe", [], |row| row
                .get::<_, i64>(0))
            .expect("materialize the default reader's SHARED lock"),
        1
    );
    assert!(!connection.is_autocommit());
}

fn commit_then_close_reader(connection: Connection, fixture: &Fixture, offset: i64) {
    connection
        .execute_batch("COMMIT")
        .expect("release the last default SQLite SHARED holder");
    assert!(connection.is_autocommit());
    fixture.expect(
        offset,
        "after-reader-commit-before-descriptor-close",
        "unlocked",
    );
    connection
        .close()
        .unwrap_or_else(|(_, error)| panic!("close default SQLite client: {error}"));
    fixture.expect(offset, "after-default-client-close", "unlocked");
}

#[test]
fn unix_second_raw_descriptor_close_releases_process_lock() {
    for second_client in [false, true] {
        let fixture = Fixture::new("second-raw-close", second_client);
        let second = second_client.then(|| {
            let second = File::open(&fixture.path).expect("open intended second raw descriptor");
            let identity = second.metadata().expect("fstat second raw descriptor");
            println!(
                "second-raw case={} pid={} dev={} ino={}",
                fixture.case,
                std::process::id(),
                identity.dev(),
                identity.ino()
            );
            second
        });
        let holder = hold_lock(&fixture.path, OUTSIDE_SQLITE_LOCK_RANGE, 1);
        if let Some(second) = &second {
            let first = holder.metadata().expect("fstat holder");
            let second = second.metadata().expect("fstat second descriptor");
            assert_eq!((first.dev(), first.ino()), (second.dev(), second.ino()));
        }
        fixture.expect(OUTSIDE_SQLITE_LOCK_RANGE, "before-second-close", "write");
        if let Some(second) = second {
            close_file(second);
        }
        fixture.expect(
            OUTSIDE_SQLITE_LOCK_RANGE,
            "after-second-close-or-control-noop",
            if second_client { "unlocked" } else { "write" },
        );
        fixture.recover(holder, OUTSIDE_SQLITE_LOCK_RANGE);
    }
}

#[test]
fn unix_default_vfs_last_shared_unlock_releases_unrelated_byte() {
    for second_client in [false, true] {
        let fixture = Fixture::new("default-last-shared-unlock", second_client);
        let reader =
            second_client.then(|| open_warmed_default_reader(&fixture.path, &fixture.case));
        const { assert!(OUTSIDE_SQLITE_LOCK_RANGE < PENDING_BYTE) };
        let holder = hold_lock(&fixture.path, OUTSIDE_SQLITE_LOCK_RANGE, 1);
        fixture.expect(OUTSIDE_SQLITE_LOCK_RANGE, "before-default-reader", "write");
        if let Some(reader) = reader {
            begin_reader(&reader);
            fixture.expect(
                OUTSIDE_SQLITE_LOCK_RANGE,
                "reader-active-raw-byte-outside-sqlite-range",
                "write",
            );
            commit_then_close_reader(reader, &fixture, OUTSIDE_SQLITE_LOCK_RANGE);
        } else {
            fixture.expect(OUTSIDE_SQLITE_LOCK_RANGE, "without-default-client", "write");
        }
        fixture.recover(holder, OUTSIDE_SQLITE_LOCK_RANGE);
    }
}

#[test]
fn unix_default_reader_downgrades_process_write_lock() {
    for second_client in [false, true] {
        let fixture = Fixture::new("default-reader-downgrade", second_client);
        let reader =
            second_client.then(|| open_warmed_default_reader(&fixture.path, &fixture.case));
        let holder = hold_lock(&fixture.path, SHARED_FIRST, 1);
        fixture.expect(SHARED_FIRST, "before-default-reader", "write");
        if let Some(reader) = reader {
            begin_reader(&reader);
            let observed =
                fixture.expect(SHARED_FIRST, "reader-active-write-replaced-by-read", "read");
            assert_eq!(
                (observed.offset, observed.length),
                (SHARED_FIRST, SHARED_SIZE),
                "the default reader must replace the raw byte with its SHARED range"
            );
            println!(
                "expected-sqlite-shared-range case={} offset={SHARED_FIRST} length={SHARED_SIZE}",
                fixture.case
            );
            commit_then_close_reader(reader, &fixture, SHARED_FIRST);
        } else {
            fixture.expect(SHARED_FIRST, "without-default-client", "write");
        }
        fixture.recover(holder, SHARED_FIRST);
    }
}
