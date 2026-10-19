use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_NOT_LOCKED};
use windows_sys::Win32::Storage::FileSystem::{
    LockFileEx, UnlockFileEx, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
};
use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

use super::{initialize_database, observe_from_child, Observation, ProbeRequest};

fn open(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .expect("open independent Windows file handle")
}

fn range(offset: i64, length: i64) -> (OVERLAPPED, u32, u32) {
    let offset = u64::try_from(offset).unwrap();
    let length = u64::try_from(length).unwrap();
    assert!(length > 0);
    let overlapped = OVERLAPPED {
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: offset as u32,
                OffsetHigh: (offset >> 32) as u32,
            },
        },
        ..OVERLAPPED::default()
    };
    (overlapped, length as u32, (length >> 32) as u32)
}

fn lock(file: &File, offset: i64, length: i64, exclusive: bool) -> io::Result<()> {
    let (mut overlapped, low, high) = range(offset, length);
    let flags = LOCKFILE_FAIL_IMMEDIATELY
        | if exclusive {
            LOCKFILE_EXCLUSIVE_LOCK
        } else {
            0
        };
    // SAFETY: the synchronous file handle and initialized OVERLAPPED remain
    // live through this nonblocking call. No ownership is transferred.
    if unsafe { LockFileEx(file.as_raw_handle(), flags, 0, low, high, &mut overlapped) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn unlock(file: &File, offset: i64, length: i64) {
    let (mut overlapped, low, high) = range(offset, length);
    // SAFETY: this live handle owns exactly the supplied locked range.
    assert_ne!(
        unsafe { UnlockFileEx(file.as_raw_handle(), 0, low, high, &mut overlapped) },
        0,
        "release owned byte-range lock: {}",
        io::Error::last_os_error()
    );
}

pub(super) fn hold_lock(path: &Path, offset: i64, length: i64) -> File {
    let file = open(path);
    lock(&file, offset, length, true).expect("acquire the fixture's only byte-range lock");
    println!(
        "raw-holder pid={} path={} handle={:?} identity={:?} offset={offset} length={length}",
        std::process::id(),
        path.display(),
        file.as_raw_handle(),
        super::stale_path::identity(&file),
    );
    file
}

pub(super) fn print_filesystem(path: &Path) {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
    let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut root = vec![0_u16; 32_768];
    // SAFETY: the input path is NUL-terminated and root is a live output buffer.
    assert_ne!(
        unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) },
        0,
        "resolve fixture volume: {}",
        io::Error::last_os_error(),
    );
    let mut name = [0_u16; 128];
    let mut serial = 0_u32;
    // SAFETY: root contains the native NUL-terminated volume path. The name
    // and serial buffers are writable and their sizes match the arguments.
    assert_ne!(
        unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        },
        0,
        "inspect fixture filesystem: {}",
        io::Error::last_os_error(),
    );
    let length = name.iter().position(|unit| *unit == 0).unwrap();
    println!(
        "fixture filesystem={} volume_serial={serial}",
        String::from_utf16_lossy(&name[..length]),
    );
}

pub(super) fn observe(request: &ProbeRequest) -> Observation {
    assert_ne!(std::process::id(), request.holder_pid);
    let file = open(&request.path);
    let (lock_type, native_error) = match lock(&file, request.offset, request.length, true) {
        Ok(()) => {
            unlock(&file, request.offset, request.length);
            ("unlocked", None)
        }
        Err(error) if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) => {
            ("conflict", error.raw_os_error())
        }
        Err(error) => panic!("independent LockFileEx observation unavailable: {error}"),
    };
    Observation {
        observer_pid: std::process::id(),
        // LockFileEx reports conflict, not the conflicting process's PID.
        holder_pid: None,
        lock_type: lock_type.into(),
        offset: request.offset,
        length: request.length,
        native_error,
    }
}

#[test]
fn windows_handle_interaction_does_not_release_another_handles_lock() {
    const OFFSET: i64 = 0;
    const LENGTH: i64 = 1;
    for second_client in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handle-lock.db");
        initialize_database(&path);
        let case = if second_client {
            "windows-second-handle"
        } else {
            "windows-no-second-handle-control"
        };
        let holder = hold_lock(&path, OFFSET, LENGTH);
        let before = observe_from_child(&path, OFFSET, LENGTH, case, "before-second-handle");
        assert_eq!(before.lock_type, "conflict");
        if second_client {
            let other = open(&path);
            assert_eq!(
                super::stale_path::identity(&holder),
                super::stale_path::identity(&other),
                "the second handle must name the same physical file",
            );
            println!(
                "case={case} second-handle-opened handle={:?}",
                other.as_raw_handle(),
            );
            let error = lock(&other, OFFSET, LENGTH, false)
                .expect_err("a distinct handle must not replace the owner's exclusive lock");
            assert_eq!(error.raw_os_error(), Some(ERROR_LOCK_VIOLATION as i32));
            let (mut overlapped, low, high) = range(OFFSET, LENGTH);
            // SAFETY: the range is valid, but this distinct live handle owns
            // no lock there; this deliberately tests the refused unlock.
            let unlocked =
                unsafe { UnlockFileEx(other.as_raw_handle(), 0, low, high, &mut overlapped) };
            let error = io::Error::last_os_error();
            assert_eq!(unlocked, 0, "a foreign handle cannot unlock the range");
            assert_eq!(error.raw_os_error(), Some(ERROR_NOT_LOCKED as i32));
            let during = observe_from_child(&path, OFFSET, LENGTH, case, "after-foreign-unlock");
            assert_eq!(during.lock_type, "conflict");
            drop(other);
        }
        let after = observe_from_child(&path, OFFSET, LENGTH, case, "after-second-handle-close");
        assert_eq!(after.lock_type, "conflict");
        // Explicit native unlock makes the independent observer's unlocked
        // positive control independent of asynchronous process teardown.
        unlock(&holder, OFFSET, LENGTH);
        let released = observe_from_child(&path, OFFSET, LENGTH, case, "after-owner-unlock");
        assert_eq!(released.lock_type, "unlocked");
        drop(holder);
    }
}
