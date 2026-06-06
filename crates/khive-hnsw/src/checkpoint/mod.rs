//! HNSW index checkpointing.
//!
//! Serializable snapshots for crash recovery and warm-start restores.
//! See `docs/checkpoint.md` for tombstone tracking, determinism, and khive-fold integration.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::config::DistanceMetric;
use crate::NodeId;

/// Errors that can occur during snapshot verification.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// Node count fields are inconsistent.
    #[error(
        "inconsistent counts: total_nodes ({total}) != live_nodes ({live}) + tombstone_count ({tombstones})"
    )]
    InconsistentCounts {
        /// Total nodes reported.
        total: usize,
        /// Live nodes reported.
        live: usize,
        /// Tombstones reported.
        tombstones: usize,
    },

    /// indexed_ids length doesn't match total_nodes.
    #[error("indexed_ids count mismatch: expected {expected}, got {actual}")]
    IdCountMismatch {
        /// Expected count (total_nodes).
        expected: usize,
        /// Actual indexed_ids length.
        actual: usize,
    },

    /// tombstoned_ids length doesn't match tombstone_count.
    #[error("tombstoned_ids count mismatch: expected {expected}, got {actual}")]
    TombstoneIdCountMismatch {
        /// Expected count (tombstone_count).
        expected: usize,
        /// Actual tombstoned_ids length.
        actual: usize,
    },

    /// Tombstoned ID not found in indexed_ids.
    #[error("tombstoned id {id:?} not found in indexed_ids")]
    TombstoneNotInIndex {
        /// The missing tombstone ID.
        id: NodeId,
    },
}

/// Sort NodeIds by their byte representation for deterministic ordering.
///
/// This ensures consistent ordering across runs regardless of HashMap iteration order,
/// which is critical for reproducible checkpoint hashes and stable index-based encodings.
#[inline]
pub(crate) fn sort_ids(ids: &mut [NodeId]) {
    ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
}

/// Helper for serde skip_serializing_if on legacy vector_count field.
fn is_zero(val: &usize) -> bool {
    *val == 0
}

/// Serializable snapshot of HNSW index state.
///
/// Captures enough information to reconstruct the index without
/// re-indexing all vectors from scratch.
///
/// # Backward Compatibility
///
/// This struct maintains backward compatibility with v1 snapshots that only
/// had `vector_count`. When deserializing old snapshots:
/// - `vector_count` is read and used to populate `total_nodes`/`live_nodes`
/// - New tombstone fields default to empty/zero
/// - Missing `vectors` field defaults to empty (old snapshots require external vector supply)
///
/// Call [`HnswSnapshot::normalize`] after deserialization to ensure consistent state.
///
/// # Warm Start
///
/// When `vectors` is non-empty the snapshot is self-contained: call
/// [`HnswIndex::restore_from_snapshot_embedded`] to restore without supplying
/// an external vector map.  Snapshots produced by [`HnswIndex::snapshot`]
/// always include the full f32 vector data.
///
/// The estimated size overhead is `dimensions × 4 bytes × node_count`.
/// For 384-dim embeddings with 10 K nodes this is ~15 MB — well within
/// typical checkpoint budgets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswSnapshot {
    /// Legacy field for backward compatibility with v1 snapshots.
    /// New code should use `total_nodes` and `live_nodes` instead.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vector_count: usize,

    /// Total number of nodes (including tombstones).
    #[serde(default)]
    pub total_nodes: usize,

    /// Number of live (non-tombstoned) nodes.
    #[serde(default)]
    pub live_nodes: usize,

    /// Number of tombstoned nodes.
    #[serde(default)]
    pub tombstone_count: usize,

    /// Maximum layer in the graph.
    pub max_layer: usize,

    /// Entry point node ID (if any).
    pub entry_point: Option<NodeId>,

    /// Index configuration at checkpoint time.
    pub config: HnswCheckpointConfig,

    /// IDs of all indexed vectors (for verification on restore).
    /// Sorted by byte representation for deterministic ordering.
    pub indexed_ids: Vec<NodeId>,

    /// IDs of tombstoned vectors.
    /// Sorted by byte representation for deterministic ordering.
    #[serde(default)]
    pub tombstoned_ids: Vec<NodeId>,

    /// Graph edges per layer: `layer -> [(node_id, [neighbor_ids])]`.
    /// Node entries within each layer are sorted by NodeId bytes.
    pub layers: Vec<Vec<(NodeId, Vec<NodeId>)>>,

    /// Embedded f32 vector data for self-contained warm-start snapshots.
    ///
    /// Maps each `NodeId` to its raw embedding vector.  When non-empty, the
    /// snapshot is self-contained and can be restored via
    /// [`HnswIndex::restore_from_snapshot_embedded`] without supplying a
    /// separate vector map.
    ///
    /// Defaults to empty for backward compatibility with snapshots that
    /// pre-date this field (those require an external vector map).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vectors: Vec<(NodeId, Vec<f32>)>,
}

