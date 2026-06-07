//! Memory budget operations for HNSW index.

use super::HnswIndex;

impl HnswIndex {
    /// Get the configured memory budget, if any.
    pub fn memory_budget(&self) -> Option<usize> {
        self.config.memory_budget
    }

    /// Set or clear the memory budget at runtime.
    pub fn set_memory_budget(&mut self, budget: Option<usize>) {
        self.config.memory_budget = budget;
    }

    /// Estimate the current memory usage of the index in bytes.
    /// Conservative estimate; actual usage may differ due to allocator overhead and alignment.
    pub fn memory_usage(&self) -> usize {
        let num_nodes = self.nodes.len();
        let dims = self.config.dimensions;

        // Per-node: vector (dims * 4 bytes for f32) + fixed fields
        // HnswNode has: vector(Vec overhead 24 + data) + neighbors(Vec overhead 24)
        //             + max_layer(8) + norm(4)
        let node_overhead: usize = 24 + 24 + 8 + 4; // 60 bytes fixed per node
        let per_node = dims * 4 + node_overhead;
        let nodes_total = num_nodes * per_node;

        // Neighbor entries: each Vec<usize> per layer, each entry is 8 bytes (usize)
        // Plus Vec overhead (24 bytes) per layer per node
        let mut neighbor_entries: usize = 0;
        let mut layer_vecs: usize = 0;
        for node in &self.nodes {
            layer_vecs += node.neighbors.len() * 24; // Vec overhead per layer
            for layer in &node.neighbors {
                neighbor_entries += layer.len();
            }
        }
        let neighbors_total = neighbor_entries * 8 + layer_vecs;

        // ID mapping overhead:
        // HashMap<EmbeddingId, usize>: ~(num_nodes * 40) for bucket/metadata
        // Vec<EmbeddingId>: num_nodes * 16
        let mapping_overhead = num_nodes * 40 + num_nodes * 16;

        // Tombstone bitset overhead: 1 byte per node (Vec<bool>)
        let tombstone_overhead = self.tombstones.len();

        // Quantized arena overhead:
        // - data: num_nodes * dims * 1 byte (i8)
        // - meta: num_nodes * 8 bytes (QuantMeta: scale f32 + norm f32)
        let quantized_overhead = num_nodes * dims + num_nodes * 8;

        nodes_total + neighbors_total + mapping_overhead + tombstone_overhead + quantized_overhead
    }

    /// Estimate the incremental memory cost of inserting one new vector.
    pub fn estimate_insert_cost(&self) -> usize {
        let dims = self.config.dimensions;

        // Vector data + node fixed overhead
        let node_overhead: usize = 24 + 24 + 8 + 4;
        let per_node = dims * 4 + node_overhead;

        // Expected neighbors: at least 1 layer with m_max0 neighbors,
        // plus Vec overhead. Use m_max0 as conservative estimate for layer 0.
        // Neighbors are usize (8 bytes each)
        let expected_neighbors = self.config.m_max0 * 8 + 24;

        // ID mapping entry overhead (HashMap entry + Vec entry)
        let mapping_entry = 40 + 16;

        // Quantized arena: dims * 1 byte (i8) + 8 bytes (QuantMeta)
        let quantized_cost = dims + 8;

        per_node + expected_neighbors + mapping_entry + quantized_cost
    }
}
