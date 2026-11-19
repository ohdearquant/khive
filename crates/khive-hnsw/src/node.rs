//! Internal node storage for the HNSW graph — dense `Vec<HnswNode>` with per-layer neighbor lists.

/// Internal node in the HNSW graph.
///
/// Nodes are stored in a dense `Vec<HnswNode>` indexed by an internal `usize` ID.
/// The `EmbeddingId` <-> `usize` mapping is maintained by `HnswIndex`.
/// Neighbor lists use internal `usize` IDs for O(1) array lookups during search.
#[derive(Debug, Clone)]
pub(crate) struct HnswNode {
    /// The vector data.
    pub vector: Vec<f32>,
    /// Connections per layer: layer -> list of internal neighbor IDs.
    pub neighbors: Vec<Vec<usize>>,
    /// Maximum layer this node exists in.
    pub max_layer: usize,
    /// Cached L2 norm for cosine similarity optimization.
    pub norm: f32,
}

impl HnswNode {
    /// Create a new node with computed norm.
    pub fn new(vector: Vec<f32>, max_layer: usize) -> Self {
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        Self {
            vector,
            neighbors: vec![Vec::new(); max_layer + 1],
            max_layer,
            norm,
        }
    }

    /// Update vector and recompute norm.
    pub fn update_vector(&mut self, vector: Vec<f32>) {
        self.norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        self.vector = vector;
    }
}

#[cfg(test)]
#[path = "node/tests.rs"]
mod tests;
