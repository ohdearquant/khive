//! HNSW index struct and core methods.

use std::collections::HashMap;
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::SeedableRng;

use crate::metrics::MetricsSink;
use crate::NodeId;

use super::super::config::HnswConfig;
use super::super::node::HnswNode;
use super::super::stats::TombstoneStats;
use super::quantized::QuantizedArena;

/// In-memory HNSW vector index with tombstone-based lazy deletion.
/// Nodes use internal `usize` IDs for O(1) lookups; `NodeId` ↔ `usize` maps live at the API boundary.
pub struct HnswIndex {
    /// Configuration.
    pub(crate) config: HnswConfig,

    /// Dense node storage indexed by internal usize ID.
    pub(crate) nodes: Vec<HnswNode>,

    /// External NodeId -> internal usize mapping.
    /// Only used at API boundary (insert, search result conversion, delete).
    pub(crate) id_to_internal: HashMap<NodeId, usize>,

    /// Internal usize -> external NodeId mapping.
    /// Indexed by internal ID for O(1) reverse lookup.
    pub(crate) internal_to_id: Vec<NodeId>,

    /// Entry point node (highest layer node). Internal usize ID.
    pub(crate) entry_point: Option<usize>,

    /// Current maximum layer in the graph.
    pub(crate) max_level: usize,

    /// Tombstoned (soft-deleted) internal node IDs.
    /// Dense bitset indexed by internal ID for O(1) lookup on the search hot path.
    pub(crate) tombstones: Vec<bool>,

    /// Count of tombstoned nodes (maintained separately to avoid scanning the Vec).
    pub(crate) tombstone_count: usize,

    /// Insertions since last rebuild (for tracking recall degradation).
    pub(crate) additions_since_rebuild: usize,

    /// Random number generator for level generation.
    /// If config.seed is Some, this is a seeded RNG for reproducibility.
    pub(crate) rng: StdRng,

    /// Optional metrics sink for observability.
    pub(crate) metrics: Option<Arc<dyn MetricsSink>>,

    /// INT8 quantized vector arena for fast approximate distance.
    /// Maintained in parallel with `nodes` -- same internal ID ordering.
    pub(crate) quantized: QuantizedArena,

    /// Whether to use INT8 quantized distance for candidate filtering.
    /// Default: false. Enable for large indexes (50K+ vectors) where
    /// distance computation dominates search time.
    pub(crate) use_quantized: bool,
}

impl Clone for HnswIndex {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            nodes: self.nodes.clone(),
            id_to_internal: self.id_to_internal.clone(),
            internal_to_id: self.internal_to_id.clone(),
            entry_point: self.entry_point,
            max_level: self.max_level,
            tombstones: self.tombstones.clone(),
            tombstone_count: self.tombstone_count,
            additions_since_rebuild: self.additions_since_rebuild,
            rng: self.rng.clone(),
            metrics: self.metrics.clone(),
            quantized: self.quantized.clone(),
            use_quantized: self.use_quantized,
        }
    }
}

impl std::fmt::Debug for HnswIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HnswIndex")
            .field("config", &self.config)
            .field("num_nodes", &self.nodes.len())
            .field("max_level", &self.max_level)
            .field("tombstones", &self.tombstone_count)
            .field("additions_since_rebuild", &self.additions_since_rebuild)
            .field("use_quantized", &self.use_quantized)
            .finish()
    }
}

impl HnswIndex {
    /// Create a new HNSW index with default configuration and specified dimensions.
    pub fn new(dimensions: usize) -> Self {
        Self::with_config(HnswConfig::with_dimensions(dimensions))
    }

    /// Create a new HNSW index with custom configuration. Panics if config is invalid; use `try_with_config` for external input.
    pub fn with_config(config: HnswConfig) -> Self {
        config.validate().expect("HNSW configuration must be valid");
        Self::build_from_config(config)
    }

    /// Create a new HNSW index with custom configuration, returning an error if invalid.
    /// Use for configs from external input (deserialization, user-provided values).
    pub fn try_with_config(config: HnswConfig) -> crate::error::Result<Self> {
        config.validate()?;
        Ok(Self::build_from_config(config))
    }

