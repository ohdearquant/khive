//! Write budget tracking for proposal apply operations.

use khive_runtime::RuntimeError;
use khive_types::ProposalChangeset;

/// Per-apply write budget. Tracks new entity/note rows; `None` means unlimited.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WriteBudget {
    pub(crate) max_new_entries: Option<u64>,
    pub(crate) consumed_new_entries: u64,
}

impl WriteBudget {
    pub(crate) fn new(max_new_entries: Option<u64>) -> Self {
        Self {
            max_new_entries,
            consumed_new_entries: 0,
        }
    }

    /// Attempt to consume one entry. Returns `WriteBudgetExceeded` if over limit.
    pub(crate) fn consume_new_entry(&mut self) -> Result<(), RuntimeError> {
        if let Some(max) = self.max_new_entries {
            let next = self.consumed_new_entries + 1;
            if next > max {
                return Err(RuntimeError::WriteBudgetExceeded {
                    max_new_entries: max,
                    attempted_new_entries: next,
                });
            }
            self.consumed_new_entries = next;
        }
        Ok(())
    }
}

/// Count `AddEntity` + `AddNote` steps in a changeset tree for the pre-flight budget check.
pub(crate) fn count_new_entries(changeset: &ProposalChangeset) -> u64 {
    match changeset {
        ProposalChangeset::AddEntity { .. } => 1,
        ProposalChangeset::AddNote { .. } => 1,
        ProposalChangeset::Compound { steps } => steps.iter().map(count_new_entries).sum(),
        _ => 0,
    }
}

/// Return true when a proposal changeset contains a Compound with more than one
/// step. Multi-step Compound cannot be applied atomically with the current
/// runtime/storage APIs, so pack-kg rejects it until an atomic apply primitive
/// exists.
pub(crate) fn has_multi_step_compound(changeset: &ProposalChangeset) -> bool {
    match changeset {
        ProposalChangeset::Compound { steps } => {
            steps.len() > 1 || steps.iter().any(has_multi_step_compound)
        }
        _ => false,
    }
}