/// Subset of [`super::HnswConfig`] relevant for checkpoint compatibility.
///
/// Stored as simple values (e.g. `metric` as `String`) so that checkpoints
/// remain deserializable even if the enum representation changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HnswCheckpointConfig {
    /// Maximum connections per node per layer (M).
    pub m: usize,
    /// Size of dynamic candidate list during construction.
    pub ef_construction: usize,
    /// Distance metric name (e.g. `"cosine"`, `"dot"`, `"euclidean"`).
    pub metric: String,
}

impl HnswCheckpointConfig {
    /// Create a checkpoint config from the full [`super::HnswConfig`].
    pub fn from_hnsw_config(config: &super::config::HnswConfig) -> Self {
        Self {
            m: config.m,
            ef_construction: config.ef_construction,
            metric: metric_to_string(&config.metric),
        }
    }
}

impl HnswSnapshot {
    /// Check if this snapshot is compatible with the given config.
    ///
    /// Two configs are compatible when `m`, `ef_construction`, and `metric`
    /// all match. Loading a snapshot into an index with incompatible
    /// parameters would produce incorrect search results.
    pub fn is_compatible(&self, config: &HnswCheckpointConfig) -> bool {
        self.config == *config
    }

    /// Get the number of live (non-tombstoned) vectors in this snapshot.
    ///
    /// For backward compatibility, this returns `live_nodes` which represents
    /// the same semantic meaning as the legacy `vector_count` (all vectors
    /// were "live" before tombstone support).
    pub fn len(&self) -> usize {
        self.live_nodes
    }

    /// Get the total number of nodes (including tombstones).
    pub fn total_len(&self) -> usize {
        self.total_nodes
    }

    /// Get the number of tombstoned nodes.
    pub fn tombstone_count(&self) -> usize {
        self.tombstone_count
    }

    /// Returns `true` if the snapshot contains no live vectors.
    pub fn is_empty(&self) -> bool {
        self.live_nodes == 0
    }

    /// Normalize the snapshot after deserialization.
    ///
    /// This handles backward compatibility with v1 snapshots that only
    /// had `vector_count`. If `total_nodes` is 0 but `vector_count` > 0
    /// or `indexed_ids` is non-empty, the counts are populated from
    /// available data.
    ///
    /// Call this after deserializing a snapshot of unknown version.
    pub fn normalize(&mut self) {
        // Handle v1 -> v2 migration
        if self.total_nodes == 0 {
            if self.vector_count > 0 {
                // V1 snapshot with vector_count
                self.total_nodes = self.vector_count;
                self.live_nodes = self.vector_count;
                self.tombstone_count = 0;
            } else if !self.indexed_ids.is_empty() {
                // Fallback: infer from indexed_ids
                self.total_nodes = self.indexed_ids.len();
                self.live_nodes = self.indexed_ids.len() - self.tombstoned_ids.len();
                self.tombstone_count = self.tombstoned_ids.len();
            }
        }

        // Ensure tombstone_count matches tombstoned_ids
        if self.tombstone_count == 0 && !self.tombstoned_ids.is_empty() {
            self.tombstone_count = self.tombstoned_ids.len();
        }
    }

