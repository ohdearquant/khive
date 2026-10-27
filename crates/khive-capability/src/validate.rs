//! Library request validation over explicitly classified operations.
//!
//! Runtime integrations must opt into this adapter; this crate does not wire a gate.

use lion_core::{CapId, PluginId, Right};

use crate::capability::CapabilityManager;
use crate::error::KernelError;

pub struct RequestValidator<'a> {
    caps: &'a CapabilityManager,
}

impl<'a> RequestValidator<'a> {
    pub fn new(caps: &'a CapabilityManager) -> Self {
        Self { caps }
    }

    pub fn check(
        &self,
        plugin_id: PluginId,
        cap_id: CapId,
        namespace: &str,
        operation: Operation,
    ) -> Result<(), KernelError> {
        let right = operation.required_right();
        self.caps.validate(plugin_id, cap_id, namespace, right)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Operation {
    Read,
    Create,
    Update,
    Delete,
    Search,
    Link,
    Traverse,
    Send,
    Recall,
    Remember,
    Assign,
    Complete,
    /// Read-only graph export.
    Export,
}

impl Operation {
    pub fn required_right(self) -> Right {
        match self {
            Self::Read | Self::Search | Self::Traverse | Self::Recall | Self::Export => Right::Read,
            Self::Create | Self::Remember | Self::Assign | Self::Link => Right::Create,
            Self::Update | Self::Complete => Right::Write,
            Self::Delete => Right::Delete,
            Self::Send => Right::Send,
        }
    }

    /// Collapse the same operation classification to Read/Write binding rights.
    ///
    /// Read-only operations require Read; all mutations require Write. This is
    /// distinct from the finer Create/Delete/Send bits of `required_right`.
    /// The exhaustive match refuses to silently classify a new operation.
    pub fn binding_right(self) -> Right {
        match self {
            Self::Read | Self::Search | Self::Traverse | Self::Recall | Self::Export => Right::Read,
            Self::Create
            | Self::Remember
            | Self::Assign
            | Self::Link
            | Self::Update
            | Self::Complete
            | Self::Delete
            | Self::Send => Right::Write,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `Operation` variant must classify to exactly one binding right.
    /// This is enforced at compile time by `binding_right`'s exhaustive match
    /// (no wildcard arm); this test additionally pins the read/write split so
    /// a future edit that silently widens the read set is caught in review.
    #[test]
    fn binding_right_splits_read_vs_write() {
        let reads = [
            Operation::Read,
            Operation::Search,
            Operation::Traverse,
            Operation::Recall,
            Operation::Export,
        ];
        let writes = [
            Operation::Create,
            Operation::Remember,
            Operation::Assign,
            Operation::Link,
            Operation::Update,
            Operation::Complete,
            Operation::Delete,
            Operation::Send,
        ];
        for op in reads {
            assert_eq!(op.binding_right(), Right::Read, "{op:?} must need Read");
        }
        for op in writes {
            assert_eq!(op.binding_right(), Right::Write, "{op:?} must need Write");
        }
    }
}
