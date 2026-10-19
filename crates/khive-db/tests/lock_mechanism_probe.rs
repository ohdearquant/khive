//! Routing-free rollback-file lock probes for ADR-085 Amendment 13.
//!
//! These fixtures use bundled SQLite and raw OS clients. They reproduce
//! mechanisms, without constructing a pool, registering a VFS, or routing
//! anything through a worker. Each parent fixture acquires one lock; a fresh
//! process observes it independently before and after the interfering client.

#![cfg(any(unix, windows))]

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[path = "lock_mechanism_probe/stale_path.rs"]
mod stale_path;
#[cfg(unix)]
#[path = "lock_mechanism_probe/unix.rs"]
mod unix;
#[cfg(windows)]
#[path = "lock_mechanism_probe/windows.rs"]
mod windows;

const OBSERVER_REQUEST: &str = "KHIVE_LOCK_MECHANISM_OBSERVER_REQUEST";
const REPORT_PREFIX: &str = "LOCK_MECHANISM_OBSERVATION=";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ProbeRequest {
    path: PathBuf,
    offset: i64,
    length: i64,
    holder_pid: u32,
    case: String,
    phase: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Observation {
    observer_pid: u32,
    holder_pid: Option<u32>,
    lock_type: String,
    offset: i64,
    length: i64,
    native_error: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProbeReport {
    request: ProbeRequest,
    observation: Observation,
}

fn initialize_database(path: &Path) {
    assert_eq!(
        rusqlite::version(),
        "3.53.2",
        "probe's pinned bundled SQLite"
    );
    let conn = rusqlite::Connection::open(path).expect("create plain SQLite fixture");
    let mode: String = conn
        .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
    conn.execute_batch("CREATE TABLE lock_probe(value INTEGER); INSERT INTO lock_probe VALUES(1)")
        .unwrap();
    // SAFETY: SQLite owns this static default-VFS descriptor and its NUL-
    // terminated name for the lifetime of the linked SQLite library.
    let vfs = unsafe {
        let vfs = rusqlite::ffi::sqlite3_vfs_find(std::ptr::null());
        assert!(!vfs.is_null(), "bundled SQLite has no default VFS");
        CStr::from_ptr((*vfs).zName).to_string_lossy().into_owned()
    };
    println!(
        "fixture os={} path={} sqlite={} sqlite_number={} default_vfs={vfs} journal={mode}",
        std::env::consts::OS,
        path.display(),
        rusqlite::version(),
        rusqlite::version_number(),
    );
    print_filesystem(path);
    conn.close()
        .unwrap_or_else(|(_, error)| panic!("close SQLite fixture before raw lock: {error}"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn print_filesystem(path: &Path) {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: statfs is a plain native output structure. The path is NUL-
    // terminated, and the output buffer lives for the synchronous call.
    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::statfs(path.as_ptr(), &mut filesystem) },
        0,
        "inspect fixture filesystem: {}",
        std::io::Error::last_os_error(),
    );
    #[cfg(target_os = "linux")]
    println!("fixture filesystem_type=0x{:x}", filesystem.f_type);
    #[cfg(target_os = "macos")]
    {
        let name: Vec<u8> = filesystem
            .f_fstypename
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        println!("fixture filesystem={}", String::from_utf8_lossy(&name));
    }
}

#[cfg(windows)]
fn print_filesystem(path: &Path) {
    windows::print_filesystem(path);
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn print_filesystem(_path: &Path) {
    println!("fixture filesystem must be recorded by the host on this Unix platform");
}

fn observe_from_child(
    path: &Path,
    offset: i64,
    length: i64,
    case: &str,
    phase: &str,
) -> Observation {
    let request = ProbeRequest {
        path: path.to_path_buf(),
        offset,
        length,
        holder_pid: std::process::id(),
        case: case.into(),
        phase: phase.into(),
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "independent_observer",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(OBSERVER_REQUEST, serde_json::to_string(&request).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start independent lock observer");
    let child_pid = child.id();
    assert_ne!(child_pid, request.holder_pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("wait for lock observer").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("reap timed-out observer");
            panic!(
                "independent lock observer timed out: request={request:?}, stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child
        .wait_with_output()
        .expect("collect lock observer output");
    assert!(
        output.status.success(),
        "observer failed: request={request:?}, status={}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 observer output");
    let reports: Vec<ProbeReport> = stdout
        .lines()
        .filter_map(|line| line.split_once(REPORT_PREFIX).map(|(_, json)| json))
        .map(|json| serde_json::from_str(json).expect("valid observer report"))
        .collect();
    assert_eq!(
        reports.len(),
        1,
        "expected exactly one observation: {stdout}"
    );
    let report = reports.into_iter().next().unwrap();
    assert_eq!(report.request, request);
    assert_eq!(report.observation.observer_pid, child_pid);
    println!("{REPORT_PREFIX}{}", serde_json::to_string(&report).unwrap());
    #[cfg(unix)]
    assert!(
        report.observation.native_error.is_none(),
        "F_GETLK observation failed: {report:?}"
    );
    report.observation
}

#[test]
#[ignore = "invoked only as an independent child lock observer"]
fn independent_observer() {
    let request: ProbeRequest = serde_json::from_str(
        &std::env::var(OBSERVER_REQUEST).expect("child observer requires its fixture request"),
    )
    .unwrap();
    assert_ne!(std::process::id(), request.holder_pid);
    assert!(request.offset >= 0 && request.length > 0);
    #[cfg(unix)]
    let observation = unix::observe(&request);
    #[cfg(windows)]
    let observation = windows::observe(&request);
    let report = ProbeReport {
        request,
        observation,
    };
    println!("{REPORT_PREFIX}{}", serde_json::to_string(&report).unwrap());
}