    /// Internal constructor shared by `with_config` and `try_with_config`.
    fn build_from_config(config: HnswConfig) -> Self {
        // Initialize RNG - seeded if config.seed is Some, otherwise from entropy
        let rng = match config.seed {
            Some(seed) => StdRng::seed_from_u64(seed),
            None => StdRng::from_entropy(),
        };

        let dims = config.dimensions;
        Self {
            config,
            nodes: Vec::new(),
            id_to_internal: HashMap::new(),
            internal_to_id: Vec::new(),
            entry_point: None,
            max_level: 0,
            tombstones: Vec::new(),
            tombstone_count: 0,
            additions_since_rebuild: 0,
            rng,
            metrics: None,
            quantized: QuantizedArena::new(dims),
            use_quantized: false,
        }
    }

    /// Get the current configuration.
    pub fn config(&self) -> &HnswConfig {
        &self.config
    }

    /// Attach a metrics sink (builder pattern).
    /// The sink receives events from `search`, `insert`, and `rebuild` operations.
    #[must_use]
    pub fn with_metrics(mut self, sink: Arc<dyn MetricsSink>) -> Self {
        self.metrics = Some(sink);
        self
    }

    /// Set or replace the metrics sink at runtime.
    ///
    /// Pass `Some(sink)` to enable metrics, or `None` to disable.
    pub fn set_metrics(&mut self, sink: Option<Arc<dyn MetricsSink>>) {
        self.metrics = sink;
    }

    /// Enable INT8 quantized distance for candidate filtering (builder pattern).
    /// Two-phase: INT8 screening (~3x faster) then f32 final ranking. Use `set_quantized` for runtime toggle.
    #[must_use]
    pub fn with_quantized(mut self) -> Self {
        self.use_quantized = true;
        self
    }

    /// Enable or disable INT8 quantized search at runtime; no rebuild cost since the arena is always maintained.
    pub fn set_quantized(&mut self, enabled: bool) {
        self.use_quantized = enabled;
    }

    /// Check if INT8 quantized search is enabled.
    pub fn is_quantized(&self) -> bool {
        self.use_quantized
    }

    /// Get the number of vectors in the index (including tombstones).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Get the number of live (non-tombstoned) vectors.
    pub fn len_live(&self) -> usize {
        self.nodes.len().saturating_sub(self.tombstone_count)
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Get the vector for an embedding ID, if present.
    pub fn get_vector(&self, id: &NodeId) -> Option<Vec<f32>> {
        self.id_to_internal
            .get(id)
            .map(|&iid| self.nodes[iid].vector.clone())
    }

    /// Get tombstone statistics.
    pub fn tombstone_stats(&self) -> TombstoneStats {
        let total = self.nodes.len();
        let tombstones = self.tombstone_count;
        let live = total.saturating_sub(tombstones);
        let ratio = if total > 0 {
            tombstones as f64 / total as f64
        } else {
            0.0
        };

        TombstoneStats {
            total_nodes: total,
            tombstone_count: tombstones,
            live_nodes: live,
            ratio,
        }
    }

    /// Check if a rebuild is recommended based on tombstone ratio.
    pub fn needs_rebuild(&self) -> bool {
        self.tombstone_stats()
            .needs_rebuild_at(self.config.rebuild_threshold)
    }

    /// Look up the internal ID for an NodeId, if it exists.
    #[inline]
    // REASON: `internal_id` is forward-deployed for the delta-merge path
    // (cross-index lookups for incremental HNSW merge). Not yet wired into a
    // caller; removing it would require re-adding it when the merge path lands.
    #[allow(dead_code)]
    pub(crate) fn internal_id(&self, id: &NodeId) -> Option<usize> {
        self.id_to_internal.get(id).copied()
    }

    /// Check if an internal ID is tombstoned. O(1) array lookup.
    #[inline]
    pub(crate) fn is_tombstoned(&self, iid: usize) -> bool {
        iid < self.tombstones.len() && self.tombstones[iid]
    }

    /// Look up the external NodeId for an internal ID.
    #[inline]
    pub(crate) fn external_id(&self, iid: usize) -> NodeId {
        self.internal_to_id[iid]
    }

    /// Create a serializable snapshot including full vector data for warm-start restores.
    /// Use `restore_from_snapshot_embedded` to restore without a separate vector map.
    pub fn snapshot(&self) -> super::super::checkpoint::HnswSnapshot {
        use super::super::checkpoint::{HnswCheckpointConfig, HnswSnapshot};

        let mut indexed_ids: Vec<_> = self.internal_to_id.clone();
        indexed_ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        let mut tombstoned_ids: Vec<_> = self
            .tombstones
            .iter()
            .enumerate()
            .filter(|(_, &is_tomb)| is_tomb)
            .map(|(iid, _)| self.external_id(iid))
            .collect();
        tombstoned_ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        // Build layer representation -- convert internal IDs to NodeId
        let mut layers = Vec::new();
        for level in 0..=self.max_level {
            let mut layer_nodes = Vec::new();
            for (iid, node) in self.nodes.iter().enumerate() {
                if level < node.neighbors.len() {
                    let neighbors: Vec<NodeId> = node.neighbors[level]
                        .iter()
                        .map(|&nid| self.external_id(nid))
                        .collect();
                    layer_nodes.push((self.external_id(iid), neighbors));
                }
            }
            // Sort for deterministic ordering
            layer_nodes.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
            layers.push(layer_nodes);
        }

        // Convert entry_point from internal to external
        let entry_point_ext = self.entry_point.map(|iid| self.external_id(iid));

        // Embed full f32 vector data for self-contained warm-start snapshots.
        // Sorted by NodeId bytes to match indexed_ids ordering.
        let mut vectors: Vec<(NodeId, Vec<f32>)> = self
            .internal_to_id
            .iter()
            .zip(self.nodes.iter())
            .map(|(&id, node)| (id, node.vector.clone()))
            .collect();
        vectors.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));

