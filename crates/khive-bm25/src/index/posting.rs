//! Posting list and block-max metadata types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// SoA posting list: parallel doc_ids and term_freqs arrays, sorted by doc_id.
#[derive(Debug, Clone, Default, Serialize)]
#[doc(hidden)]
pub struct PostingList {
    /// Document IDs, sorted ascending for binary-search seeks in WAND.
    pub doc_ids: Vec<u32>,
    /// Term frequencies, parallel to `doc_ids`. Clamped to u8::MAX (255).
    pub(crate) term_freqs: Vec<u8>,
}

impl<'de> serde::Deserialize<'de> for PostingList {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as DeError;

        // Deserialize into raw struct first.
        #[derive(serde::Deserialize)]
        struct Raw {
            doc_ids: Vec<u32>,
            term_freqs: Vec<u8>,
        }

        let raw = Raw::deserialize(deserializer)?;

        // Invariant: lengths must match.
        if raw.doc_ids.len() != raw.term_freqs.len() {
            return Err(D::Error::custom(format!(
                "PostingList invariant violated: doc_ids.len()={} != term_freqs.len()={}",
                raw.doc_ids.len(),
                raw.term_freqs.len()
            )));
        }

        // Invariant: doc_ids must be sorted in strictly ascending order.
        if raw.doc_ids.windows(2).any(|w| w[0] >= w[1]) {
            return Err(D::Error::custom(
                "PostingList invariant violated: doc_ids must be strictly sorted ascending",
            ));
        }

        // Invariant: no sentinel doc IDs (u32::MAX is used as TERMINATED_DOC).
        if raw.doc_ids.contains(&u32::MAX) {
            return Err(D::Error::custom(
                "PostingList invariant violated: doc_id u32::MAX is reserved as a sentinel",
            ));
        }

        Ok(PostingList {
            doc_ids: raw.doc_ids,
            term_freqs: raw.term_freqs,
        })
    }
}

impl PostingList {
    /// Number of postings in this list.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.doc_ids.len()
    }

    /// Whether the posting list is empty.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.doc_ids.is_empty()
    }

    /// Insert a posting at the given position, maintaining sorted order.
    #[inline]
    pub(crate) fn insert(&mut self, index: usize, doc_id: u32, term_freq: u8) {
        self.doc_ids.insert(index, doc_id);
        self.term_freqs.insert(index, term_freq);
    }

    /// Remove the posting at the given position.
    #[inline]
    pub(crate) fn remove(&mut self, index: usize) {
        self.doc_ids.remove(index);
        self.term_freqs.remove(index);
    }

    /// Find the insertion point for a doc_id (binary search).
    #[inline]
    pub(crate) fn partition_point_by_doc_id(&self, target: u32) -> usize {
        self.doc_ids.partition_point(|&id| id < target)
    }

    /// Memory usage in bytes (actual heap allocation, no padding waste).
    #[inline]
    #[allow(dead_code)] // REASON: reserved for memory diagnostics endpoint
    pub(crate) fn heap_bytes(&self) -> usize {
        // Vec<u32> capacity * 4 + Vec<u8> capacity * 1
        // Use len() as approximation (capacity >= len)
        self.doc_ids.len() * 4 + self.term_freqs.len()
    }
}

/// Per-block BM25 upper-bound metadata for a posting list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BlockMaxBlock {
    /// Smallest document id in the block.
    pub(crate) min_doc_id: u32,
    /// Largest document id in the block.
    pub(crate) max_doc_id: u32,
    /// Maximum exact BM25 contribution of this term among postings in the block.
    pub(crate) max_score_contribution: f64,
    /// Suffix maximum of `max_score_contribution` from this block to the end.
    pub(crate) suffix_max_score: f64,
}

/// Block-max metadata for a term posting list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct TermBlockMaxMeta {
    pub(crate) blocks: Vec<BlockMaxBlock>,
}

/// Lazily rebuilt block-max metadata cache keyed by postings epoch.
#[derive(Debug, Clone, Default)]
pub(crate) struct BlockMaxState {
    pub(crate) built_epoch: Option<u64>,
    pub(crate) per_term: HashMap<String, TermBlockMaxMeta>,
}
