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
