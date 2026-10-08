//! Vector similarity and search primitives.

use alloc::{vec, vec::Vec};

mod codec;
pub use codec::{
    decode_f32_le, decode_f32_native, encode_f32_le, encode_f32_native, VectorCodecError,
};

/// Distance metric for vector similarity search.
///
/// # Variants
/// - `Cosine`: `1 - cosine_similarity`. Value in [0, 2] for unit vectors.
/// - `Dot`: dot product (negated for min-heap; higher dot = lower distance).
/// - `L2`: Euclidean (L2) distance.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DistanceMetric {
    Cosine,
    Dot,
    L2,
}

impl Default for DistanceMetric {
    #[inline]
    fn default() -> Self {
        Self::Cosine
    }
}

/// Generation-based visited-node tracker for greedy search.
///
/// Avoids clearing a `Vec<bool>` on every query by incrementing a generation counter.
#[derive(Debug, Clone)]
pub struct VisitedSet {
    marks: Vec<u64>,
    generation: u64,
}

impl VisitedSet {
    /// Create a new `VisitedSet` with pre-allocated capacity for `capacity` nodes.
    pub fn new(capacity: usize) -> Self {
        Self {
            marks: vec![0; capacity],
            generation: 1,
        }
    }

    /// Reset the visited state for all nodes in O(1) by advancing the generation.
    #[inline]
    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.marks.fill(0);
            self.generation = 1;
        }
    }

    /// Grow the internal buffer if `node` would be out of range.
    ///
    /// Resizing uses `node + 1`; the maximum ID must permit that addition and allocation.
    #[inline]
    pub fn ensure_capacity(&mut self, node: usize) {
        if node >= self.marks.len() {
            self.marks.resize(node + 1, 0);
        }
    }

    /// Mark `node` as visited if it has not been visited in this generation.
    ///
    /// Returns `true` on first visit, `false` on subsequent calls for the same node.
    /// Resizing uses `node + 1`, with the same capacity requirements as `ensure_capacity`.
    #[inline]
    pub fn mark_if_new(&mut self, node: usize) -> bool {
        if node >= self.marks.len() {
            self.marks.resize(node + 1, 0);
        }
        if self.marks[node] == self.generation {
            false
        } else {
            self.marks[node] = self.generation;
            true
        }
    }

    /// Mark a node as visited; returns `true` on its first visit in this generation.
    #[inline]
    pub fn visit(&mut self, node: usize) -> bool {
        self.mark_if_new(node)
    }

    /// Mark multiple nodes as visited.
    #[inline]
    pub fn visit_all(&mut self, nodes: impl Iterator<Item = usize>) {
        for node in nodes {
            self.visit(node);
        }
    }

    /// Return `true` if `node` has been marked in the current generation.
    #[inline]
    pub fn is_marked(&self, node: usize) -> bool {
        node < self.marks.len() && self.marks[node] == self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visited_set_wraparound_resets_marks() {
        let mut vs = VisitedSet {
            marks: vec![0; 4],
            generation: u64::MAX,
        };
        vs.mark_if_new(0);
        vs.clear();
        assert!(vs.mark_if_new(0));
        assert!(!vs.mark_if_new(0));
    }

    #[test]
    fn default_is_cosine() {
        assert_eq!(DistanceMetric::default(), DistanceMetric::Cosine);
    }
}
