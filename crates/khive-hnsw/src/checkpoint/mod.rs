//! HNSW index checkpointing — snapshots for crash recovery and warm-start restores.

mod ckpt_config;
#[cfg(feature = "checkpoint")]
mod fold_types;
mod snapshot;

pub use ckpt_config::HnswCheckpointConfig;
pub use snapshot::{HnswSnapshot, SnapshotError};

#[cfg(feature = "checkpoint")]
pub use fold_types::{HnswCheckpoint, HnswCheckpointStore};

#[cfg(test)]
pub(crate) use crate::config::DistanceMetric;
#[cfg(test)]
pub(crate) use crate::NodeId;
#[cfg(test)]
pub(crate) use ckpt_config::{metric_to_string, sort_ids};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(all(test, feature = "checkpoint"))]
#[path = "integration_tests.rs"]
mod checkpoint_integration_tests;
