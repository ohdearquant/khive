//! Publication behavior against real held directories, without replacing I/O with mocks.

#![cfg(unix)]

use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use khive_fs::atomic_publish::{
    publish_atomic_at, publish_atomic_at_detailed, stage_atomic_at, stage_atomic_at_detailed,
    AtomicPublishPhase, StaleTmp,
};
use khive_fs::fd_relative::rename_at;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "khive-fs-publish-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create scratch directory: {e}"),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn open(&self) -> File {
        File::open(self.path()).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.path());
    }
}

#[test]
fn publication_replaces_final_and_stale_regular_tmp_for_both_policies() {
    for policy in [StaleTmp::Refuse, StaleTmp::Unlink] {
        let scratch = Scratch::new();
        let dir = scratch.open();
        for stale_exists in [false, true] {
            fs::write(scratch.path().join("final"), b"old final").unwrap();
            if stale_exists {
                fs::write(scratch.path().join("tmp"), b"old staging bytes").unwrap();
            }
            publish_atomic_at(&dir, "tmp", "final", policy, |file| {
                file.write_all(b"new bytes")
            })
            .unwrap();
            assert_eq!(
                fs::read(scratch.path().join("final")).unwrap(),
                b"new bytes"
            );
            assert!(!scratch.path().join("tmp").exists());
        }
    }
}

#[test]
fn staging_leaves_the_destination_untouched_until_explicit_promotion() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    fs::write(scratch.path().join("final"), b"old final").unwrap();
    stage_atomic_at(&dir, "tmp", StaleTmp::Refuse, |file| {
        file.write_all(b"staged")
    })
    .unwrap();
    assert_eq!(
        fs::read(scratch.path().join("final")).unwrap(),
        b"old final"
    );
    assert_eq!(fs::read(scratch.path().join("tmp")).unwrap(), b"staged");
    rename_at(&dir, OsStr::new("tmp"), &dir, OsStr::new("final")).unwrap();
    dir.sync_all().unwrap();
    assert_eq!(fs::read(scratch.path().join("final")).unwrap(), b"staged");
}

#[test]
fn refuse_preserves_nonregular_entries_and_never_calls_writer() {
    for entry in ["symlink", "fifo", "directory"] {
        let scratch = Scratch::new();
        let dir = scratch.open();
        fs::write(scratch.path().join("target"), b"precious").unwrap();
        fs::write(scratch.path().join("final"), b"old final").unwrap();
        let tmp = scratch.path().join("tmp");
        match entry {
            "symlink" => symlink("target", &tmp).unwrap(),
            "directory" => fs::create_dir(&tmp).unwrap(),
            "fifo" => {
                let name = CString::new(tmp.as_os_str().as_bytes()).unwrap();
                // SAFETY: name is a live NUL-terminated path under this test's scratch directory.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        let error = publish_atomic_at_detailed(&dir, "tmp", "final", StaleTmp::Refuse, |_| {
            panic!("nonregular staging entry must refuse before callback")
        })
        .unwrap_err();
        assert_eq!(error.phase(), AtomicPublishPhase::RefuseTmp);
        assert_eq!(error.io_error().kind(), io::ErrorKind::InvalidData);
        assert!(fs::symlink_metadata(tmp).is_ok());
        assert_eq!(
            fs::read(scratch.path().join("target")).unwrap(),
            b"precious"
        );
        assert_eq!(
            fs::read(scratch.path().join("final")).unwrap(),
            b"old final"
        );
    }
}

#[test]
fn unlink_removes_symlink_entry_but_preserves_target_and_refuses_directory() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    fs::write(scratch.path().join("target"), b"precious").unwrap();
    symlink("target", scratch.path().join("tmp")).unwrap();
    publish_atomic_at(&dir, "tmp", "final", StaleTmp::Unlink, |file| {
        file.write_all(b"published")
    })
    .unwrap();
    assert_eq!(
        fs::read(scratch.path().join("target")).unwrap(),
        b"precious"
    );
    assert_eq!(
        fs::read(scratch.path().join("final")).unwrap(),
        b"published"
    );

    fs::create_dir(scratch.path().join("tmp")).unwrap();
    let error = publish_atomic_at_detailed(&dir, "tmp", "final", StaleTmp::Unlink, |_| {
        panic!("directory unlink must fail before callback")
    })
    .unwrap_err();
    assert_eq!(error.phase(), AtomicPublishPhase::RemoveTmp);
    assert!(error.io_error().raw_os_error().is_some());
    assert!(scratch.path().join("tmp").is_dir());
    assert_eq!(
        fs::read(scratch.path().join("final")).unwrap(),
        b"published"
    );
}

