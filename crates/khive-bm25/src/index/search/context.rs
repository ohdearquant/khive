//! Per-query scratch buffers for BM25 search.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Sentinel doc_id for a terminated (exhausted) cursor.
pub(crate) const TERMINATED_DOC: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapEntry {
    pub(crate) doc_id: u32,
    pub(crate) score: f64,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.doc_id == other.doc_id && self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.doc_id.cmp(&self.doc_id))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShallowBlockInfo {
    pub(crate) max_score: f64,
    pub(crate) last_doc: u32,
}

/// Reusable per-query scratch space; cleared automatically at the start of each search.
pub struct SearchContext {
    pub(crate) score_vec: Vec<f64>,
    pub(crate) touched_docs: Vec<u32>,
    pub(crate) results_buf: Vec<(u32, f64)>,
    pub(crate) heap: BinaryHeap<std::cmp::Reverse<HeapEntry>>,
}

impl SearchContext {
    /// Create a new, empty search context.
    pub fn new() -> Self {
        Self {
            score_vec: Vec::new(),
            touched_docs: Vec::new(),
            results_buf: Vec::new(),
            heap: BinaryHeap::new(),
        }
    }

    /// Create a search context pre-allocated for an expected number of matches.
    pub fn with_capacity(estimated_matches: usize) -> Self {
        Self {
            score_vec: Vec::new(),
            touched_docs: Vec::with_capacity(estimated_matches),
            results_buf: Vec::with_capacity(estimated_matches),
            heap: BinaryHeap::with_capacity(estimated_matches.min(64)),
        }
    }

    /// Clear all per-query state without releasing heap memory.
    pub fn clear(&mut self) {
        for &doc_id in &self.touched_docs {
            if (doc_id as usize) < self.score_vec.len() {
                self.score_vec[doc_id as usize] = 0.0;
            }
        }
        self.touched_docs.clear();
        self.results_buf.clear();
        self.heap.clear();
    }
}

impl Default for SearchContext {
    fn default() -> Self {
        Self::new()
    }
}
