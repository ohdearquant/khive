//! Component-by-component directory walk with a caller-supplied link policy.
//!
//! [`walk_to_directory`] opens every component of a path with `O_DIRECTORY | O_NOFOLLOW`
//! relative to the descriptor of the component before it, so a symlink replaced at any level is
//! never followed by ordinary kernel path resolution. A component the kernel refuses because it
//! is a symlink gets one second look: the walk reads its metadata without following it and asks
//! the caller's [`LinkPolicy`] whether the link may be followed. A link the policy accepts is
//! read with `readlinkat` and its target is spliced in front of the components still to be
//! opened, under a budget of links the walk may follow.
//!
//! The walk owns the descriptor handling, the link buffer and the budget. The policy owns the
//! decision about which links to trust.

mod ancestor_policy;
pub use ancestor_policy::{
    AncestorLinkCondition, AncestorLinkPolicy, AncestorLinkRefusal, AncestorWalkEndpoint,
    ANCESTOR_LINK_BUDGET,
};

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use crate::fd_relative::{c_name, open_dir_at, stat_at};

/// A link the walk met, as shown to a [`LinkPolicy`].
pub struct LinkContext<'a> {
    /// The link's name inside its parent directory.
    pub name: &'a OsStr,
    /// Whether no component remains after this link, counting the components of links that were
    /// already followed.
    pub is_last: bool,
    /// The link's metadata, read without following it.
    pub link_stat: libc::stat,
    /// The pinned directory the link was found in.
    pub parent: &'a File,
}

/// The caller's decision about which links a walk may follow.
pub trait LinkPolicy {
    /// Decide whether the walk may follow the link described by `ctx`.
    ///
    /// It runs once the entry is confirmed to be a symlink, before the walk spends any of its
    /// budget or reads the link. An error ends the walk and is returned unchanged.
    fn before_follow(&mut self, ctx: &LinkContext<'_>) -> io::Result<()>;

    /// Check the link described by `ctx` after the walk has read its target.
    ///
    /// `ctx.parent` and `ctx.name` still name the link, so a policy can inspect it again and
    /// refuse a link that changed since `before_follow`. An error ends the walk and is returned
    /// unchanged. The default accepts every link.
    fn after_read(&mut self, _ctx: &LinkContext<'_>) -> io::Result<()> {
        Ok(())
    }
}

/// The error a walk ends with when it meets a link after its budget is spent.
///
/// It travels inside an [`io::Error`]; `get_ref` followed by `downcast_ref` recognizes it.
#[derive(Debug)]
pub struct BudgetExhausted {
    /// The link the walk could not follow.
    pub component: OsString,
}

impl fmt::Display for BudgetExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "symlink budget exhausted at {:?}", self.component)
    }
}

impl std::error::Error for BudgetExhausted {}

/// Open a directory read-only with a close-on-exec descriptor and final-component no-follow.
///
/// Earlier symlinks are resolved normally; a trailing slash or `/.` makes the preceding
/// component an ancestor. This applies the kernel's `O_NOFOLLOW` semantics, not a containment
/// or ancestor-trust policy. A non-directory is refused by the open itself.
pub fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map(OwnedFd::from)
}

/// Open `/` (absolute) or `.` (relative) as the starting descriptor of a walk.
fn open_anchor(absolute: bool) -> io::Result<File> {
    open_dir_nofollow(Path::new(if absolute { "/" } else { "." })).map(File::from)
}

/// Read a symlink target relative to a held directory, preserving its bytes.
///
/// `name` has the single-component contract of [`c_name`]. OS errors, including EINVAL for
/// a non-link, pass through unchanged. The buffer grows up to `PATH_MAX`; a target filling
/// that ceiling is refused as InvalidData, never returned as potentially truncated success.
/// Repeated reads do not prove the link stayed unchanged; callers own identity rechecks.
pub fn read_link_at(parent: &File, name: &OsStr) -> io::Result<PathBuf> {
    let name = c_name(name)?;
    let ceiling = libc::PATH_MAX as usize;
    let mut capacity = 128.min(ceiling);
    loop {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(capacity)
            .map_err(io::Error::other)?;
        buffer.resize(capacity, 0u8);
        // SAFETY: parent is live, name is NUL-terminated, and buffer is writable for the call.
        let length = unsafe {
            libc::readlinkat(
                parent.as_raw_fd(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        if (length as usize) < capacity {
            buffer.truncate(length as usize);
            return Ok(PathBuf::from(OsString::from_vec(buffer)));
        }
        if capacity == ceiling {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "symlink target fills PATH_MAX buffer",
            ));
        }
        capacity = capacity.checked_mul(2).unwrap_or(ceiling).min(ceiling);
    }
}

/// The names `path` asks the walk to open, in order.
///
/// The root, `.` and a prefix name nothing to open. `..` is opened like any other name.
fn names(path: &Path) -> Vec<OsString> {
    let mut opened = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => opened.push(OsString::from("..")),
            Component::Normal(name) => opened.push(name.to_os_string()),
        }
    }
    opened
}

/// Open every directory along `path` and return the pinned handles, the final directory last.
///
/// An absolute path starts at `/` and a relative one at the current directory; that starting
/// handle is the first element of the result, so the result is never empty. Each component is
/// opened with `O_DIRECTORY | O_NOFOLLOW` relative to the handle before it. When the kernel
/// refuses a component with `ELOOP` or `ENOTDIR` and the entry really is a symlink, the walk
/// offers it to `policy` through [`LinkPolicy::before_follow`], spends one unit of `budget`,
/// reads the target and offers it again through [`LinkPolicy::after_read`]. The target is then
/// spliced in front of the components still to be opened: a relative target resolves from the
/// directory the link lives in, and an absolute target restarts at `/`, which is pinned as one
/// more handle.
///
/// Any other failure to open a component returns that open error as it is, and so does a
/// refusal from `policy`. A link met after `budget` links were already followed ends the walk
/// with a [`BudgetExhausted`] error.
pub fn walk_to_directory<P: LinkPolicy>(
    path: &Path,
    policy: &mut P,
    mut budget: u32,
) -> io::Result<Vec<File>> {
    let mut current = open_anchor(path.is_absolute())?;
    let mut pinned = Vec::new();
    let mut remaining: VecDeque<OsString> = names(path).into();
    while let Some(name) = remaining.pop_front() {
        let open_error = match open_dir_at(&current, &name) {
            Ok(next) => {
                pinned.push(std::mem::replace(&mut current, next));
                continue;
            }
            Err(error) => error,
        };
        let raw = open_error.raw_os_error();
        if raw != Some(libc::ELOOP) && raw != Some(libc::ENOTDIR) {
            return Err(open_error);
        }
        let link_stat = match stat_at(&current, &name) {
            Ok(stat) if (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK => stat,
            _ => return Err(open_error),
        };
        let context = LinkContext {
            name: &name,
            is_last: remaining.is_empty(),
            link_stat,
            parent: &current,
        };
        policy.before_follow(&context)?;
        if budget == 0 {
            return Err(io::Error::other(BudgetExhausted { component: name }));
        }
        budget -= 1;
        let link_target = read_link_at(&current, &name)?;
        policy.after_read(&context)?;
        let target = link_target.as_path();
        if target.is_absolute() {
            let anchor = open_anchor(true)?;
            pinned.push(std::mem::replace(&mut current, anchor));
        }
        for name in names(target).into_iter().rev() {
            remaining.push_front(name);
        }
    }
    pinned.push(current);
    Ok(pinned)
}
