//! pack-brain — profile-oriented Bayesian auto-tuning for khive.

pub mod event;
pub mod fold;
pub mod handlers;
pub mod persist;
pub mod section;
pub mod state;
pub mod tunable;

mod pack;

pub(crate) use pack::sync_balanced_recall_record;
pub use pack::{BrainPack, ENTITY_CACHE_CAPACITY};

#[cfg(test)]
mod tests;
