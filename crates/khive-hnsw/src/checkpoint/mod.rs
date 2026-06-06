//! HNSW index checkpointing.
//!
//! Serializable snapshots for crash recovery and warm-start restores.
//! See `docs/checkpoint.md` for tombstone tracking, determinism, and khive-fold integration.

mod ckpt_config;
mod snapshot;

pub use ckpt_config::HnswCheckpointConfig;
pub use snapshot::{HnswSnapshot, SnapshotError};

// Items re-exported for use in inline tests (via `use super::*`)
#[cfg(test)]
pub(crate) use crate::config::DistanceMetric;
#[cfg(test)]
pub(crate) use crate::NodeId;
#[cfg(test)]
pub(crate) use ckpt_config::{metric_to_string, sort_ids};

// ── Feature-gated fold integration ──────────────────────────────────────

/// Type alias for HNSW checkpoints using the fold checkpoint system.
///
/// Wraps an [`HnswSnapshot`] in the generic [`khive_fold::Checkpoint`]
/// envelope which adds checkpoint ID, timestamp, and fold context.
#[cfg(feature = "checkpoint")]
pub type HnswCheckpoint = khive_fold::Checkpoint<HnswSnapshot>;

/// Type alias for an in-memory HNSW checkpoint store.
///
/// Suitable for testing and development. Production deployments should
/// implement [`khive_fold::CheckpointStore`] with durable storage.
#[cfg(feature = "checkpoint")]
pub type HnswCheckpointStore = khive_fold::InMemoryCheckpointStore<HnswSnapshot>;

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(all(test, feature = "checkpoint"))]
#[path = "integration_tests.rs"]
mod checkpoint_integration_tests;
