//! Inspect held SQLite locks from another process: a local F_GETLK query cannot
//! observe the calling process's own locks.
#![cfg(unix)]

use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_runtime::events_split::{direct_backend_for, direct_backend_read_only_for};
use serde_json::{json, Value};

const CHILD_MODE: &str = "KHIVE_EVENTS_REGISTRY_LOCK_TEST_MODE";
const PENDING_BYTE: i64 = 0x4000_0000;
const SHARED_FIRST: i64 = PENDING_BYTE + 2;
const SHARED_SIZE: i64 = 510;
const RESERVED_BYTE: i64 = PENDING_BYTE + 1;
const SHM_DMS_BYTE: i64 = 128;
const WAL_WRITE_BYTE: i64 = 120;
#[allow(clippy::unnecessary_cast)]
const F_WRLCK: libc::c_short = libc::F_WRLCK as libc::c_short;
#[allow(clippy::unnecessary_cast)]
const F_RDLCK: libc::c_short = libc::F_RDLCK as libc::c_short;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_file(path: &Path, child: &mut Child, stderr: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before {}: {}",
            path.display(),
            std::fs::read_to_string(stderr).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "child blocked before {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_parent(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "parent did not release {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_lock(path: &Path, start: i64, len: i64, kind: libc::c_short, pid: u32) {
    let file = std::fs::File::open(path).unwrap();
    // SAFETY: flock is plain data; all input fields are initialized below.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = F_WRLCK;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = start;
    lock.l_len = len;
    // SAFETY: file owns a live descriptor and lock points to initialized data.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) };
    assert_eq!(rc, 0, "F_GETLK failed: {}", std::io::Error::last_os_error());
    assert_eq!(
        lock.l_type,
        kind,
        "held lock lost on {} at {start}",
        path.display()
    );
    assert_eq!(lock.l_pid, pid as libc::pid_t, "unexpected lock owner");
}

fn run_lock_order(read_only_first: bool) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let stderr_path = dir.path().join("child.stderr");
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "events_registry_lock_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    command.env(
        CHILD_MODE,
        if read_only_first {
            "read-only"
        } else {
            "writable"
        },
    );
    let mut child = ChildGuard(command.spawn().unwrap());
    let db = dir.path().join("events.db");
    let shm = dir.path().join("events.db-shm");
    wait_for_file(&dir.path().join("ready"), &mut child.0, &stderr_path);
    let probe = |pid| {
        assert_lock(&db, SHARED_FIRST, SHARED_SIZE, F_RDLCK, pid);
        if read_only_first {
            assert_lock(&db, RESERVED_BYTE, 1, F_WRLCK, pid);
        } else {
            assert_lock(&shm, SHM_DMS_BYTE, 1, F_RDLCK, pid);
            assert_lock(&shm, WAL_WRITE_BYTE, 1, F_WRLCK, pid);
        }
    };
    probe(child.0.id());
    std::fs::write(dir.path().join("construct"), b"").unwrap();
    wait_for_file(&dir.path().join("checked"), &mut child.0, &stderr_path);
    probe(child.0.id());
    let observation: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("observation.json")).unwrap())
            .unwrap();
    std::fs::write(dir.path().join("release"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "child did not finish after release"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "child failed: {}",
        std::fs::read_to_string(stderr_path).unwrap()
    );
    let (existing, requested) = if read_only_first {
        ("read-only", "writable")
    } else {
        ("writable", "read-only")
    };
    assert_eq!(observation["error"], format!(
        "invalid input: events database {} is already open {existing} in this process; cannot open it {requested}; read-only events access requires a separate frozen snapshot",
        std::fs::canonicalize(&db).unwrap().display()
    ));
    assert_eq!(observation["same_mode_reused"], true);
    assert_eq!(observation["read_only_write_refused"], read_only_first);
    assert_eq!(observation["permissions_unchanged"], true);
}

#[test]
fn writable_then_read_only_keeps_sqlite_locks() {
    run_lock_order(false);
}

#[test]
fn read_only_then_writable_keeps_sqlite_locks() {
    run_lock_order(true);
}

