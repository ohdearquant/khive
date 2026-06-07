//! HNSW index statistics types.

use super::config::DEFAULT_REBUILD_THRESHOLD;

/// Statistics about tombstoned nodes in an HNSW index.
#[derive(Debug, Clone, Copy)]
pub struct TombstoneStats {
    /// Total number of nodes in the graph.
    pub total_nodes: usize,
    /// Number of tombstoned nodes.
    pub tombstone_count: usize,
    /// Number of live (non-tombstoned) nodes.
    pub live_nodes: usize,
    /// Ratio of tombstoned to total nodes (0.0 - 1.0).
    pub ratio: f64,
}

impl TombstoneStats {
    /// Check if rebuild is needed based on default threshold.
    pub fn needs_rebuild(&self) -> bool {
        self.needs_rebuild_at(DEFAULT_REBUILD_THRESHOLD)
    }

    /// Check if rebuild is needed at a specific threshold.
    pub fn needs_rebuild_at(&self, threshold: f64) -> bool {
        self.ratio > threshold
    }
}

/// Statistics returned from a rebuild operation.
#[derive(Debug, Clone, Copy)]
pub struct RebuildStats {
    /// Number of nodes before rebuild.
    pub nodes_before: usize,
    /// Number of nodes removed (tombstones).
    pub nodes_removed: usize,
    /// Number of nodes after rebuild.
    pub nodes_after: usize,
    /// Number of neighbor references cleaned up.
    pub edges_cleaned: usize,
    /// Whether entry point was updated.
    pub entry_point_updated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tombstone_stats_needs_rebuild() {
        let stats = TombstoneStats {
            total_nodes: 100,
            tombstone_count: 10,
            live_nodes: 90,
            ratio: 0.10,
        };
        assert!(!stats.needs_rebuild()); // 10% == 10% threshold (not strictly greater)

        let stats = TombstoneStats {
            total_nodes: 100,
            tombstone_count: 20,
            live_nodes: 80,
            ratio: 0.20,
        };
        assert!(stats.needs_rebuild()); // 20% > 10% threshold
    }

    #[test]
    fn test_tombstone_stats_custom_threshold() {
        let stats = TombstoneStats {
            total_nodes: 100,
            tombstone_count: 10,
            live_nodes: 90,
            ratio: 0.10,
        };
        assert!(stats.needs_rebuild_at(0.05)); // 10% > 5%
        assert!(!stats.needs_rebuild_at(0.15)); // 10% < 15%
    }
}
