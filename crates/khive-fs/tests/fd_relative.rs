//! Behavior of the descriptor-relative helpers against real directories.

#![cfg(unix)]

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{symlink, OpenOptionsExt};
use std::path::{Path, PathBuf};

use khive_fs::fd_relative::{
    clear_errno, current_errno, errno_location, list_names, open_at, open_dir_at, open_file_at,
    stat_at, stat_fd, Create, OpenFileOptions,
};

/// A scratch directory under the system temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let name = format!("khive-fs-{label}-{}", std::process::id());
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn open(&self) -> File {
        File::open(&self.0).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A final symlink refused under `O_NOFOLLOW` reports `ELOOP`, or `ENOTDIR` when the open also
/// requires a directory; platforms differ on which of the two a directory open reports.
fn assert_refused_symlink(error: &std::io::Error) {
    let code = error.raw_os_error().unwrap();
    assert!(
        code == libc::ELOOP || code == libc::ENOTDIR,
        "unexpected errno {code} for a final symlink"
    );
}

#[test]
fn stat_at_reports_a_symlink_as_a_symlink() {
    let scratch = Scratch::new("stat-at");
    std::fs::write(scratch.path().join("target.txt"), b"data").unwrap();
    symlink("target.txt", scratch.path().join("link")).unwrap();
    let dir = scratch.open();

    let link = stat_at(&dir, OsStr::new("link")).unwrap();
    assert_eq!(link.st_mode & libc::S_IFMT, libc::S_IFLNK);

    let file = stat_at(&dir, OsStr::new("target.txt")).unwrap();
    assert_eq!(file.st_mode & libc::S_IFMT, libc::S_IFREG);
}

#[test]
fn file_identity_matches_open_handles_and_hard_links_but_not_copies() {
    use khive_fs::fd_relative::FileIdentity;
    use std::os::unix::fs::MetadataExt;

    let scratch = Scratch::new("identity-handles");
    let original = scratch.path().join("original");
    std::fs::write(&original, b"same contents").unwrap();
    std::fs::hard_link(&original, scratch.path().join("hard-link")).unwrap();
    std::fs::copy(&original, scratch.path().join("copy")).unwrap();
    let first = File::open(&original).unwrap();
    let second = File::open(&original).unwrap();
    let hard_link = File::open(scratch.path().join("hard-link")).unwrap();
    let copy = File::open(scratch.path().join("copy")).unwrap();
    let identity = FileIdentity::of(&first).unwrap();
    let metadata = first.metadata().unwrap();
    assert_eq!(
        identity,
        FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino()
        }
    );
    assert_eq!(identity, FileIdentity::of(&second).unwrap());
    assert_eq!(identity, FileIdentity::of(&hard_link).unwrap());
    assert_ne!(identity, FileIdentity::of(&copy).unwrap());
    let dir = scratch.open();
    assert_eq!(
        identity,
        FileIdentity::at(&dir, OsStr::new("original")).unwrap()
    );
    assert_eq!(
        FileIdentity::of(&dir).unwrap(),
        FileIdentity::at(&dir, OsStr::new(".")).unwrap()
    );
}

#[test]
fn file_identity_at_keeps_byte_names_and_identifies_the_symlink_itself() {
    use khive_fs::fd_relative::FileIdentity;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let scratch = Scratch::new("identity-at");
    // A byte that is not valid UTF-8 exercises the raw-name path on Linux; APFS refuses
    // such names at creation, so other targets use a non-ASCII but valid name instead.
    #[cfg(target_os = "linux")]
    let name = OsStr::from_bytes(b"target-\xff");
    #[cfg(not(target_os = "linux"))]
    let name = OsStr::from_bytes("target-\u{e9}".as_bytes());
    std::fs::write(scratch.path().join(name), b"data").unwrap();
    symlink(name, scratch.path().join("link")).unwrap();
    let dir = scratch.open();
    let target = File::open(scratch.path().join(name)).unwrap();
    let target_identity = FileIdentity::of(&target).unwrap();
    assert_eq!(FileIdentity::at(&dir, name).unwrap(), target_identity);
    let link_identity = FileIdentity::at(&dir, OsStr::new("link")).unwrap();
    let link_metadata = std::fs::symlink_metadata(scratch.path().join("link")).unwrap();
    assert_eq!(
        link_identity,
        FileIdentity {
            dev: link_metadata.dev(),
            ino: link_metadata.ino()
        }
    );
    assert_ne!(link_identity, target_identity);
}

