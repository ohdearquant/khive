//! Pre-allocated search buffers for HNSW search.

use std::collections::BinaryHeap;

use crate::distance::OrderedF32;

use khive_types::vector::VisitedSet;

/// Pre-allocated search context; reuse across calls to amortize allocation cost.
pub struct HnswSearchContext {
    /// Min-heap: candidates to explore (closest first). Uses internal usize IDs.
    pub(crate) candidates: BinaryHeap<std::cmp::Reverse<(OrderedF32, usize)>>,
    /// Max-heap: best results so far (furthest first, for pruning). Uses internal usize IDs.
    pub(crate) results: BinaryHeap<(OrderedF32, usize)>,
    /// Visited node tracking with O(1) operations.
    pub(crate) visited: VisitedSet,
    /// Scratch buffer for final sorted results (internal usize IDs).
    pub(crate) result_buf: Vec<(f32, usize)>,
    /// Pre-allocated capacity hint (ef value used to size buffers).
    ef_hint: usize,
}

impl HnswSearchContext {
    /// Create a pre-allocated context sized for the given `ef` value.
    pub fn new(ef: usize) -> Self {
        Self {
            candidates: BinaryHeap::with_capacity(ef),
            results: BinaryHeap::with_capacity(ef),
            visited: VisitedSet::new(ef * 4), // Over-allocate to reduce resizes
            result_buf: Vec::with_capacity(ef),
            ef_hint: ef,
        }
    }

    /// Clear all buffers without deallocating; called automatically at search start.
    pub(crate) fn clear(&mut self) {
        self.candidates.clear();
        self.results.clear();
        self.visited.clear(); // O(1) generation increment
        self.result_buf.clear();
    }

    /// Ensure all buffers are large enough for the given `ef` and node count.
    pub(crate) fn ensure_capacity(&mut self, ef: usize, num_nodes: usize) {
        if ef > self.ef_hint {
            // Reserve capacity for result_buf, candidates, and results heaps.
            // Without this, only result_buf was pre-reserved and the heaps would
            // still reallocate during search under larger ef values.
            self.result_buf
                .reserve(ef.saturating_sub(self.result_buf.capacity()));
            // BinaryHeap::reserve(additional) — reserve at least `additional` more capacity.
            let cand_add = ef.saturating_sub(self.candidates.capacity());
            if cand_add > 0 {
                self.candidates.reserve(cand_add);
            }
            let res_add = ef.saturating_sub(self.results.capacity());
            if res_add > 0 {
                self.results.reserve(res_add);
            }
            self.ef_hint = ef;
        }
        self.visited.ensure_capacity(num_nodes);
    }
}
