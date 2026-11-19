//! A name that is not one path component is refused before any system call.

#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use khive_fs::fd_relative::{c_name, list_names, open_at, open_dir_at, stat_at, stat_fd};

/// The one message the name check raises.
const NOT_ONE_COMPONENT: &str = "path component must be a single name";

/// A scratch directory under the system temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let name = format!("khive-fs-names-{label}-{}", std::process::id());
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

/// A pinned directory, `parent`, and two names that each lead somewhere outside it.
///
/// `a/b` goes through a symlink inside `parent`, and the absolute name needs no parent at all.
/// Every target is an existing directory, so each helper would reach it if the kernel were
/// handed the name as it is: only the name check stands in the way.
fn escapes(label: &str) -> (Scratch, File, [OsString; 2]) {
    let scratch = Scratch::new(label);
    let root = scratch.path();
    let parent_path = root.join("parent");
    std::fs::create_dir_all(&parent_path).unwrap();
    std::fs::create_dir_all(root.join("elsewhere/b")).unwrap();
    symlink("../elsewhere", parent_path.join("a")).unwrap();
    let names = [
        OsString::from("a/b"),
        root.join("elsewhere/b").into_os_string(),
    ];
    for name in &names {
        assert!(
            std::fs::metadata(parent_path.join(name)).is_ok(),
            "{name:?} must lead to something that exists"
        );
    }
    let parent = File::open(parent_path).unwrap();
    (scratch, parent, names)
}

/// The error of a refused call; panics when the call was accepted.
fn refusal<T>(result: io::Result<T>, name: &OsStr) -> io::Error {
    match result {
        Ok(_) => panic!("{name:?} was accepted"),
        Err(error) => error,
    }
}

/// The refusal is the name check's own error: no system call ran, so there is no OS error code.
fn assert_not_one_component(error: &io::Error, name: &OsStr) {
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name:?}");
    assert_eq!(error.raw_os_error(), None, "{name:?}");
    assert_eq!(error.to_string(), NOT_ONE_COMPONENT, "{name:?}");
}

fn same_inode(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[test]
fn stat_at_refuses_names_that_are_not_one_component() {
    let (_scratch, parent, names) = escapes("stat-at");
    for name in &names {
        let error = refusal(stat_at(&parent, name), name);
        assert_not_one_component(&error, name);
    }
}

#[test]
fn open_at_refuses_names_that_are_not_one_component() {
    let (_scratch, parent, names) = escapes("open-at");
    for name in &names {
        for directory in [false, true] {
            let error = refusal(open_at(&parent, name, directory), name);
            assert_not_one_component(&error, name);
        }
    }
}

#[test]
fn open_dir_at_refuses_names_that_are_not_one_component() {
    let (_scratch, parent, names) = escapes("open-dir-at");
    for name in &names {
        let error = refusal(open_dir_at(&parent, name), name);
        assert_not_one_component(&error, name);
    }
}

#[test]
fn an_empty_name_is_refused_by_every_helper() {
    let scratch = Scratch::new("empty");
    let dir = scratch.open();
    let name = OsStr::new("");

    assert_not_one_component(&refusal(stat_at(&dir, name), name), name);
    assert_not_one_component(&refusal(open_at(&dir, name, false), name), name);
    assert_not_one_component(&refusal(open_at(&dir, name, true), name), name);
    assert_not_one_component(&refusal(open_dir_at(&dir, name), name), name);
}

#[test]
fn c_name_accepts_single_components_and_refuses_the_rest() {
    for name in [".", "..", "...", ".hidden", "..a", "a.."] {
        let name = OsStr::new(name);
        assert_eq!(c_name(name).unwrap().to_bytes(), name.as_bytes());
    }
    for name in ["", "/", "/a", "a/", "a/b"] {
        let name = OsStr::new(name);
        assert_not_one_component(&refusal(c_name(name), name), name);
    }

    let nul = OsStr::from_bytes(b"a\0b");
    let error = c_name(nul).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "NUL in path component");
}

#[test]
fn dot_is_accepted_and_list_names_still_works() {
    let scratch = Scratch::new("dot");
    std::fs::write(scratch.path().join("file"), b"").unwrap();
    std::fs::create_dir(scratch.path().join("sub")).unwrap();
    let dir = scratch.open();
    let expected = stat_fd(&dir).unwrap();

    let stat = stat_at(&dir, OsStr::new(".")).unwrap();
    assert!(same_inode(&stat, &expected));
    for directory in [false, true] {
        let dot = open_at(&dir, OsStr::new("."), directory).unwrap();
        assert!(same_inode(&stat_fd(&dot).unwrap(), &expected));
    }

    let names = list_names(&dir).unwrap();
    let names: Vec<_> = names.iter().map(|name| name.to_str().unwrap()).collect();
    assert_eq!(names, ["file", "sub"]);
}

#[test]
fn dot_dot_is_accepted_as_a_single_component() {
    let scratch = Scratch::new("dot-dot");
    std::fs::create_dir(scratch.path().join("sub")).unwrap();
    let root = scratch.open();
    let expected = stat_fd(&root).unwrap();
    let sub = open_dir_at(&root, OsStr::new("sub")).unwrap();

    let up = open_dir_at(&sub, OsStr::new("..")).unwrap();
    assert!(same_inode(&stat_fd(&up).unwrap(), &expected));
}

#[test]
fn open_dir_at_refuses_a_regular_file() {
    let scratch = Scratch::new("regular-file");
    std::fs::write(scratch.path().join("plain"), b"data").unwrap();
    let dir = scratch.open();

    let error = open_dir_at(&dir, OsStr::new("plain")).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
}
