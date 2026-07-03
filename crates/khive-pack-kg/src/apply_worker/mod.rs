//! ProposalApplyWorker — applies approved proposal changesets to the KG.

mod budget;
mod worker;

pub use worker::ProposalApplyWorker;

#[cfg(test)]
pub(crate) use crate::projection_worker::ProposalsProjectionWorker;
pub(crate) use budget::has_multi_step_compound;

#[cfg(test)]
pub(crate) use budget::{count_new_entries, WriteBudget};
#[cfg(test)]
pub(crate) use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
#[cfg(test)]
pub(crate) use khive_storage::EventFilter;
#[cfg(test)]
pub(crate) use khive_types::{EntityDraft, EventKind};

#[cfg(test)]
mod tests;
