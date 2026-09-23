//! Write budget tracking for proposal apply operations.

use khive_runtime::{RuntimeError, VerbRegistry};
use khive_types::{NoteDraft, ProposalChangeset};

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

/// Run an `AddNote` draft's owning pack's proposal-note hook, if it registered one
/// (ADR-017's 2026-09-22 amendment). `resolved_kind` is the note kind to look the hook up
/// by; the hook sees a draft carrying that kind, mirroring how `validate_proposal_entity`
/// receives a draft with the canonical kind for `AddEntity`. A kind no pack owns, or an
/// owning pack that registered no hook, leaves the draft unexamined: this seam only ever
/// narrows admission relative to the default, never grants it.
pub(crate) fn check_note_proposal_admission(
    note: &NoteDraft,
    resolved_kind: &str,
    registry: &VerbRegistry,
) -> Result<(), RuntimeError> {
    let Some(hook) = registry.find_kind_hook(resolved_kind) else {
        return Ok(());
    };
    let mut draft = note.clone();
    draft.kind = resolved_kind.to_string();
    hook.validate_proposal_note(&draft)
}

/// Walk a changeset tree and run [`check_note_proposal_admission`] against every `AddNote`
/// step, at proposal creation, before a proposal row exists. Mirrors the recursion shape of
/// `count_new_entries`/`has_multi_step_compound`: an arbitrarily deep single-step `Compound`
/// is walked into, and multi-step `Compound` is rejected earlier by `has_multi_step_compound`
/// so this never has to choose among siblings.
///
/// Unlike the apply-time call in `worker.rs`, no per-step canonical kind has been computed
/// yet here, so each `AddNote`'s kind is trimmed and lowercased before the hook lookup. This
/// is a best-effort match against a pack's declared canonical spelling, not a full kind
/// validation: an unrecognized kind is left for `propose`'s existing behavior to accept and
/// for apply-time `canonical_note_kind` to reject, unchanged by this seam.
pub(crate) fn validate_note_proposal_admission(
    changeset: &ProposalChangeset,
    registry: &VerbRegistry,
) -> Result<(), RuntimeError> {
    match changeset {
        ProposalChangeset::AddNote { note } => {
            let kind = note.kind.trim().to_ascii_lowercase();
            check_note_proposal_admission(note, &kind, registry)
        }
        ProposalChangeset::Compound { steps } => {
            for step in steps {
                validate_note_proposal_admission(step, registry)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
