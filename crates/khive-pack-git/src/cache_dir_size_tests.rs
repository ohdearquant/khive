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
fn persistent_descendant_metadata_denial_is_skipped_after_bounded_retry() {
    let tree = tempfile::tempdir().unwrap();
    let denied = tree.path().join("denied");
    std::fs::write(&denied, b"unreadable").unwrap();
    std::fs::write(tree.path().join("kept"), [1u8; 37]).unwrap();
    let calls = Cell::new(0);
    let waits = Cell::new(0);
    let size = dir_size_with(
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
    )
    .expect("denied descendant metadata is skipped after a bounded retry");
    assert_eq!(size, 37, "all readable descendants remain counted");
    assert_eq!(calls.get(), DIR_SIZE_DENIED_RETRIES + 1);
    assert_eq!(waits.get(), DIR_SIZE_DENIED_RETRIES);
}

#[test]
fn persistent_descendant_directory_open_denial_remains_fatal() {
    let calls = Cell::new(0);
    let waits = Cell::new(0);
    let result = dir_size_io(
        false,
        true,
        false,
        || {
            calls.set(calls.get() + 1);
            Err::<(), _>(Error::from(ErrorKind::PermissionDenied))
        },
        &mut || waits.set(waits.get() + 1),
    );
    assert!(matches!(result, Err(error) if error.kind() == ErrorKind::PermissionDenied));
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