        HnswSnapshot {
            vector_count: 0, // Legacy field, not used
            total_nodes: self.nodes.len(),
            live_nodes: self.len_live(),
            tombstone_count: self.tombstone_count,
            max_layer: self.max_level,
            entry_point: entry_point_ext,
            config: HnswCheckpointConfig::from_hnsw_config(&self.config),
            indexed_ids,
            tombstoned_ids,
            layers,
            vectors,
        }
    }

    /// Restore from a self-contained snapshot (embedded vectors). Errors on missing vectors or incompatible config.
    pub fn restore_from_snapshot_embedded(
        &mut self,
        snapshot: &super::super::checkpoint::HnswSnapshot,
    ) -> Result<(), crate::error::RetrievalError> {
        use crate::error::RetrievalError;

        if snapshot.vectors.is_empty() && !snapshot.indexed_ids.is_empty() {
            return Err(RetrievalError::hnsw(
                "Snapshot contains no embedded vectors; use restore_from_snapshot with an external vector map",
            ));
        }

        let vectors: std::collections::HashMap<NodeId, Vec<f32>> =
            snapshot.vectors.iter().cloned().collect();
        self.restore_from_snapshot(snapshot, &vectors)
    }

    /// Rebuild neighbor connections from a snapshot. Caller-supplied `vectors` override embedded ones. Errors on incompatible config, missing vectors, or bad dims.
    pub fn restore_from_snapshot(
        &mut self,
        snapshot: &super::super::checkpoint::HnswSnapshot,
        vectors: &std::collections::HashMap<NodeId, Vec<f32>>,
    ) -> Result<(), crate::error::RetrievalError> {
        use super::super::checkpoint::HnswCheckpointConfig;
        use crate::config::MAX_LEVEL;
        use crate::error::RetrievalError;

        // Verify snapshot integrity (counts, ID consistency)
        snapshot
            .verify()
            .map_err(|e| RetrievalError::hnsw(format!("Invalid snapshot: {e}")))?;

        // Check config compatibility
        let current_config = HnswCheckpointConfig::from_hnsw_config(&self.config);
        if !snapshot.is_compatible(&current_config) {
            return Err(RetrievalError::hnsw(format!(
                "Snapshot config incompatible: expected {:?}, got {:?}",
                current_config, snapshot.config
            )));
        }

        // Validate layer count before any mutation
        if snapshot.max_layer > MAX_LEVEL {
            return Err(RetrievalError::hnsw(format!(
                "Snapshot max_layer {} exceeds MAX_LEVEL {}",
                snapshot.max_layer, MAX_LEVEL
            )));
        }

        // Validate entry point membership before any mutation
        if let Some(ep) = snapshot.entry_point {
            if !snapshot.indexed_ids.contains(&ep) {
                return Err(RetrievalError::hnsw(format!(
                    "Snapshot entry_point {ep:?} is not in indexed_ids"
                )));
            }
        }

        // Build a merged vector lookup: snapshot-embedded vectors are the base,
        // caller-supplied entries take precedence (to allow incremental updates).
        //
        // Priority: caller-supplied > snapshot-embedded.
        // Both sources are collected into a single owned map to avoid lifetime
        // complications with mixed borrows.
        let mut merged_vectors: HashMap<NodeId, Vec<f32>> = snapshot
            .vectors
            .iter()
            .map(|(id, v)| (*id, v.clone()))
            .collect();
        // Caller-supplied entries override embedded ones
        for (id, v) in vectors {
            merged_vectors.insert(*id, v.clone());
        }

        // Validate ALL vectors exist, have correct dimensions, and are finite BEFORE clearing.
        // This ensures the current index is not corrupted on error.
        let dims = self.config.dimensions;
        for id in &snapshot.indexed_ids {
            let vec = merged_vectors
                .get(id)
                .ok_or_else(|| RetrievalError::hnsw(format!("Missing vector for ID {id:?}")))?;
            if vec.len() != dims {
                return Err(RetrievalError::DimensionMismatch {
                    expected: dims,
                    actual: vec.len(),
                });
            }
            for (vi, &v) in vec.iter().enumerate() {
                if !v.is_finite() {
                    return Err(RetrievalError::hnsw(format!(
                        "Non-finite value in vector for node {id:?} at index {vi}: {v}"
                    )));
                }
            }
        }

        // All validation passed — now safe to clear current state.
        self.nodes.clear();
        self.id_to_internal.clear();
        self.internal_to_id.clear();
        self.tombstones.clear();
        self.tombstone_count = 0;
        self.quantized.clear();

        // First pass: assign internal IDs and build mapping
        // We need a consistent ordering, so use indexed_ids order
        let mut ext_to_internal: HashMap<NodeId, usize> = HashMap::new();
        for (idx, id) in snapshot.indexed_ids.iter().enumerate() {
            ext_to_internal.insert(*id, idx);
        }

        // Build nodes with vectors (all validated above)
        for id in &snapshot.indexed_ids {
            // SAFETY: validated above — unwrap is safe.
            let vector = merged_vectors[id].clone();
            let level = self.calculate_level_for_restore(&snapshot.layers, id);
            let node = HnswNode::new(vector, level);
            let iid = self.nodes.len();
            self.quantized.push(&node.vector, node.norm);
            self.nodes.push(node);
            self.id_to_internal.insert(*id, iid);
            self.internal_to_id.push(*id);
        }

        // Set max_level and entry_point
        self.max_level = snapshot.max_layer;
        self.entry_point = snapshot
            .entry_point
            .and_then(|eid| ext_to_internal.get(&eid).copied());

        // Restore neighbor connections from layers -- convert NodeId to internal usize.
        // Unknown neighbor IDs (not in indexed_ids) are silently dropped to handle
        // snapshots produced by indexes with concurrent deletes.
        for (level, layer) in snapshot.layers.iter().enumerate() {
            for (node_id, neighbors) in layer {
                if let Some(&iid) = ext_to_internal.get(node_id) {
                    if level < self.nodes[iid].neighbors.len() {
                        self.nodes[iid].neighbors[level] = neighbors
                            .iter()
                            .filter_map(|nid| ext_to_internal.get(nid).copied())
                            .collect();
                    }
                }
            }
        }

        // Restore tombstones -- convert NodeId to internal usize
        // Resize tombstones bitset to match node count
        self.tombstones.resize(self.nodes.len(), false);
        for id in &snapshot.tombstoned_ids {
            if let Some(&iid) = ext_to_internal.get(id) {
                if !self.tombstones[iid] {
                    self.tombstones[iid] = true;
                    self.tombstone_count += 1;
                }
            }
        }

        Ok(())
    }

    /// Calculate the level for a node during restore based on snapshot layers.
    fn calculate_level_for_restore(
        &self,
        layers: &[Vec<(NodeId, Vec<NodeId>)>],
        id: &NodeId,
    ) -> usize {
        // Find the highest layer where this node appears
        let mut level = 0;
        for (l, layer) in layers.iter().enumerate() {
            if layer.iter().any(|(node_id, _)| node_id == id) {
                level = l;
            }
        }
        level
    }
}
