//! BM25 scoring primitives: IDF cache, term scorer, and block-max metadata builder.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::RwLock;

use super::posting::{BlockMaxBlock, PostingList, TermBlockMaxMeta};

/// IDF cache keyed by document frequency; invalidated when doc_count changes.
#[derive(Debug, Default)]
pub(crate) struct IdfCache {
    /// The `N` (total document count) for which cached values are valid.
    pub(crate) cached_doc_count: AtomicUsize,
    /// Map from document frequency -> precomputed IDF value.
    pub(crate) by_df: RwLock<HashMap<usize, f64>>,
}

impl Clone for IdfCache {
    fn clone(&self) -> Self {
        let map_clone = self
            .by_df
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Self {
            cached_doc_count: AtomicUsize::new(self.cached_doc_count.load(AtomicOrdering::Relaxed)),
            by_df: RwLock::new(map_clone),
        }
    }
}

/// Robertson-Walker IDF: always non-negative via `+1` inside `ln()`.
#[inline]
pub(crate) fn idf_from_doc_freq(doc_freq: usize, doc_count: usize) -> f64 {
    let n = doc_count as f64;
    let df = doc_freq as f64;
    ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
}

/// Compute a single-term BM25 contribution for a posting.
#[inline]
pub(crate) fn bm25_term_score(
    idf: f64,
    term_freq: u8,
    doc_length: usize,
    avgdl: f64,
    k1: f64,
    b: f64,
) -> f64 {
    if avgdl <= f64::EPSILON {
        return 0.0;
    }

    let tf = term_freq as f64;
    let numerator = tf * (k1 + 1.0);
    let denominator = tf + k1 * (1.0 - b + b * (doc_length as f64 / avgdl));
    idf * (numerator / denominator)
}

/// Pre-computed BM25 scoring constants for one term; eliminates hot-loop arithmetic.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bm25TermScorer {
    pub(crate) idf: f64,
    /// k1 + 1.0
    k1_plus_1: f64,
    /// k1 * (1.0 - b)
    k1_times_one_minus_b: f64,
    /// k1 * b / avgdl
    k1_times_b_over_avgdl: f64,
}

impl Bm25TermScorer {
    #[inline]
    pub(crate) fn new(idf: f64, k1: f64, b: f64, avgdl: f64) -> Self {
        let inv_avgdl = if avgdl > f64::EPSILON {
            1.0 / avgdl
        } else {
            0.0
        };
        Self {
            idf,
            k1_plus_1: k1 + 1.0,
            k1_times_one_minus_b: k1 * (1.0 - b),
            k1_times_b_over_avgdl: k1 * b * inv_avgdl,
        }
    }

    /// IDF value for this term.
    #[inline]
    pub(crate) fn idf_f32(&self) -> f32 {
        self.idf as f32
    }

    /// Pre-computed k1 + 1.
    #[inline]
    pub(crate) fn k1_plus_1_f32(&self) -> f32 {
        self.k1_plus_1 as f32
    }

    /// Pre-computed k1 * (1 - b), the constant portion of the denominator.
    #[inline]
    pub(crate) fn denom_base_f32(&self) -> f32 {
        self.k1_times_one_minus_b as f32
    }

    /// Pre-computed k1 * b / avgdl, the per-doc-length factor in the denominator.
    #[inline]
    pub(crate) fn denom_dl_factor_f32(&self) -> f32 {
        self.k1_times_b_over_avgdl as f32
    }

    /// Score a posting with pre-computed constants.
    #[inline]
    pub(crate) fn score(&self, term_freq: u8, doc_length: usize) -> f64 {
        let tf = term_freq as f64;
        let numerator = tf * self.k1_plus_1;
        let denominator =
            tf + self.k1_times_one_minus_b + self.k1_times_b_over_avgdl * (doc_length as f64);
        self.idf * (numerator / denominator)
    }
}

pub(crate) fn build_term_block_max_meta(
    postings: &PostingList,
    doc_lengths: &HashMap<u32, usize>,
    block_size: usize,
    idf: f64,
    avgdl: f64,
    k1: f64,
    b: f64,
) -> TermBlockMaxMeta {
    if postings.is_empty() {
        return TermBlockMaxMeta::default();
    }

    let n = postings.len();
    let num_blocks = n.div_ceil(block_size);
    let mut blocks = Vec::with_capacity(num_blocks);

    for block_idx in 0..num_blocks {
        let start = block_idx * block_size;
        let end = (start + block_size).min(n);

        let min_doc_id = postings.doc_ids[start];
        let max_doc_id = postings.doc_ids[end - 1];

        let mut max_score_contribution = 0.0;
        for i in start..end {
            let doc_id = postings.doc_ids[i];
            let term_freq = postings.term_freqs[i];
            let doc_length = doc_lengths.get(&doc_id).copied().unwrap_or_else(|| {
                debug_assert!(false, "posting list references unknown doc_id {doc_id}");
                0
            });
            let score = bm25_term_score(idf, term_freq, doc_length, avgdl, k1, b);
            if score > max_score_contribution {
                max_score_contribution = score;
            }
        }

        blocks.push(BlockMaxBlock {
            min_doc_id,
            max_doc_id,
            max_score_contribution,
            suffix_max_score: max_score_contribution,
        });
    }

    // Compute suffix-max scores (back to front).
    let mut suffix_max = 0.0;
    for block in blocks.iter_mut().rev() {
        if block.max_score_contribution > suffix_max {
            suffix_max = block.max_score_contribution;
        }
        block.suffix_max_score = suffix_max;
    }

    TermBlockMaxMeta { blocks }
}

/// Statistics about a BM25 index.
#[derive(Debug, Clone, Default)]
pub struct Bm25Stats {
    /// Number of indexed documents.
    pub doc_count: usize,
    /// Total token count across all documents.
    pub total_tokens: usize,
    /// Average document length (in tokens).
    pub avg_doc_length: f64,
    /// Number of unique terms in the index.
    pub unique_terms: usize,
}

#[cfg(test)]
mod poison_tests {
    use super::IdfCache;
    use std::sync::atomic::Ordering;

    #[test]
    fn idf_cache_clone_recovers_data_from_poisoned_lock() {
        let cache = IdfCache::default();
        cache.cached_doc_count.store(10, Ordering::Relaxed);

        // Populate the cache before poisoning.
        {
            let mut guard = cache.by_df.write().unwrap();
            guard.insert(1, 2.5);
            guard.insert(3, 1.1);
        }

        // Poison the lock by panicking inside a write-lock scope.
        let _ = std::panic::catch_unwind(|| {
            let _guard = cache.by_df.write().unwrap();
            panic!("intentional poison");
        });

        // The lock is now poisoned — verify that.
        assert!(cache.by_df.read().is_err(), "lock must be poisoned");

        // Clone must recover the populated data, not produce an empty map.
        let cloned = cache.clone();

        let guard = cloned.by_df.read().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            guard.len(),
            2,
            "cloned IdfCache must preserve all entries from a poisoned lock"
        );
        assert!(
            (*guard.get(&1).unwrap() - 2.5).abs() < f64::EPSILON,
            "IDF value for df=1 must survive poison-clone"
        );
        assert_eq!(
            cloned.cached_doc_count.load(Ordering::Relaxed),
            10,
            "doc_count must be preserved across poison-clone"
        );
    }
}