    /// Verify internal consistency of the snapshot.
    ///
    /// Checks:
    /// 1. `total_nodes == live_nodes + tombstone_count`
    /// 2. `indexed_ids.len() == total_nodes`
    /// 3. `tombstoned_ids.len() == tombstone_count`
    /// 4. All tombstoned IDs exist in indexed_ids
    ///
    /// Returns `Ok(())` if all invariants hold, otherwise returns the
    /// first error encountered.
    pub fn verify(&self) -> Result<(), SnapshotError> {
        // Check count consistency
        if self.total_nodes != self.live_nodes + self.tombstone_count {
            return Err(SnapshotError::InconsistentCounts {
                total: self.total_nodes,
                live: self.live_nodes,
                tombstones: self.tombstone_count,
            });
        }

        // Check indexed_ids matches total_nodes
        if self.indexed_ids.len() != self.total_nodes {
            return Err(SnapshotError::IdCountMismatch {
                expected: self.total_nodes,
                actual: self.indexed_ids.len(),
            });
        }

        // Check tombstoned_ids matches tombstone_count
        if self.tombstoned_ids.len() != self.tombstone_count {
            return Err(SnapshotError::TombstoneIdCountMismatch {
                expected: self.tombstone_count,
                actual: self.tombstoned_ids.len(),
            });
        }

        // Check all tombstoned IDs are in indexed_ids
        if !self.tombstoned_ids.is_empty() {
            let indexed_set: HashSet<_> = self.indexed_ids.iter().collect();
            for id in &self.tombstoned_ids {
                if !indexed_set.contains(id) {
                    return Err(SnapshotError::TombstoneNotInIndex { id: *id });
                }
            }
        }

        Ok(())
    }

    /// Check if indexed_ids, tombstoned_ids, and layers are in canonical sorted order.
    ///
    /// Canonical order means all ID lists are sorted by their byte representation.
    /// This ensures deterministic serialization and stable index-based encodings.
    pub fn is_canonical(&self) -> bool {
        // Check indexed_ids are sorted
        let ids_sorted = self
            .indexed_ids
            .windows(2)
            .all(|w| w[0].as_bytes() <= w[1].as_bytes());

        if !ids_sorted {
            return false;
        }

        // Check tombstoned_ids are sorted
        let tombstones_sorted = self
            .tombstoned_ids
            .windows(2)
            .all(|w| w[0].as_bytes() <= w[1].as_bytes());

        if !tombstones_sorted {
            return false;
        }

        // Check each layer's nodes are sorted by ID
        for layer in &self.layers {
            let layer_sorted = layer
                .windows(2)
                .all(|w| w[0].0.as_bytes() <= w[1].0.as_bytes());
            if !layer_sorted {
                return false;
            }
        }

        true
    }

    /// Ensure canonical ordering (idempotent).
    ///
    /// Sorts `indexed_ids`, `tombstoned_ids`, and layer node entries by their
    /// byte representation. This should be called before serializing snapshots
    /// to ensure deterministic output.
    ///
    /// # Note
    ///
    /// Neighbor lists within each node are intentionally not sorted, as their order
    /// may reflect proximity/priority from the HNSW algorithm. Only the top-level
    /// node ordering within layers is canonicalized.
    pub fn canonicalize(&mut self) {
        // Sort indexed IDs
        sort_ids(&mut self.indexed_ids);

        // Sort tombstoned IDs
        sort_ids(&mut self.tombstoned_ids);

        // Sort layer node order (but preserve neighbor list order within each node)
        for layer in &mut self.layers {
            layer.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
        }
    }
}

/// Convert a [`DistanceMetric`] to its canonical string representation.
pub(crate) fn metric_to_string(metric: &DistanceMetric) -> String {
    match metric {
        DistanceMetric::Cosine => "cosine".to_string(),
        DistanceMetric::Dot => "dot".to_string(),
        DistanceMetric::L2 => "euclidean".to_string(),
        // Fall back to debug repr for future variants.
        other => format!("{:?}", other).to_lowercase(),
    }
}

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
