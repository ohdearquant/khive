//! Behavior of the descriptor-relative helpers against real directories.

#![cfg(unix)]

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use khive_fs::fd_relative::{
    clear_errno, current_errno, errno_location, list_names, open_at, open_dir_at, stat_at, stat_fd,
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
