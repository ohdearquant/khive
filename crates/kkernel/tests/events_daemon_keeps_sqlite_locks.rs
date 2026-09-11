//! The events daemon must hold SQLite's advisory locks on its database for as
//! long as it serves it.
//!
//! In WAL mode every open connection keeps a SHARED lock on the database file
//! and a shared lock on the `-shm` "DMS" byte; those locks are how any other
//! connection learns, when it closes, that it is not the last one and must
//! leave the `-wal`/`-shm` sidecars alone. POSIX advisory locks are per
//! process and per inode and are released by the close of ANY descriptor for
//! the inode, so a daemon that opens and closes a descriptor on its own
//! database after SQLite did (a permission pass, an integrity peek) drops the
//! locks without SQLite noticing. An external connection closing afterwards
//! then checkpoints and unlinks the sidecars underneath the daemon, which keeps
//! writing to the unlinked inodes.
//!
//! Locks are invisible to the process that holds them (`F_GETLK` never reports
//! a conflict with one's own locks), so the check has to be cross-process: this
//! test runs the real daemon subcommand as a child and probes both ranges from
//! here with `F_GETLK`, which answers with the type and the pid of a
//! conflicting holder.
#![cfg(unix)]

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// SQLite's pending byte and the SHARED range above it (`os_unix.c`).
const PENDING_BYTE: i64 = 0x4000_0000;
const SHARED_FIRST: i64 = PENDING_BYTE + 2;
const SHARED_SIZE: i64 = 510;
/// The `-shm` byte every connection with an open WAL index holds shared
/// (`UNIX_SHM_DMS`).
const SHM_DMS_BYTE: i64 = 128;
/// The libc crate types `F_WRLCK`/`F_RDLCK` as `c_short` on macOS and as
/// `c_int` on Linux, while `flock.l_type` is `c_short` on both.
#[allow(clippy::unnecessary_cast)]
const F_WRLCK: libc::c_short = libc::F_WRLCK as libc::c_short;
#[allow(clippy::unnecessary_cast)]
const F_RDLCK: libc::c_short = libc::F_RDLCK as libc::c_short;

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Ask the kernel, from this process, whether an exclusive lock over
/// `[start, start + len)` of `path` would conflict with a lock held by another
/// process. Returns the conflicting lock as the kernel describes it: `l_type`
/// is `F_UNLCK` when nothing conflicts, else the holder's type and pid.
fn conflicting_lock(path: &Path, start: i64, len: i64) -> libc::flock {
    // Opening and closing this descriptor releases only THIS process's locks
    // on the inode, of which there are none.
    let file = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("open {} for F_GETLK: {e}", path.display()));
    // SAFETY: `flock` is plain data; every field is set below or zero.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = F_WRLCK;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = start;
    lock.l_len = len;
    // SAFETY: `fd` is a live descriptor owned by `file` for the call; `lock`
    // is a valid `flock` the kernel fills in.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) };
    assert_eq!(
        rc,
        0,
        "F_GETLK on {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    lock
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[test]
fn events_daemon_keeps_shared_locks_on_its_database_while_idle() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // The daemon validates the socket directory's ownership and mode before
    // binding; a private directory passes.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("events.db");
    let socket = dir.path().join("events.sock");
    let stderr_path = dir.path().join("daemon.stderr");
    let stderr = std::fs::File::create(&stderr_path).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .arg("events-daemon")
        .arg("--db")
        .arg(&db)
        .arg("--socket")
        .arg(&socket)
        .current_dir(dir.path())
        .env_remove("KHIVE_DB")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn events daemon");
    let mut daemon = DaemonGuard(child);
    let child = &mut daemon.0;

    // Readiness: the daemon binds the socket only after the schema is ensured,
    // which is after SQLite opened the database and its WAL. Cold builds and
    // loaded machines make this slow; the bound is generous and the failure
    // message carries the child's stderr.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !socket.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "events daemon exited before binding: {status}\n{}",
                std::fs::read_to_string(&stderr_path).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "events daemon did not bind {} in time\n{}",
            socket.display(),
            std::fs::read_to_string(&stderr_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let wal = sidecar(&db, "-wal");
    let shm = sidecar(&db, "-shm");
    assert!(wal.exists(), "WAL mode leaves {} on disk", wal.display());
    assert!(shm.exists(), "WAL mode leaves {} on disk", shm.display());

    let probe = || {
        let db_lock = conflicting_lock(&db, SHARED_FIRST, SHARED_SIZE);
        let dms_lock = conflicting_lock(&shm, SHM_DMS_BYTE, 1);
        (db_lock, dms_lock)
    };
    let daemon_pid = child.id() as libc::pid_t;
    let check = |label: &str, lock: libc::flock, path: &Path| {
        assert_eq!(
            lock.l_type,
            F_RDLCK,
            "{label}: the idle events daemon must hold a shared lock on {} (F_GETLK type {})",
            path.display(),
            lock.l_type
        );
        assert_eq!(
            lock.l_pid,
            daemon_pid,
            "{label}: the shared lock on {} must be the daemon's",
            path.display()
        );
    };

    let (db_lock, dms_lock) = probe();
    check("database SHARED range", db_lock, &db);
    check("shm DMS byte", dms_lock, &shm);

    // The locks are not a boot-time artifact: they are still held after the
    // daemon has been idle for a while and after this process has opened and
    // closed the files several more times.
    std::thread::sleep(Duration::from_millis(500));
    for _ in 0..3 {
        let (db_lock, dms_lock) = probe();
        check("database SHARED range, idle", db_lock, &db);
        check("shm DMS byte, idle", dms_lock, &shm);
    }

    child.kill().unwrap();
    let _ = child.wait();
}
