//! Rebuild, delete, and clear operations for HNSW index.

use crate::NodeId;

use super::HnswIndex;
use crate::metrics::{self, MetricEvent, MetricValue};
use crate::stats::RebuildStats;

impl HnswIndex {
    /// Mark a vector for deletion (lazy tombstone); returns true if it existed.
    /// If the deleted node was the entry point, a replacement is found from neighbors O(M).
    pub fn delete(&mut self, id: NodeId) -> bool {
        match self.id_to_internal.get(&id) {
            Some(&iid) => {
                // Grow tombstone bitset if needed
                if iid >= self.tombstones.len() {
                    self.tombstones.resize(iid + 1, false);
                }
                let was_new = !self.tombstones[iid];
                if was_new {
                    self.tombstones[iid] = true;
                    self.tombstone_count += 1;
                    self.repair_entry_point_after_delete(iid);
                }
                was_new
            }
            None => false,
        }
    }

    /// Repair the entry point after a delete; O(M) typical, O(N) fallback only if all neighbors are dead.
    fn repair_entry_point_after_delete(&mut self, tombstoned_id: usize) {
        let current_ep = match self.entry_point {
            Some(ep) if ep == tombstoned_id => ep,
            _ => return, // Not the entry point, nothing to do
        };

        // Search the tombstoned node's neighbors across all layers (highest first)
        // for a live replacement. Prefer higher-layer neighbors since they provide
        // better graph coverage for search entry.
        let node = &self.nodes[current_ep];
        for layer in (0..node.neighbors.len()).rev() {
            for &neighbor_id in &node.neighbors[layer] {
                if !self.is_tombstoned(neighbor_id) {
                    self.entry_point = Some(neighbor_id);
                    return;
                }
            }
        }

        // Extremely rare fallback: all neighbors are tombstoned too.
        // Scan for ANY live node. This is O(N) but should essentially never happen
        // in practice -- it requires tombstoning an entire neighborhood.
        for iid in 0..self.nodes.len() {
            if !self.is_tombstoned(iid) {
                self.entry_point = Some(iid);
                return;
            }
        }

        // All nodes are tombstoned
        self.entry_point = None;
    }

    /// Physically remove tombstoned nodes and clean up neighbor references.
    /// Emits rebuild metrics when a sink is attached.
    pub fn rebuild(&mut self) -> RebuildStats {
        let start = std::time::Instant::now();

        let stats = self.rebuild_inner();

        // Emit metrics
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_REBUILD_DURATION_MS,
                value: MetricValue::Histogram(elapsed),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_REBUILD_COUNT,
                value: MetricValue::Counter(1),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_REBUILD_NODES_REMOVED,
                value: MetricValue::Gauge(stats.nodes_removed as f64),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_INDEX_SIZE,
                value: MetricValue::Gauge(self.len_live() as f64),
                labels: vec![],
            },
        );

        stats
    }

    /// Inner rebuild logic (uninstrumented); compacts storage and re-assigns internal IDs.
    fn rebuild_inner(&mut self) -> RebuildStats {
        let nodes_before = self.nodes.len();
        let nodes_removed = self.tombstone_count;

        // Track if entry point needs update
        let entry_point_was_tombstone = self
            .entry_point
            .map(|ep| self.is_tombstoned(ep))
            .unwrap_or(false);

        // Build old_to_new mapping: compact non-tombstoned nodes
        let mut old_to_new: Vec<Option<usize>> = vec![None; self.nodes.len()];
        let mut new_nodes: Vec<super::super::node::HnswNode> =
            Vec::with_capacity(self.nodes.len() - nodes_removed);
        let mut new_internal_to_id: Vec<NodeId> =
            Vec::with_capacity(self.nodes.len() - nodes_removed);
        let mut new_id_to_internal =
            std::collections::HashMap::with_capacity(self.nodes.len() - nodes_removed);

        let mut new_idx = 0usize;
        for (old_idx, mapping) in old_to_new.iter_mut().enumerate() {
            if self.is_tombstoned(old_idx) {
                // Remove from external mapping
                let ext_id = self.internal_to_id[old_idx];
                self.id_to_internal.remove(&ext_id);
                continue;
            }
            *mapping = Some(new_idx);
            let ext_id = self.internal_to_id[old_idx];
            new_id_to_internal.insert(ext_id, new_idx);
            new_internal_to_id.push(ext_id);
            new_idx += 1;
        }

        // Clone nodes and remap neighbor IDs
        let mut edges_cleaned = 0usize;
        for old_idx in 0..self.nodes.len() {
            if self.is_tombstoned(old_idx) {
                continue;
            }
            let mut node = self.nodes[old_idx].clone();
            for neighbors in &mut node.neighbors {
                let before = neighbors.len();
                // Remap internal IDs and remove references to tombstoned nodes
                *neighbors = neighbors
                    .iter()
                    .filter_map(|&old_nid| old_to_new[old_nid])
                    .collect();
                edges_cleaned += before - neighbors.len();
            }
            new_nodes.push(node);
        }

        // Update entry point
        let entry_point_updated = if entry_point_was_tombstone || self.entry_point.is_none() {
            // Find new entry point among surviving nodes
            let new_ep = new_nodes
                .iter()
                .enumerate()
                .max_by_key(|(_, n)| n.max_layer)
                .map(|(idx, _)| idx);
            self.entry_point = new_ep;
            entry_point_was_tombstone
        } else {
            // Remap existing entry point
            self.entry_point = self.entry_point.and_then(|old_ep| old_to_new[old_ep]);
            false
        };

        // Update max_level
        self.max_level = new_nodes.iter().map(|n| n.max_layer).max().unwrap_or(0);

        // Swap in compacted state
        self.nodes = new_nodes;
        self.id_to_internal = new_id_to_internal;
        self.internal_to_id = new_internal_to_id;
        self.tombstones.clear();
        self.tombstone_count = 0;
        self.additions_since_rebuild = 0;

        // Rebuild quantized arena from compacted nodes
        self.quantized.clear();
        for node in &self.nodes {
            self.quantized.push(&node.vector, node.norm);
        }

        RebuildStats {
            nodes_before,
            nodes_removed,
            nodes_after: self.nodes.len(),
            edges_cleaned,
            entry_point_updated,
        }
    }

    /// Clear all data from the index.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.id_to_internal.clear();
        self.internal_to_id.clear();
        self.tombstones.clear();
        self.tombstone_count = 0;
        self.quantized.clear();
        self.entry_point = None;
        self.max_level = 0;
        self.additions_since_rebuild = 0;
    }

    /// Update entry point to the node with the highest max_layer.
    // REASON: Called by rebuild in manual mode; not dead in all compile paths.
    // Kept here to avoid split of rebuild state management across modules.
    #[allow(dead_code)]
    pub(super) fn update_entry_point(&mut self) {
        let new_entry = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(idx, _)| !self.is_tombstoned(*idx))
            .max_by_key(|(_, n)| n.max_layer)
            .map(|(idx, _)| idx);

        self.entry_point = new_entry;
        self.max_level = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(idx, _)| !self.is_tombstoned(*idx))
            .map(|(_, n)| n.max_layer)
            .max()
            .unwrap_or(0);
    }
}
