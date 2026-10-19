use std::fs::File;
use std::path::Path;

use super::{initialize_database, observe_from_child};

#[cfg(unix)]
use super::unix::hold_lock;
#[cfg(windows)]
use super::windows::hold_lock;

#[cfg(unix)]
pub(super) fn identity(file: &File) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata().unwrap();
    (metadata.dev(), metadata.ino())
}

#[cfg(windows)]
pub(super) fn identity(file: &File) -> (u64, u64) {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: this file owns a live handle; info is the correctly sized
    // output buffer. Querying it does not open or close another handle.
    assert_ne!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) },
        0,
        "read opened file identity: {}",
        std::io::Error::last_os_error()
    );
    (
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    )
}

fn assert_held(path: &Path, case: &str, phase: &str) {
    let observed = observe_from_child(path, 0, 1, case, phase);
    #[cfg(unix)]
    {
        assert_eq!(observed.lock_type, "write");
        assert_eq!(observed.holder_pid, Some(std::process::id()));
    }
    #[cfg(windows)]
    assert_eq!(observed.lock_type, "conflict");
}

#[test]
fn stale_path_truncate_after_rename_changes_the_replacement_while_original_lock_survives() {
    for second_client in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let stale_path = dir.path().join("database.db");
        let held_path = dir.path().join("renamed.db");
        initialize_database(&stale_path);
        let holder = hold_lock(&stale_path, 0, 1);
        let original_identity = identity(&holder);
        let original_size = holder.metadata().unwrap().len();
        let case = if second_client {
            "stale-path-truncate"
        } else {
            "stale-path-no-second-client-control"
        };
        assert_held(&stale_path, case, "before-rename");
        std::fs::rename(&stale_path, &held_path).unwrap();
        initialize_database(&stale_path);
        let replacement = File::open(&stale_path).unwrap();
        let replacement_identity = identity(&replacement);
        assert_ne!(original_identity, replacement_identity);
        drop(replacement);
        let before = std::fs::read(&stale_path).unwrap();
        assert!(!before.is_empty());
        println!(
            "case={case} old_path={} held_path={} original_identity={original_identity:?} \
             replacement_identity={replacement_identity:?} original_bytes={original_size} \
             replacement_bytes_before={}",
            stale_path.display(),
            held_path.display(),
            before.len(),
        );
        assert_held(&held_path, case, "before-stale-client");
        if second_client {
            // A path remembered before rename names a different, unlocked
            // object now. No descriptor for the original is opened/closed.
            std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&stale_path)
                .expect("stale-path client truncates the replacement");
        }
        assert_held(&held_path, case, "after-stale-client");
        let after = std::fs::read(&stale_path).unwrap();
        println!("case={case} replacement_bytes_after={}", after.len());
        if second_client {
            assert!(after.is_empty(), "the stale-path loss must actually occur");
        } else {
            assert_eq!(
                after, before,
                "survival control must preserve replacement bytes"
            );
        }
        assert_eq!(identity(&holder), original_identity);
        assert_eq!(holder.metadata().unwrap().len(), original_size);
        #[cfg(windows)]
        super::windows::unlock(&holder, 0, 1);
        drop(holder);
        let released = observe_from_child(&held_path, 0, 1, case, "after-original-owner-close");
        assert_eq!(released.lock_type, "unlocked");
        let conn = rusqlite::Connection::open(&held_path).unwrap();
        let value: i64 = conn
            .query_row("SELECT value FROM lock_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 1, "the renamed original database remains intact");
    }
}
