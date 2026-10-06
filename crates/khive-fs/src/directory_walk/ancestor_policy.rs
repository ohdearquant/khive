use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;

use super::{LinkContext, LinkPolicy};
use crate::fd_relative::{stat_at, stat_fd};

/// Maximum ancestor links followed by the standard directory-walk policy.
pub const ANCESTOR_LINK_BUDGET: u32 = 8;

/// Whether the walk ends at its target or at the target's parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncestorWalkEndpoint {
    /// The last component names the target and must not be a link.
    FinalTarget,
    /// The last component is an ancestor; the caller opens its target without following links.
    TargetParent,
}

/// The condition that refused an ancestor link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncestorLinkCondition {
    /// A link names the final target.
    FinalComponent,
    /// Neither root nor the effective user owns the link.
    LinkOwner,
    /// Neither root nor the effective user owns its parent.
    ParentOwner,
    /// Group or other may write its parent without sticky protection.
    ParentPermissions,
    /// A macOS parent ACL entry grants a right.
    ParentAclGrant,
    /// The parent ACL could not be inspected completely.
    ParentAclWitness,
    /// The pinned parent's metadata could not be read.
    ParentMetadata,
    /// The link changed or could not be re-inspected after its target was read.
    LinkChanged,
}

impl fmt::Display for AncestorLinkCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FinalComponent => "final_component",
            Self::LinkOwner => "link_owner",
            Self::ParentOwner => "parent_owner",
            Self::ParentPermissions => "parent_permissions",
            Self::ParentAclGrant => "parent_acl_grant",
            Self::ParentAclWitness => "parent_acl_witness",
            Self::ParentMetadata => "parent_metadata",
            Self::LinkChanged => "link_changed",
        })
    }
}

/// A named policy refusal carried inside the walk's `io::Error`.
#[derive(Debug)]
pub struct AncestorLinkRefusal {
    /// The condition that refused the link.
    pub condition: AncestorLinkCondition,
    /// The component inside the pinned parent.
    pub component: OsString,
    /// The link owner observed before reading its target.
    pub link_uid: u32,
    /// The parent owner, when its metadata was available.
    pub parent_uid: Option<u32>,
    /// The parent mode, when its metadata was available.
    pub parent_mode: Option<u32>,
    source: Option<io::Error>,
}

impl fmt::Display for AncestorLinkRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ancestor link {:?} refused: {} (link uid {}, parent uid {:?}, parent mode {:?})",
            self.component, self.condition, self.link_uid, self.parent_uid, self.parent_mode
        )?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AncestorLinkRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParentFacts {
    uid: u32,
    mode: u32,
}

#[derive(Debug)]
struct Failure {
    condition: AncestorLinkCondition,
    parent: Option<ParentFacts>,
    source: Option<io::Error>,
}

impl Failure {
    fn new(condition: AncestorLinkCondition, parent: Option<ParentFacts>) -> Self {
        Self {
            condition,
            parent,
            source: None,
        }
    }

    fn into_io(self, ctx: &LinkContext<'_>) -> io::Error {
        let kind = self
            .source
            .as_ref()
            .map_or(io::ErrorKind::Other, io::Error::kind);
        io::Error::new(
            kind,
            AncestorLinkRefusal {
                condition: self.condition,
                component: ctx.name.to_os_string(),
                link_uid: ctx.link_stat.st_uid,
                parent_uid: self.parent.map(|parent| parent.uid),
                parent_mode: self.parent.map(|parent| parent.mode),
                source: self.source,
            },
        )
    }
}

fn evaluate_before(
    endpoint: AncestorWalkEndpoint,
    is_last: bool,
    link_uid: u32,
    effective_uid: u32,
    parent: impl FnOnce() -> io::Result<ParentFacts>,
    acl_grants: impl FnOnce() -> io::Result<bool>,
) -> Result<(), Failure> {
    use AncestorLinkCondition as Condition;
    if endpoint == AncestorWalkEndpoint::FinalTarget && is_last {
        return Err(Failure::new(Condition::FinalComponent, None));
    }
    if link_uid != 0 && link_uid != effective_uid {
        return Err(Failure::new(Condition::LinkOwner, None));
    }
    let parent = parent().map_err(|source| Failure {
        condition: Condition::ParentMetadata,
        parent: None,
        source: Some(source),
    })?;
    if parent.uid != 0 && parent.uid != effective_uid {
        return Err(Failure::new(Condition::ParentOwner, Some(parent)));
    }
    if parent.mode & 0o022 != 0 && parent.mode & 0o1000 == 0 {
        return Err(Failure::new(Condition::ParentPermissions, Some(parent)));
    }
    let grants = acl_grants().map_err(|source| Failure {
        condition: Condition::ParentAclWitness,
        parent: Some(parent),
        source: Some(source),
    })?;
    if grants {
        return Err(Failure::new(Condition::ParentAclGrant, Some(parent)));
    }
    Ok(())
}

fn identity_unchanged(before: &libc::stat, after: &libc::stat) -> bool {
    before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_mode == after.st_mode
        && before.st_uid == after.st_uid
}

