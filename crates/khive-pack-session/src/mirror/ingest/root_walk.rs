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

use khive_fs::directory_walk::{
    walk_to_directory, AncestorLinkCondition, AncestorLinkPolicy, AncestorLinkRefusal,
    AncestorWalkEndpoint, BudgetExhausted, LinkContext, LinkPolicy, ANCESTOR_LINK_BUDGET,
};
#[cfg(test)]
use khive_fs::fd_relative::stat_at;
use khive_fs::fd_relative::{c_name, open_dir_at};

struct MirrorAncestorLinks {
    shared: AncestorLinkPolicy,
}

impl MirrorAncestorLinks {
    fn new() -> Self {
        Self {
            shared: AncestorLinkPolicy::new(AncestorWalkEndpoint::FinalTarget),
        }
    }
}

impl LinkPolicy for MirrorAncestorLinks {
    fn before_follow(&mut self, ctx: &LinkContext<'_>) -> std::io::Result<()> {
        if let Err(error) = self.shared.before_follow(ctx) {
            let final_component = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<AncestorLinkRefusal>())
                .is_some_and(|refusal| refusal.condition == AncestorLinkCondition::FinalComponent);
            if final_component {
                return Err(match open_dir_at(ctx.parent, ctx.name) {
                    Err(error) => error,
                    Ok(_) => std::io::Error::from_raw_os_error(libc::ELOOP),
                });
            }
            return Err(error);
        }
        Ok(())
    }

    fn after_read(&mut self, ctx: &LinkContext<'_>) -> std::io::Result<()> {
        let target_length = read_link_length(ctx.parent, ctx.name)?;
        if target_length == 0 || target_length == libc::PATH_MAX as usize {
            return Err(std::io::Error::other(
                "mirror source root ancestor symlink target is empty or too long",
            ));
        }
        self.shared.after_read(ctx)
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
    if error
        .get_ref()
        .is_some_and(|inner| inner.is::<AncestorLinkRefusal>())
    {
        return error;
    }
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
/// follows links the test builds to exercise the generic walk independently.
fn open_root_with<P: LinkPolicy>(root: &Path, policy: &mut P) -> std::io::Result<Vec<File>> {
    // Retain every directory handle until the source leaf opens.
    let walked = walk_to_directory(root, policy, ANCESTOR_LINK_BUDGET);
    walked.map_err(root_walk_error)
}

/// Prove the configured root through native directory handles before its
/// identity can become the first probe's witness. Ancestor trust comes from
/// the shared policy applied to the pinned parent and link metadata.
/// The final root component still refuses every link.
/// No pathname canonicalization admits a root.
pub(super) fn open_source_root(root: &Path) -> std::io::Result<Vec<File>> {
    let mut policy = MirrorAncestorLinks::new();
    open_root_with(root, &mut policy)
}

#[cfg(test)]
#[path = "root_walk_tests.rs"]
mod tests;
