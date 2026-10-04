//! Opening the configured mirror root.
//!
//! [`open_source_root`] hands the root to the shared directory walk. The walk opens every
//! component with `O_DIRECTORY | O_NOFOLLOW` relative to the handle of the component before it,
//! reads the target of each link the policy accepts, and enforces the hop budget. This module
//! decides which links the mirror trusts and words each refusal.

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

use khive_fs::directory_walk::{walk_to_directory, BudgetExhausted, LinkContext, LinkPolicy};
use khive_fs::fd_relative::{c_name, open_dir_at, stat_at, stat_fd};

use super::ancestor_symlink_parent_fd_is_trusted;

/// Ancestor links the walk may follow before it refuses the root.
const ANCESTOR_SYMLINK_BUDGET: u32 = 8;

/// Follows an ancestor link only when root owns it and its parent is a root-owned directory that
/// prevents non-root entry replacement. The final component of the root follows no link.
struct RootOwnedAncestorLinks;

impl LinkPolicy for RootOwnedAncestorLinks {
    fn before_follow(&mut self, ctx: &LinkContext<'_>) -> std::io::Result<()> {
        if ctx.is_last {
            // The kernel refused to open the final component because it is a link, and the same
            // open reports the same error. A component that became a directory in between is
            // still refused.
            let refusal = match open_dir_at(ctx.parent, ctx.name) {
                Err(error) => error,
                Ok(_) => std::io::Error::from_raw_os_error(libc::ELOOP),
            };
            return Err(refusal);
        }
        if ctx.link_stat.st_uid != 0 {
            return Err(std::io::Error::other(
                "mirror source root has a non-root-owned ancestor symlink",
            ));
        }
        let parent_stat = stat_fd(ctx.parent)?;
        if !ancestor_symlink_parent_fd_is_trusted(
            ctx.parent.as_raw_fd(),
            parent_stat.st_uid,
            parent_stat.st_mode,
        ) {
            return Err(std::io::Error::other(
                "mirror source root ancestor symlink parent permits non-root entry replacement",
            ));
        }
        Ok(())
    }

    fn after_read(&mut self, ctx: &LinkContext<'_>) -> std::io::Result<()> {
        // The walk has read the target already. This read bounds its length the way the mirror
        // always has; a link whose identity is unchanged still holds the same target.
        let target_length = read_link_length(ctx.parent, ctx.name)?;
        if target_length == 0 || target_length == libc::PATH_MAX as usize {
            return Err(std::io::Error::other(
                "mirror source root ancestor symlink target is empty or too long",
            ));
        }
        let after = stat_at(ctx.parent, ctx.name)?;
        let before = &ctx.link_stat;
        if after.st_dev != before.st_dev
            || after.st_ino != before.st_ino
            || after.st_mode != before.st_mode
            || after.st_uid != before.st_uid
        {
            return Err(std::io::Error::other(
                "mirror source root ancestor symlink changed while resolving",
            ));
        }
        Ok(())
    }
}

/// The length `readlinkat` reports for the link `name` inside `parent`, read into a buffer of
/// `PATH_MAX` bytes.
fn read_link_length(parent: &File, name: &OsStr) -> std::io::Result<usize> {
    let name = c_name(name)?;
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: `parent` is a live descriptor, `name` is NUL-terminated for the call, and `buffer`
    // is a writable region of its declared length.
    let length = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(length as usize)
}

/// Word the walk's own errors the way the mirror reports them.
///
/// A spent link budget arrives as [`BudgetExhausted`], and a path component with an interior NUL
/// byte arrives as an `InvalidInput` error that carries no OS error code. Every other error is
/// already the OS error or the refusal the policy produced, and passes through unchanged.
fn root_walk_error(error: std::io::Error) -> std::io::Error {
    let budget_spent = error
        .get_ref()
        .is_some_and(|inner| inner.is::<BudgetExhausted>());
    if budget_spent {
        return std::io::Error::other("mirror source root exceeds the ancestor symlink limit");
    }
    if error.kind() == std::io::ErrorKind::InvalidInput && error.raw_os_error().is_none() {
        return std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mirror source root contains a NUL byte",
        );
    }
    error
}

/// Walk `root` under `policy` with the mirror's link budget and word the walk's own errors the way
/// the mirror reports them. `open_source_root` passes the mirror's policy; the tests pass one that
/// follows the links their own user can create, since root must own a link the mirror follows.
fn open_root_with<P: LinkPolicy>(root: &Path, policy: &mut P) -> std::io::Result<Vec<File>> {
    // Retain every directory handle until the source leaf opens.
    let walked = walk_to_directory(root, policy, ANCESTOR_SYMLINK_BUDGET);
    walked.map_err(root_walk_error)
}

/// Prove the configured root through native directory handles before its
/// identity can become the first probe's witness. Root-owned ancestor links
/// resolve only when a root-owned parent prevents non-root entry replacement.
/// The final root component still refuses every link.
/// No pathname canonicalization admits a root.
pub(super) fn open_source_root(root: &Path) -> std::io::Result<Vec<File>> {
    let mut policy = RootOwnedAncestorLinks;
    open_root_with(root, &mut policy)
}

#[cfg(test)]
#[path = "root_walk_tests.rs"]
mod tests;
