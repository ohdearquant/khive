use super::*;
use std::cell::Cell;
use std::io::{Error, ErrorKind};

#[test]
fn denied_then_not_found_keeps_surviving_bytes() {
    let tree = tempfile::tempdir().unwrap();
    let transient = tree.path().join("temp");
    std::fs::write(&transient, b"temporary").unwrap();
    std::fs::write(tree.path().join("kept"), [1u8; 37]).unwrap();
    let calls = Cell::new(0);
    let waits = Cell::new(0);
    let size = dir_size_with(
        tree.path(),
        true,
        |path| {
            if path == transient {
                calls.set(calls.get() + 1);
                Err(Error::from(if calls.get() == 1 {
                    ErrorKind::PermissionDenied
                } else {
                    ErrorKind::NotFound
                }))
            } else {
                std::fs::symlink_metadata(path)
            }
        },
        || waits.set(waits.get() + 1),
    )
    .expect("vanished descendant must be rechecked after denial");
    assert_eq!(size, 37, "remaining file bytes must be counted");
    assert_eq!(calls.get(), 2);
    assert_eq!(waits.get(), 1);
}

#[test]
fn persistent_denial_remains_fatal_at_bound() {
    let tree = tempfile::tempdir().unwrap();
    let denied = tree.path().join("denied");
    std::fs::write(&denied, b"unreadable").unwrap();
    let calls = Cell::new(0);
    let waits = Cell::new(0);
    let result = dir_size_with(
        tree.path(),
        true,
        |path| {
            if path == denied {
                calls.set(calls.get() + 1);
                Err(Error::from(ErrorKind::PermissionDenied))
            } else {
                std::fs::symlink_metadata(path)
            }
        },
        || waits.set(waits.get() + 1),
    );
    assert!(
        matches!(result, Err(CacheError::Io(error)) if error.kind() == ErrorKind::PermissionDenied),
        "persistent denial must not evade byte cap"
    );
    assert_eq!(calls.get(), DIR_SIZE_DENIED_RETRIES + 1);
    assert_eq!(waits.get(), DIR_SIZE_DENIED_RETRIES);
}

#[test]
fn root_denial_is_fatal_without_retry() {
    let tree = tempfile::tempdir().unwrap();
    let calls = Cell::new(0);
    let waits = Cell::new(0);
    let result = dir_size_with(
        tree.path(),
        true,
        |_| {
            calls.set(calls.get() + 1);
            Err(Error::from(ErrorKind::PermissionDenied))
        },
        || waits.set(waits.get() + 1),
    );
    assert!(
        matches!(result, Err(CacheError::Io(error)) if error.kind() == ErrorKind::PermissionDenied)
    );
    assert_eq!(calls.get(), 1, "root must never retry");
    assert_eq!(waits.get(), 0);
}

#[test]
fn descendant_not_found_and_non_windows_denial_unchanged() {
    let tree = tempfile::tempdir().unwrap();
    let transient = tree.path().join("temp");
    std::fs::write(&transient, b"temporary").unwrap();
    for error in [ErrorKind::NotFound, ErrorKind::PermissionDenied] {
        let calls = Cell::new(0);
        let waits = Cell::new(0);
        let result = dir_size_with(
            tree.path(),
            false,
            |path| {
                if path == transient {
                    calls.set(calls.get() + 1);
                    Err(Error::from(error))
                } else {
                    std::fs::symlink_metadata(path)
                }
            },
            || waits.set(waits.get() + 1),
        );
        if error == ErrorKind::NotFound {
            assert_eq!(result.unwrap(), 0);
        } else {
            assert!(result.is_err(), "non-Windows denial must remain fatal");
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(waits.get(), 0);
    }
}

#[cfg(windows)]
#[test]
fn windows_real_delete_pending_descendant_recovers() {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_DISPOSITION_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let tree = tempfile::tempdir().unwrap();
    let pending = tree.path().join("delete-pending");
    std::fs::write(&pending, b"temporary").unwrap();
    std::fs::write(tree.path().join("kept"), [2u8; 41]).unwrap();
    let mut held = Some(
        std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&pending)
            .unwrap(),
    );
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // Use FileDispositionInfo, not the extended POSIX-unlink disposition.
    // SAFETY: the DELETE-capable file handle remains open and disposition is
    // a live FILE_DISPOSITION_INFO with the exact size required by this class.
    assert_ne!(
        unsafe {
            SetFileInformationByHandle(
                held.as_ref().unwrap().as_raw_handle(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    let denied = Cell::new(false);
    let size = dir_size_with(
        tree.path(),
        cfg!(windows),
        |path| {
            let result = std::fs::symlink_metadata(path);
            if path == pending
                && result
                    .as_ref()
                    .is_err_and(|e| e.kind() == ErrorKind::PermissionDenied)
            {
                denied.set(true);
                // Release only after observing the real OS error, avoiding a race
                // between a timer and the walker. The retry then sees NotFound.
                drop(held.take());
            }
            result
        },
        || std::thread::sleep(DIR_SIZE_DENIED_WAIT),
    )
    .expect("real delete-pending entry must recover");
    assert!(
        denied.get(),
        "fixture must observe native delete-pending denial"
    );
    assert_eq!(size, 41, "native retry must count all remaining bytes");
    assert_eq!(dir_size(tree.path()).unwrap(), 41);
}
