//! pack-brain — profile management registry for khive.

pub mod fold;
pub mod handlers;
pub mod persist;
pub mod tunable;

mod event;
mod pack;

pub(crate) use pack::sync_balanced_recall_record;
pub use pack::{BrainPack, ENTITY_CACHE_CAPACITY};

#[cfg(test)]
mod tests;
