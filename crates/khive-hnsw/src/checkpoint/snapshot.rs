//! Serializable snapshot of HNSW index state.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::NodeId;

use super::ckpt_config::{sort_ids, HnswCheckpointConfig};

/// Helper for serde skip_serializing_if on legacy vector_count field.
pub(crate) fn is_zero(val: &usize) -> bool {
    *val == 0
}

/// Errors that can occur during snapshot verification.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
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

/// Serializable snapshot of HNSW index state.
/// When `vectors` is non-empty, restore via `restore_from_snapshot_embedded`; otherwise supply an external map.
#[derive(Debug, Clone, Serialize)]
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
    /// `HnswIndex::restore_from_snapshot_embedded` without supplying a
    /// separate vector map.
    ///
    /// Defaults to empty for backward compatibility with snapshots that
    /// pre-date this field (those require an external vector map).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vectors: Vec<(NodeId, Vec<f32>)>,
}

impl HnswSnapshot {
    /// Check if this snapshot is compatible with the given config (m, ef_construction, metric must match).
    pub fn is_compatible(&self, config: &HnswCheckpointConfig) -> bool {
        self.config == *config
    }

    /// Get the number of live (non-tombstoned) vectors in this snapshot.
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

    /// Normalize v1 backward-compat fields after deserialization; call on snapshots of unknown version.
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

    /// Verify internal consistency: counts, ID list lengths, tombstone membership.
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

    /// Check if all ID lists are in canonical (byte-sorted) order for deterministic serialization.
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

    /// Sort ID lists and layer nodes by byte representation for deterministic serialization.
    /// Neighbor lists within each node are intentionally left unsorted.
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

// ── Wire type for validated deserialization ─────────────────────────────

/// Raw wire representation of [`HnswSnapshot`] for serde; normalizes and verifies on `TryFrom`.
#[derive(Serialize, Deserialize)]
struct RawHnswSnapshot {
    #[serde(default, skip_serializing_if = "is_zero")]
    vector_count: usize,
    #[serde(default)]
    total_nodes: usize,
    #[serde(default)]
    live_nodes: usize,
    #[serde(default)]
    tombstone_count: usize,
    max_layer: usize,
    entry_point: Option<NodeId>,
    config: HnswCheckpointConfig,
    indexed_ids: Vec<NodeId>,
    #[serde(default)]
    tombstoned_ids: Vec<NodeId>,
    layers: Vec<Vec<(NodeId, Vec<NodeId>)>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    vectors: Vec<(NodeId, Vec<f32>)>,
}

impl TryFrom<RawHnswSnapshot> for HnswSnapshot {
    type Error = SnapshotError;

    fn try_from(mut raw: RawHnswSnapshot) -> Result<Self, SnapshotError> {
        // Normalize v1 backward-compat fields before validation.
        if raw.total_nodes == 0 {
            if raw.vector_count > 0 {
                raw.total_nodes = raw.vector_count;
                raw.live_nodes = raw.vector_count;
                raw.tombstone_count = 0;
            } else if !raw.indexed_ids.is_empty() {
                raw.total_nodes = raw.indexed_ids.len();
                raw.live_nodes = raw.indexed_ids.len() - raw.tombstoned_ids.len();
                raw.tombstone_count = raw.tombstoned_ids.len();
            }
        }
        if raw.tombstone_count == 0 && !raw.tombstoned_ids.is_empty() {
            raw.tombstone_count = raw.tombstoned_ids.len();
        }

        let snap = HnswSnapshot {
            vector_count: raw.vector_count,
            total_nodes: raw.total_nodes,
            live_nodes: raw.live_nodes,
            tombstone_count: raw.tombstone_count,
            max_layer: raw.max_layer,
            entry_point: raw.entry_point,
            config: raw.config,
            indexed_ids: raw.indexed_ids,
            tombstoned_ids: raw.tombstoned_ids,
            layers: raw.layers,
            vectors: raw.vectors,
        };

        snap.verify()?;
        Ok(snap)
    }
}

impl<'de> Deserialize<'de> for HnswSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawHnswSnapshot::deserialize(deserializer)?;
        HnswSnapshot::try_from(raw).map_err(serde::de::Error::custom)
    }
}
