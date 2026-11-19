use super::*;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "khive-sync-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create scratch: {e}"),
            }
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn refuse_sync(_: &File) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::EIO))
}

#[test]
fn actual_sync_boundaries_preserve_phase_errno_and_rename_order() {
    for fail_file in [true, false] {
        let scratch = Scratch::new();
        let dir = File::open(&scratch.0).unwrap();
        std::fs::write(scratch.0.join("final"), b"old").unwrap();
        let syncs = if fail_file {
            Syncs {
                file: Some(refuse_sync),
                directory: None,
            }
        } else {
            Syncs {
                file: None,
                directory: Some(refuse_sync),
            }
        };
        let error = publish_with_syncs(
            &dir,
            OsStr::new("tmp"),
            OsStr::new("final"),
            StaleTmp::RefuseExisting.into(),
            |file| file.write_all(b"new"),
            &syncs,
        )
        .unwrap_err();
        assert_eq!(error.io_error().raw_os_error(), Some(libc::EIO));
        assert_eq!(
            error.phase(),
            if fail_file {
                AtomicPublishPhase::SyncTmp
            } else {
                AtomicPublishPhase::SyncDirectory
            }
        );
        assert_eq!(
            std::fs::read(scratch.0.join("final")).unwrap(),
            if fail_file { b"old" } else { b"new" }
        );
        if fail_file {
            assert_eq!(std::fs::read(scratch.0.join("tmp")).unwrap(), b"new");
        } else {
            assert!(!scratch.0.join("tmp").exists());
        }
    }
}