#[test]
fn events_registry_lock_child() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let dir = std::env::current_dir().unwrap();
    let db = dir.join("events.db");
    let read_only_first = mode == "read-only";
    if read_only_first {
        let seed = rusqlite::Connection::open(&db).unwrap();
        seed.execute_batch("CREATE TABLE lock_witness(id INTEGER PRIMARY KEY)")
            .unwrap();
        drop(seed);
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let first = if read_only_first {
        direct_backend_read_only_for(&db)
    } else {
        direct_backend_for(&db)
    }
    .unwrap();
    // A read-only pool cannot legally own a write transaction. In that order,
    // a separate SQLite-owned connection witnesses the process's held locks.
    let external_writer = read_only_first.then(|| rusqlite::Connection::open(&db).unwrap());
    let writer = if read_only_first {
        None
    } else {
        Some(first.pool().writer().unwrap())
    };
    let conn = external_writer
        .as_ref()
        .unwrap_or_else(|| writer.as_ref().unwrap().conn());
    if !read_only_first {
        conn.execute_batch("CREATE TABLE lock_witness(id INTEGER PRIMARY KEY)")
            .unwrap();
    }
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let before_mode = std::fs::metadata(&db).unwrap().permissions().mode();
    std::fs::write(dir.join("ready"), b"").unwrap();
    wait_for_parent(&dir.join("construct"));
    std::fs::create_dir(dir.join("alias-parent")).unwrap();
    let alias = dir.join("alias-parent").join("..").join("events.db");
    assert_ne!(alias, db, "fixture must use a distinct lexical path");
    let incompatible = if read_only_first {
        direct_backend_for(&alias)
    } else {
        direct_backend_read_only_for(&alias)
    };
    let error = incompatible.err().map(|error| error.to_string());
    let same = if read_only_first {
        direct_backend_read_only_for(&alias)
    } else {
        direct_backend_for(&alias)
    }
    .unwrap();
    let read_only_write_refused = read_only_first
        && first
            .pool()
            .writer()
            .unwrap()
            .conn()
            .execute("INSERT INTO lock_witness DEFAULT VALUES", [])
            .is_err_and(|error| error.sqlite_error_code() == Some(rusqlite::ErrorCode::ReadOnly));
    let observation = json!({
        "error": error,
        "same_mode_reused": Arc::ptr_eq(&first, &same),
        "read_only_write_refused": read_only_write_refused,
        "permissions_unchanged": before_mode == std::fs::metadata(&db).unwrap().permissions().mode(),
    });
    std::fs::write(
        dir.join("observation.json"),
        serde_json::to_vec(&observation).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.join("checked"), b"").unwrap();
    wait_for_parent(&dir.join("release"));
    conn.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn concurrent_compatible_openers_reuse_one_canonical_entry() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("events.db");
    std::fs::create_dir(dir.path().join("alias-parent")).unwrap();
    let start = Arc::new(std::sync::Barrier::new(8));
    let handles = std::thread::scope(|scope| {
        let threads = (0..8)
            .map(|index| {
                let start = Arc::clone(&start);
                let path = if index % 2 == 0 {
                    db.clone()
                } else {
                    dir.path().join("alias-parent").join("..").join("events.db")
                };
                scope.spawn(move || {
                    start.wait();
                    direct_backend_for(&path).unwrap()
                })
            })
            .collect::<Vec<_>>();
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(handles
        .iter()
        .all(|handle| Arc::ptr_eq(&handles[0], handle)));
    assert!(!handles[0].is_read_only());
}

#[test]
fn read_only_initialization_preserves_missing_and_frozen_files() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.db");
    assert!(direct_backend_read_only_for(&missing).is_err());
    assert!(!missing.exists());
    let db = dir.path().join("frozen.db");
    {
        let seed = rusqlite::Connection::open(&db).unwrap();
        seed.execute_batch("CREATE TABLE frozen_row(id INTEGER PRIMARY KEY)")
            .unwrap();
    }
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o400)).unwrap();
    let before = std::fs::read(&db).unwrap();
    let read_only = direct_backend_read_only_for(&db).unwrap();
    assert!(read_only.is_read_only());
    let writer = read_only.pool().writer().unwrap();
    let error = writer
        .conn()
        .execute("INSERT INTO frozen_row DEFAULT VALUES", [])
        .unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ReadOnly)
    );
    drop(writer);
    assert!(direct_backend_for(&db).is_err());
    assert_eq!(
        std::fs::metadata(&db).unwrap().permissions().mode() & 0o777,
        0o400
    );
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