#[test]
fn invalid_names_refuse_before_removing_tmp_or_calling_writer() {
    for policy in [StaleTmp::Refuse, StaleTmp::Unlink] {
        let scratch = Scratch::new();
        let dir = scratch.open();
        fs::write(scratch.path().join("tmp"), b"stale").unwrap();
        fs::write(scratch.path().join("final"), b"incumbent").unwrap();
        for invalid in ["", ".", "..", "a/b", "/absolute", "nul\0suffix"] {
            for (tmp, destination) in [("tmp", invalid), (invalid, "final")] {
                let error = publish_atomic_at_detailed(&dir, tmp, destination, policy, |_| {
                    panic!("invalid name must refuse before callback")
                })
                .unwrap_err();
                assert_eq!(error.phase(), AtomicPublishPhase::ValidateNames);
                assert_eq!(error.io_error().kind(), io::ErrorKind::InvalidInput);
                assert_eq!(fs::read(scratch.path().join("tmp")).unwrap(), b"stale");
                assert_eq!(
                    fs::read(scratch.path().join("final")).unwrap(),
                    b"incumbent"
                );
            }
            let error = stage_atomic_at_detailed(&dir, invalid, policy, |_| {
                panic!("invalid staging name must refuse before callback")
            })
            .unwrap_err();
            assert_eq!(error.phase(), AtomicPublishPhase::ValidateNames);
        }
        let error = publish_atomic_at(&dir, "tmp", "tmp", policy, |_| {
            panic!("equal names must refuse before callback")
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(scratch.path().join("tmp")).unwrap(), b"stale");
    }
}

#[test]
fn writer_failure_retains_partial_tmp_final_and_original_errno() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    fs::write(scratch.path().join("final"), b"old").unwrap();
    let error = publish_atomic_at_detailed(&dir, "tmp", "final", StaleTmp::Refuse, |file| {
        file.write_all(b"partial")?;
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    })
    .unwrap_err();
    assert_eq!(error.phase(), AtomicPublishPhase::WriteTmp);
    assert_eq!(error.io_error().raw_os_error(), Some(libc::ENOSPC));
    let source = std::error::Error::source(&error).unwrap();
    assert_eq!(
        source.downcast_ref::<io::Error>().unwrap().raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(error.into_source().raw_os_error(), Some(libc::ENOSPC));
    assert_eq!(fs::read(scratch.path().join("tmp")).unwrap(), b"partial");
    assert_eq!(fs::read(scratch.path().join("final")).unwrap(), b"old");

    let error = publish_atomic_at(&dir, "tmp", "final", StaleTmp::Refuse, |_| {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    })
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
    assert_eq!(fs::read(scratch.path().join("final")).unwrap(), b"old");
}

#[test]
fn rename_failure_retains_synced_tmp_without_removing_destination() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    fs::create_dir(scratch.path().join("final")).unwrap();
    fs::write(scratch.path().join("final/child"), b"precious").unwrap();
    let error = publish_atomic_at_detailed(&dir, "tmp", "final", StaleTmp::Refuse, |file| {
        file.write_all(b"staged")
    })
    .unwrap_err();
    assert_eq!(error.phase(), AtomicPublishPhase::Rename);
    assert!(error.into_source().raw_os_error().is_some());
    assert_eq!(fs::read(scratch.path().join("tmp")).unwrap(), b"staged");
    assert_eq!(
        fs::read(scratch.path().join("final/child")).unwrap(),
        b"precious"
    );
}