#[test]
fn file_identity_at_preserves_name_refusals_and_os_errors() {
    use khive_fs::fd_relative::FileIdentity;

    let scratch = Scratch::new("identity-errors");
    let dir = scratch.open();
    for name in ["", "sub/file", "/absolute", "nul\0suffix"] {
        let error = FileIdentity::at(&dir, OsStr::new(name)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{name:?}");
        assert_eq!(error.raw_os_error(), None, "{name:?}");
    }
    let missing = FileIdentity::at(&dir, OsStr::new("missing")).unwrap_err();
    assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
    std::fs::write(scratch.path().join("file"), b"data").unwrap();
    let file = File::open(scratch.path().join("file")).unwrap();
    let not_directory = FileIdentity::at(&file, OsStr::new("child")).unwrap_err();
    assert_eq!(not_directory.raw_os_error(), Some(libc::ENOTDIR));
}

#[test]
fn open_at_refuses_a_final_symlink() {
    let scratch = Scratch::new("open-at");
    std::fs::write(scratch.path().join("target.txt"), b"data").unwrap();
    symlink("target.txt", scratch.path().join("link")).unwrap();
    let dir = scratch.open();

    let file = open_at(&dir, OsStr::new("target.txt"), false).unwrap();
    let stat = stat_fd(&file).unwrap();
    assert_eq!(stat.st_mode & libc::S_IFMT, libc::S_IFREG);

    let error = open_at(&dir, OsStr::new("link"), false).unwrap_err();
    assert_refused_symlink(&error);
}

#[test]
fn open_dir_at_refuses_a_final_symlink_and_opens_a_directory() {
    let scratch = Scratch::new("open-dir-at");
    std::fs::create_dir(scratch.path().join("sub")).unwrap();
    symlink("sub", scratch.path().join("link")).unwrap();
    let dir = scratch.open();

    let sub = open_dir_at(&dir, OsStr::new("sub")).unwrap();
    let stat = stat_fd(&sub).unwrap();
    assert_eq!(stat.st_mode & libc::S_IFMT, libc::S_IFDIR);

    let error = open_dir_at(&dir, OsStr::new("link")).unwrap_err();
    assert_refused_symlink(&error);
}

#[test]
fn list_names_omits_dot_entries_and_returns_sorted_names() {
    let scratch = Scratch::new("list-names");
    for name in ["c", "a", ".hidden", "b"] {
        std::fs::write(scratch.path().join(name), b"").unwrap();
    }
    std::fs::create_dir(scratch.path().join("m")).unwrap();
    let dir = scratch.open();

    let names = list_names(&dir).unwrap();
    let names: Vec<_> = names.iter().map(|name| name.to_str().unwrap()).collect();
    assert_eq!(names, [".hidden", "a", "b", "c", "m"]);
}

#[test]
fn current_errno_is_zero_after_clear_errno() {
    // SAFETY: `errno_location` returns the live `errno` cell of this thread.
    unsafe { *errno_location() = libc::EIO };
    assert_eq!(current_errno(), libc::EIO);

    clear_errno();
    assert_eq!(current_errno(), 0);
}

fn writable_options(create: Create) -> OpenFileOptions {
    OpenFileOptions {
        read_write: true,
        create,
        nonblock: false,
        mode: 0o640,
    }
}

#[test]
fn writable_open_preserves_contents_and_obeys_creation_policy() {
    let scratch = Scratch::new("write-create");
    let dir = scratch.open();
    assert_eq!(
        open_file_at(dir.as_fd(), "missing", writable_options(Create::No))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    let mut file = open_file_at(dir.as_fd(), "new", writable_options(Create::Exclusive)).unwrap();
    file.write_all(b"kept").unwrap();
    assert_eq!(
        open_file_at(dir.as_fd(), "new", writable_options(Create::Exclusive))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    for create in [Create::No, Create::IfMissing] {
        let mut reopened = open_file_at(dir.as_fd(), "new", writable_options(create)).unwrap();
        let mut contents = String::new();
        reopened.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "kept");
        reopened.seek(SeekFrom::Start(0)).unwrap();
        reopened.write_all(b"kept").unwrap();
    }
    open_file_at(dir.as_fd(), "created", writable_options(Create::IfMissing)).unwrap();
    assert!(scratch.path().join("created").exists());
}

#[test]
fn writable_open_refuses_symlinks_and_invalid_names() {
    let scratch = Scratch::new("write-refuse");
    let dir = scratch.open();
    std::fs::write(scratch.path().join("target"), b"unchanged").unwrap();
    symlink("target", scratch.path().join("link")).unwrap();
    for create in [Create::No, Create::IfMissing] {
        let err = open_file_at(dir.as_fd(), "link", writable_options(create)).unwrap_err();
        assert_refused_symlink(&err);
    }
    assert!(open_file_at(dir.as_fd(), "link", writable_options(Create::Exclusive)).is_err());
    for name in ["", ".", "..", "sub/file", "/absolute", "nul\0suffix"] {
        let err = open_file_at(dir.as_fd(), name, writable_options(Create::IfMissing)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{name:?}");
        assert_eq!(err.raw_os_error(), None, "{name:?}");
    }
    assert_eq!(
        std::fs::read(scratch.path().join("target")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn writable_open_sets_access_nonblock_and_close_on_exec() {
    let scratch = Scratch::new("write-flags");
    let dir = scratch.open();
    for (name, read_write, nonblock) in [("write", false, true), ("readwrite", true, false)] {
        let file = open_file_at(
            dir.as_fd(),
            name,
            OpenFileOptions {
                read_write,
                nonblock,
                ..writable_options(Create::Exclusive)
            },
        )
        .unwrap();
        // SAFETY: file owns a live descriptor and both fcntl operations only read its flags.
        let status = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: file still owns the live descriptor; this reads close-on-exec flags.
        let descriptor = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert!(status >= 0 && descriptor >= 0);
        assert_eq!(
            status & libc::O_ACCMODE,
            if read_write {
                libc::O_RDWR
            } else {
                libc::O_WRONLY
            }
        );
        assert_eq!(status & libc::O_NONBLOCK != 0, nonblock);
        assert_ne!(descriptor & libc::FD_CLOEXEC, 0);
    }
}

#[test]
fn writable_open_applies_mode_with_the_process_umask() {
    let scratch = Scratch::new("write-mode");
    let dir = scratch.open();
    // Compare to an independent standard-library open under the same umask without changing it.
    for (name, mode) in [("none", 0), ("permissions", 0o777)] {
        let control = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(scratch.path().join(format!("control-{name}")))
            .unwrap();
        let file = open_file_at(
            dir.as_fd(),
            name,
            OpenFileOptions {
                mode,
                ..writable_options(Create::Exclusive)
            },
        )
        .unwrap();
        assert_eq!(
            stat_fd(&file).unwrap().st_mode & 0o777,
            stat_fd(&control).unwrap().st_mode & 0o777
        );
    }
}

mod mutations {
    use super::*;
    use khive_fs::fd_relative::{rename_at, unlink_at};
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn rename_uses_independent_held_parents_and_replaces_the_destination() {
        let scratch = Scratch::new("rename-held-parents");
        let root = scratch.path();
        // APFS refuses non-UTF-8 names (EILSEQ), so the byte-name arm is Linux-only;
        // every other platform exercises the same paths with a UTF-8 name.
        #[cfg(target_os = "linux")]
        let source_name = OsStr::from_bytes(b"source-\xff");
        #[cfg(not(target_os = "linux"))]
        let source_name = OsStr::from_bytes("source-\u{e9}".as_bytes());
        std::fs::create_dir(root.join("from")).unwrap();
        std::fs::create_dir(root.join("to")).unwrap();
        std::fs::write(root.join("from").join(source_name), b"original").unwrap();
        std::fs::write(root.join("to/destination"), b"replaced").unwrap();
        let from = File::open(root.join("from")).unwrap();
        let to = File::open(root.join("to")).unwrap();
        std::fs::rename(root.join("from"), root.join("held")).unwrap();
        std::fs::create_dir(root.join("from")).unwrap();
        std::fs::write(root.join("from").join(source_name), b"decoy").unwrap();

        rename_at(&from, source_name, &to, OsStr::new("destination")).unwrap();

        assert_eq!(
            std::fs::read(root.join("to/destination")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(root.join("from").join(source_name)).unwrap(),
            b"decoy"
        );
        assert_eq!(
            std::fs::symlink_metadata(root.join("held").join(source_name))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn unlink_removes_entries_without_following_links_and_preserves_errors() {
        let scratch = Scratch::new("unlink-entries");
        let dir = scratch.open();
        std::fs::write(scratch.path().join("target"), b"kept").unwrap();
        symlink("target", scratch.path().join("link")).unwrap();
        unlink_at(&dir, OsStr::new("link")).unwrap();
        assert_eq!(
            std::fs::read(scratch.path().join("target")).unwrap(),
            b"kept"
        );
        let missing = unlink_at(&dir, OsStr::new("link")).unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
        std::fs::create_dir(scratch.path().join("directory")).unwrap();
        assert!(unlink_at(&dir, OsStr::new("directory"))
            .unwrap_err()
            .raw_os_error()
            .is_some());
        assert!(scratch.path().join("directory").is_dir());
        unlink_at(&dir, OsStr::new("target")).unwrap();
        assert!(!scratch.path().join("target").exists());
    }

    #[test]
    fn mutations_validate_every_name_before_changing_entries() {
        let scratch = Scratch::new("mutation-names");
        let root = scratch.path();
        let dir = scratch.open();
        std::fs::create_dir(root.join("sub")).unwrap();
        for name in ["source", "destination", "sub/source", "sub/destination"] {
            std::fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        for name in [
            OsStr::new(""),
            OsStr::new("sub/source"),
            OsStr::new("/absolute"),
            OsStr::from_bytes(b"source\0suffix"),
        ] {
            for result in [
                unlink_at(&dir, name),
                rename_at(&dir, name, &dir, OsStr::new("destination")),
                rename_at(&dir, OsStr::new("source"), &dir, name),
            ] {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{name:?}");
                assert_eq!(error.raw_os_error(), None, "{name:?}");
            }
            for name in ["source", "destination", "sub/source", "sub/destination"] {
                assert_eq!(std::fs::read(root.join(name)).unwrap(), name.as_bytes());
            }
        }
    }
}