/// Standard ancestor trust policy for mirror, segment and WAL-pin directory walks.
#[derive(Debug)]
pub struct AncestorLinkPolicy {
    endpoint: AncestorWalkEndpoint,
    effective_uid: u32,
}

impl AncestorLinkPolicy {
    /// Capture the effective user and select the final-component interpretation.
    pub fn new(endpoint: AncestorWalkEndpoint) -> Self {
        Self {
            endpoint,
            // SAFETY: geteuid has no arguments and cannot fail.
            effective_uid: unsafe { libc::geteuid() },
        }
    }
}

impl LinkPolicy for AncestorLinkPolicy {
    fn before_follow(&mut self, ctx: &LinkContext<'_>) -> io::Result<()> {
        evaluate_before(
            self.endpoint,
            ctx.is_last,
            ctx.link_stat.st_uid,
            self.effective_uid,
            || {
                let parent = stat_fd(ctx.parent)?;
                Ok(ParentFacts {
                    uid: parent.st_uid,
                    mode: parent.st_mode as u32,
                })
            },
            || parent_acl_grants(ctx.parent),
        )
        .map_err(|failure| failure.into_io(ctx))
    }

    fn after_read(&mut self, ctx: &LinkContext<'_>) -> io::Result<()> {
        let after = stat_at(ctx.parent, ctx.name).map_err(|source| {
            Failure {
                condition: AncestorLinkCondition::LinkChanged,
                parent: None,
                source: Some(source),
            }
            .into_io(ctx)
        })?;
        if !identity_unchanged(&ctx.link_stat, &after) {
            return Err(Failure::new(AncestorLinkCondition::LinkChanged, None).into_io(ctx));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn parent_acl_grants(_parent: &File) -> io::Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "macos", test))]
fn acl_entry_grants(tag: libc::c_int, mask: u64) -> io::Result<bool> {
    match tag {
        1 => Ok(mask != 0),
        2 => Ok(false),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown ACL entry tag",
        )),
    }
}

#[cfg(any(target_os = "macos", test))]
fn darwin_entry_present(status: libc::c_int, error: io::Error) -> io::Result<bool> {
    match status {
        0 => Ok(true),
        -1 if error.raw_os_error() == Some(libc::EINVAL) => Ok(false),
        -1 => Err(error),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected Darwin ACL iterator status",
        )),
    }
}

#[cfg(target_os = "macos")]
fn parent_acl_grants(parent: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    darwin_acl_grants_fd(parent.as_raw_fd())
}

#[cfg(target_os = "macos")]
fn darwin_acl_grants_fd(parent_fd: std::os::fd::RawFd) -> io::Result<bool> {
    const ACL_TYPE_EXTENDED: libc::c_int = 0x100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    const ACL_NEXT_ENTRY: libc::c_int = -1;
    const ACL_MAX_ENTRIES: usize = 128;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_valid(acl: *mut libc::c_void) -> libc::c_int;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            selector: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut libc::c_void, tag: *mut libc::c_int) -> libc::c_int;
        fn acl_get_permset_mask_np(entry: *mut libc::c_void, mask: *mut u64) -> libc::c_int;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }

    struct OwnedAcl(*mut libc::c_void);
    impl Drop for OwnedAcl {
        fn drop(&mut self) {
            // SAFETY: the non-null ACL is owned by this guard and released once.
            unsafe { acl_free(self.0) };
        }
    }

    // SAFETY: the parent descriptor remains pinned and the extended ACL type is fixed.
    let pointer = unsafe { acl_get_fd_np(parent_fd, ACL_TYPE_EXTENDED) };
    if pointer.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    let acl = OwnedAcl(pointer);
    // SAFETY: this owned ACL is live; validation checks its handle, not its entries.
    if unsafe { acl_valid(acl.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for index in 0..=ACL_MAX_ENTRIES {
        let mut entry = std::ptr::null_mut();
        let selector = if index == 0 {
            ACL_FIRST_ENTRY
        } else {
            ACL_NEXT_ENTRY
        };
        // SAFETY: the validated, privately owned ACL and output pointer remain live.
        let status = unsafe { acl_get_entry(acl.0, selector, &mut entry) };
        // Darwin returns 0 for an entry and -1/EINVAL when these fixed selectors exhaust it.
        if !darwin_entry_present(status, io::Error::last_os_error())? {
            return Ok(false);
        }
        if entry.is_null() || index == ACL_MAX_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid or oversized ACL entry sequence",
            ));
        }
        let mut tag = 0;
        let mut mask = 0u64;
        // SAFETY: entry belongs to the live ACL and both output variables have the header types.
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the full permission mask is a uint64_t output in Darwin's ACL interface.
        if unsafe { acl_get_permset_mask_np(entry, &mut mask) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if acl_entry_grants(tag, mask)? {
            return Ok(true);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "ACL iteration did not terminate",
    ))
}

#[cfg(test)]
#[path = "ancestor_policy_tests.rs"]
mod tests;
