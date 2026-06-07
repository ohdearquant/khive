//! IDF (Inverse Document Frequency) computation with per-doc-count caching.

use super::super::{idf_from_doc_freq, Bm25Index};

impl Bm25Index {
    /// Compute BM25 IDF for a term using the Robertson-Walker variant (always non-negative).
    pub(super) fn compute_idf(&self, term: &str, doc_count: usize) -> f64 {
        use std::sync::atomic::Ordering as AtomicOrdering;

        let cached_n = self
            .idf_cache
            .cached_doc_count
            .load(AtomicOrdering::Relaxed);
        if cached_n != doc_count {
            if let Ok(mut cache) = self.idf_cache.by_df.write() {
                let recheck = self
                    .idf_cache
                    .cached_doc_count
                    .load(AtomicOrdering::Relaxed);
                if recheck != doc_count {
                    cache.clear();
                    self.idf_cache
                        .cached_doc_count
                        .store(doc_count, AtomicOrdering::Relaxed);
                }
            }
        }

        let doc_freq = self.inverted_index.get(term).map(|p| p.len()).unwrap_or(0);

        if let Ok(cache) = self.idf_cache.by_df.read() {
            if let Some(&cached) = cache.get(&doc_freq) {
                return cached;
            }
        }

        let idf = idf_from_doc_freq(doc_freq, doc_count);

        if let Ok(mut cache) = self.idf_cache.by_df.write() {
            cache.insert(doc_freq, idf);
        }

        idf
    }
}
