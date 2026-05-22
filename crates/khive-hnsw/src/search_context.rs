//! Pre-allocated search buffers for HNSW search.
//!
//! Avoids per-query heap allocation of `BinaryHeap`, `HashSet`, and result vectors.
//! Create one `HnswSearchContext` and reuse it across multiple `search_with_context` calls
//! for maximum throughput.
//!
//! # Performance
//!
//! The key optimizations are:
//! 1. **Buffer reuse**: All data structures are cleared between searches but their
//!    allocated memory persists, eliminating allocator pressure.
//! 2. **Generation-counter visited set**: Uses a dense `Vec<u64>` indexed directly by
//!    internal node ID. `clear()` is O(1) (just increment generation counter).
//!    `visit()` and `is_visited()` are O(1) array lookups with no hashing.

use std::collections::BinaryHeap;

use crate::distance::OrderedF32;

/// O(1) visited set using generation counter and dense array.
///
/// Each node slot stores the generation number when it was last visited.
/// To "clear" the set, we just increment the generation counter -- O(1).
/// A node is visited iff `markers[id] == generation`.
///
/// This replaces `HashSet<EmbeddingId>` which required O(capacity) clear
/// and O(1) amortized insert with hash computation overhead per operation.
pub(crate) struct VisitedSet {
    /// Current generation number. Incremented on each `clear()`.
    generation: u64,
    /// Dense array indexed by internal node ID.
    /// `markers[id] == generation` means node `id` has been visited.
    markers: Vec<u64>,
}

impl VisitedSet {
    /// Create a new visited set with the given capacity hint.
    pub fn new(capacity: usize) -> Self {
        Self {
            generation: 1, // Start at 1 so default 0 values are "not visited"
            markers: vec![0u64; capacity],
        }
    }

    /// Clear the visited set in O(1) by incrementing the generation counter.
    ///
    /// On the extremely rare wrap-around (every 2^64 clears), we zero the
    /// markers array to prevent false positives.
    #[inline]
    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            // Wrapped around -- reset markers to avoid false positives
            self.markers.fill(0);
            self.generation = 1;
        }
    }

    /// Ensure the set can accommodate node IDs up to `max_id` (inclusive).
    #[inline]
    pub fn ensure_capacity(&mut self, max_id: usize) {
        if max_id >= self.markers.len() {
            self.markers.resize(max_id + 1, 0);
        }
    }

    /// Mark a node as visited. Returns `true` if the node was NOT previously visited
    /// (i.e., this is the first visit), matching `HashSet::insert` semantics.
    #[inline]
    pub fn visit(&mut self, id: usize) -> bool {
        if id >= self.markers.len() {
            self.markers.resize(id + 1, 0);
        }
        if self.markers[id] == self.generation {
            false // already visited
        } else {
            self.markers[id] = self.generation;
            true // newly visited
        }
    }

    /// Mark multiple nodes as visited.
    #[inline]
    pub fn visit_all(&mut self, ids: impl Iterator<Item = usize>) {
        for id in ids {
            self.visit(id);
        }
    }
}

/// Pre-allocated search context for HNSW queries.
///
/// Reuse across multiple `search_with_context` calls to amortize allocation cost.
/// The context holds the working buffers for the greedy beam search:
///
/// - `candidates`: min-heap of nodes to explore (closest first) -- uses internal usize IDs
/// - `results`: max-heap of best results so far (furthest first, for pruning) -- uses internal usize IDs
/// - `visited`: generation-counter visited set indexed by internal usize ID
/// - `result_buf`: scratch buffer for final sorted output (internal usize IDs)
///
/// # Example
///
/// ```rust,ignore
/// use khive_retrieval::hnsw::{HnswIndex, HnswSearchContext};
///
/// let index = HnswIndex::new(128);
/// // ... insert vectors ...
///
/// let mut ctx = HnswSearchContext::new(index.config().ef_search);
///
/// // Reuse ctx across many searches
/// for query in queries {
///     let results = index.search_with_context(&query, 10, &mut ctx)?;
///     // process results...
/// }
/// ```
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
    /// Create a new search context pre-allocated for the given `ef` value.
    ///
    /// The `ef` parameter should match or exceed the `ef_search` config value
    /// of the index you plan to search.
    pub fn new(ef: usize) -> Self {
        Self {
            candidates: BinaryHeap::with_capacity(ef),
            results: BinaryHeap::with_capacity(ef),
            visited: VisitedSet::new(ef * 4), // Over-allocate to reduce resizes
            result_buf: Vec::with_capacity(ef),
            ef_hint: ef,
        }
    }

    /// Clear all buffers without deallocating.
    ///
    /// Called automatically at the start of each search. You do not need to
    /// call this manually.
    pub(crate) fn clear(&mut self) {
        self.candidates.clear();
        self.results.clear();
        self.visited.clear(); // O(1) generation increment
        self.result_buf.clear();
    }

    /// Ensure all buffers are large enough for the given `ef` and node count.
    pub(crate) fn ensure_capacity(&mut self, ef: usize, num_nodes: usize) {
        if ef > self.ef_hint {
            self.result_buf
                .reserve(ef.saturating_sub(self.result_buf.capacity()));
            self.ef_hint = ef;
        }
        self.visited.ensure_capacity(num_nodes);
    }
}
