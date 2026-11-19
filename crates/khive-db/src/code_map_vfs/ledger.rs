use std::fs::File;
use std::io;

use super::{os, Role};

#[derive(Default)]
pub(super) struct GuardedLedger {
    #[cfg(unix)]
    entries: Vec<GuardedIdentity>,
    // Preserve the Windows process-lifetime file-ID ledger. Unix inode
    // recycling does not justify changing Windows handle/delete semantics.
    #[cfg(windows)]
    entries: Vec<(os::Identity, Role)>,
}

impl GuardedLedger {
    pub(super) fn contains_identity(&self, identity: os::Identity) -> bool {
        #[cfg(unix)]
        {
            self.entries.iter().any(|entry| entry.identity == identity)
        }
        #[cfg(windows)]
        {
            self.entries.iter().any(|(opened, _)| *opened == identity)
        }
    }

    pub(super) fn has_other_role(&self, identity: os::Identity, role: Role) -> bool {
        #[cfg(unix)]
        {
            self.entries
                .iter()
                .any(|entry| entry.identity == identity && entry.role != role)
        }
        #[cfg(windows)]
        {
            self.entries
                .iter()
                .any(|(opened, opened_role)| *opened == identity && *opened_role != role)
        }
    }

    pub(super) fn record(
        &mut self,
        file: &File,
        identity: os::Identity,
        role: Role,
    ) -> io::Result<()> {
        #[cfg(unix)]
        {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.identity == identity && entry.role == role)
            {
                entry.open_handles += 1;
            } else {
                self.entries.push(GuardedIdentity {
                    identity,
                    role,
                    open_handles: 1,
                    anchor: Some(file.try_clone()?),
                });
            }
        }
        #[cfg(windows)]
        {
            let _ = file;
            self.entries.push((identity, role));
        }
        Ok(())
    }

    pub(super) fn release(&mut self, identity: os::Identity, role: Role) {
        #[cfg(unix)]
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.identity == identity && entry.role == role)
        {
            entry.open_handles = entry.open_handles.saturating_sub(1);
        }
        #[cfg(windows)]
        let _ = (identity, role);
    }

    pub(super) fn retire_unlinked(&mut self) {
        #[cfg(unix)]
        self.entries.retain(|entry| !entry.can_retire());
    }

    #[cfg(all(test, unix))]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Pin an admitted Unix physical object rather than retaining a bare inode
/// that can identify a different object after deletion and inode recycling.
#[cfg(unix)]
struct GuardedIdentity {
    identity: os::Identity,
    role: Role,
    open_handles: usize,
    anchor: Option<File>,
}

#[cfg(unix)]
impl GuardedIdentity {
    fn can_retire(&self) -> bool {
        // Named and renamed objects retain their guarded role after SQLite
        // closes. Metadata failure retains the pin and fails closed.
        self.open_handles == 0
            && os::is_unlinked(self.anchor.as_ref().expect("ledger anchor remains open"))
                .unwrap_or(false)
    }
}

#[cfg(unix)]
impl Drop for GuardedIdentity {
    fn drop(&mut self) {
        if let Some(anchor) = self.anchor.take() {
            // Closing a Unix descriptor must preserve other process-wide
            // locks through the existing deferred-close rule.
            os::close_unlocked(anchor, self.identity);
        }
    }
}