#[test]
fn publication_stays_in_held_directory_after_its_path_is_replaced() {
    let scratch = Scratch::new();
    let original = scratch.path().join("original");
    let moved = scratch.path().join("moved");
    fs::create_dir(&original).unwrap();
    let dir = File::open(&original).unwrap();
    publish_atomic_at(&dir, "tmp", "final", StaleTmp::Refuse, |file| {
        file.write_all(b"held directory")?;
        fs::rename(&original, &moved)?;
        fs::create_dir(&original)?;
        fs::write(original.join("final"), b"replacement directory")
    })
    .unwrap();
    assert_eq!(fs::read(moved.join("final")).unwrap(), b"held directory");
    assert_eq!(
        fs::read(original.join("final")).unwrap(),
        b"replacement directory"
    );
    assert!(!moved.join("tmp").exists());
    assert!(!original.join("tmp").exists());
}

#[test]
fn exclusive_policy_preserves_every_incumbent_without_invoking_writer() {
    for entry in ["regular", "symlink", "fifo", "directory"] {
        let scratch = Scratch::new();
        let dir = scratch.open();
        let tmp = scratch.path().join("tmp");
        fs::write(scratch.path().join("target"), b"precious target").unwrap();
        fs::write(scratch.path().join("final"), b"old final").unwrap();
        match entry {
            "regular" => fs::write(&tmp, b"incumbent").unwrap(),
            "symlink" => symlink("target", &tmp).unwrap(),
            "directory" => fs::create_dir(&tmp).unwrap(),
            "fifo" => {
                let name = CString::new(tmp.as_os_str().as_bytes()).unwrap();
                // SAFETY: name is a live NUL-terminated path in this private scratch directory.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        let before = fs::symlink_metadata(&tmp).unwrap().file_type();
        let mut called = false;
        let error =
            publish_atomic_at_detailed(&dir, "tmp", "final", StaleTmp::RefuseExisting, |_| {
                called = true;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.phase(), AtomicPublishPhase::CreateTmp);
        assert!(!called);
        assert_eq!(fs::symlink_metadata(&tmp).unwrap().file_type(), before);
        if entry == "regular" {
            assert_eq!(fs::read(&tmp).unwrap(), b"incumbent");
        }
        if entry == "symlink" {
            assert_eq!(fs::read_link(&tmp).unwrap(), Path::new("target"));
        }
        assert_eq!(
            fs::read(scratch.path().join("target")).unwrap(),
            b"precious target"
        );
        assert_eq!(
            fs::read(scratch.path().join("final")).unwrap(),
            b"old final"
        );
    }
}

#[test]
fn exclusive_publication_preserves_os_names_and_requested_creation_mode() {
    use khive_fs::atomic_publish::AtomicPublishOptions;
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let dir = scratch.open();
    let name = OsStr::from_bytes(b"archive-\xff");
    let reference = scratch.path().join("reference");
    File::options()
        .write(true)
        .create_new(true)
        .open(&reference)
        .unwrap();
    let mode = fs::metadata(&reference).unwrap().permissions().mode() & 0o777;
    publish_atomic_at(
        &dir,
        "tmp",
        name,
        AtomicPublishOptions {
            stale: StaleTmp::RefuseExisting,
            mode: 0o666,
        },
        |file| file.write_all(b"new archive"),
    )
    .unwrap();
    assert_eq!(fs::read(scratch.path().join(name)).unwrap(), b"new archive");
    assert_eq!(
        fs::metadata(scratch.path().join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        mode
    );
    stage_atomic_at(&dir, "old-policy", StaleTmp::Refuse, |_| Ok(())).unwrap();
    assert_eq!(
        fs::metadata(scratch.path().join("old-policy"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        mode & 0o644
    );
}

#[test]
fn exclusive_policy_validates_names_before_any_entry_change() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    fs::write(scratch.path().join("tmp"), b"incumbent").unwrap();
    for name in ["", ".", "..", "a/b", "nul\0name"] {
        let error = publish_atomic_at_detailed(&dir, "tmp", name, StaleTmp::RefuseExisting, |_| {
            panic!("writer must not run")
        })
        .unwrap_err();
        assert_eq!(error.phase(), AtomicPublishPhase::ValidateNames);
        assert_eq!(fs::read(scratch.path().join("tmp")).unwrap(), b"incumbent");
    }
    let error = publish_atomic_at_detailed(&dir, "tmp", "tmp", StaleTmp::RefuseExisting, |_| {
        panic!("writer must not run")
    })
    .unwrap_err();
    assert_eq!(error.phase(), AtomicPublishPhase::ValidateNames);
    assert_eq!(fs::read(scratch.path().join("tmp")).unwrap(), b"incumbent");
}
