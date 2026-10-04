//! Behavior of the directory walk against real directories and symlinks.

#![cfg(unix)]

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use khive_fs::directory_walk::{walk_to_directory, BudgetExhausted, LinkContext, LinkPolicy};
use khive_fs::fd_relative::stat_fd;

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

    /// The scratch directory with every symlink in its own path resolved, so the only links
    /// below it are the ones a test creates.
    fn base(&self) -> PathBuf {
        std::fs::canonicalize(&self.0).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The device and inode that name a directory.
fn identity(file: &File) -> (libc::dev_t, libc::ino_t) {
    let stat = stat_fd(file).unwrap();
    (stat.st_dev, stat.st_ino)
}

/// The number of directories named below `/` in an absolute path with no `.` or `..`.
fn depth(path: &Path) -> usize {
    path.components().count() - 1
}

/// Accepts every link and records what the walk showed it.
#[derive(Default)]
struct Recording {
    before: Vec<(OsString, bool)>,
    after: Vec<OsString>,
}

impl LinkPolicy for Recording {
    fn before_follow(&mut self, ctx: &LinkContext<'_>) -> io::Result<()> {
        self.before.push((ctx.name.to_os_string(), ctx.is_last));
        Ok(())
    }

    fn after_read(&mut self, ctx: &LinkContext<'_>) -> io::Result<()> {
        self.after.push(ctx.name.to_os_string());
        Ok(())
    }
}

/// Accepts every link and counts the links whose target was read. It keeps no per-link state,
/// so a walk that never ends does not also grow without bound.
#[derive(Default)]
struct Counting {
    read: u32,
}

impl LinkPolicy for Counting {
    fn before_follow(&mut self, _ctx: &LinkContext<'_>) -> io::Result<()> {
        Ok(())
    }

    fn after_read(&mut self, _ctx: &LinkContext<'_>) -> io::Result<()> {
        self.read += 1;
        Ok(())
    }
}

/// Refuses every link with an error of its own.
struct Refusing;

impl LinkPolicy for Refusing {
    fn before_follow(&mut self, _ctx: &LinkContext<'_>) -> io::Result<()> {
        Err(io::Error::other("refused by test policy"))
    }
}

#[test]
fn accepted_ancestor_symlink_resolves_to_the_canonical_directory() {
    let scratch = Scratch::new("walk-accepted");
    let base = scratch.base();
    std::fs::create_dir_all(base.join("real/inner")).unwrap();
    symlink("real", base.join("link")).unwrap();
    let path = base.join("link/inner");
    let mut policy = Recording::default();

    let handles = walk_to_directory(&path, &mut policy, 8).unwrap();

    let canonical = File::open(std::fs::canonicalize(&path).unwrap()).unwrap();
    assert_eq!(identity(handles.last().unwrap()), identity(&canonical));
    // The anchor, each directory of `base`, then `real` and `inner`; the link is not a handle.
    assert_eq!(handles.len(), 1 + depth(&base) + 2);
    assert_eq!(policy.before, [(OsString::from("link"), false)]);
    assert_eq!(policy.after, [OsString::from("link")]);
}

#[test]
fn an_absolute_link_target_restarts_the_walk_at_the_root() {
    let scratch = Scratch::new("walk-absolute");
    let base = scratch.base();
    std::fs::create_dir_all(base.join("real/inner")).unwrap();
    symlink(base.join("real"), base.join("link")).unwrap();
    let mut policy = Recording::default();

    let handles = walk_to_directory(&base.join("link/inner"), &mut policy, 8).unwrap();

    let inner = File::open(base.join("real/inner")).unwrap();
    assert_eq!(identity(handles.last().unwrap()), identity(&inner));
    // The anchor and `base`, the target's own `/` and `base` again, then `real` and `inner`.
    assert_eq!(handles.len(), 2 * (1 + depth(&base)) + 2);
}

#[test]
fn the_policy_is_told_when_the_link_is_the_last_component() {
    let scratch = Scratch::new("walk-last");
    let base = scratch.base();
    std::fs::create_dir(base.join("real")).unwrap();
    symlink("real", base.join("link")).unwrap();
    let mut policy = Recording::default();

    let handles = walk_to_directory(&base.join("link"), &mut policy, 8).unwrap();

    assert_eq!(policy.before, [(OsString::from("link"), true)]);
    let real = File::open(base.join("real")).unwrap();
    assert_eq!(identity(handles.last().unwrap()), identity(&real));
}

#[test]
fn a_refusing_policy_error_is_returned_unchanged() {
    let scratch = Scratch::new("walk-refused");
    let base = scratch.base();
    std::fs::create_dir(base.join("real")).unwrap();
    symlink("real", base.join("link")).unwrap();

    let error = walk_to_directory(&base.join("link"), &mut Refusing, 8).unwrap_err();

    assert_eq!(error.to_string(), "refused by test policy");
}

#[test]
fn a_symlink_cycle_ends_with_the_budget_error() {
    let scratch = Scratch::new("walk-cycle");
    let base = scratch.base();
    symlink("b", base.join("a")).unwrap();
    symlink("a", base.join("b")).unwrap();
    let mut policy = Counting::default();

    let error = walk_to_directory(&base.join("a/x"), &mut policy, 8).unwrap_err();

    let exhausted = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<BudgetExhausted>())
        .expect("the walk ends with the budget error");
    // The links alternate `a`, `b`, so the ninth one met is `a`.
    assert_eq!(exhausted.component, OsString::from("a"));
    assert_eq!(policy.read, 8);
}
