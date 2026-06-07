//! Feature-gated type aliases for khive-fold integration.

use super::snapshot::HnswSnapshot;

/// HNSW checkpoint wrapped in the khive-fold envelope.
pub type HnswCheckpoint = khive_fold::Checkpoint<HnswSnapshot>;

/// In-memory HNSW checkpoint store for testing and development.
pub type HnswCheckpointStore = khive_fold::InMemoryCheckpointStore<HnswSnapshot>;
