//! Search operations for BM25 index.
//!
//! Routes to brute-force SIMD or Block-Max WAND depending on posting-list size.
//! See `docs/simd.md` for SIMD platform support and dispatch strategy.
//!
//! FILE SIZE NOTE: This file is ~1050 lines including ~334 lines of inline SIMD parity
//! tests that require pub(super) access to scoring functions. Production code is ~714
//! lines (under the 700-line target).

mod cursor;
mod helpers;
mod simd;

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::sync::Arc;

use khive_score::DeterministicScore;

use super::{Bm25Index, Bm25TermScorer};
use crate::metrics::{self, MetricEvent, MetricValue};

use cursor::TermCursor;
use helpers::{
    advance_all_cursors_on_pivot, advance_one_cursor_past_block, align_cursors,
    current_threshold_score, find_pivot_doc, heap_to_results, maybe_push_top_k,
    sort_and_prune_terminated,
};
use simd::score_batch_4;
#[cfg(target_arch = "x86_64")]
use simd::select_score_batch_8;

/// Postings threshold below which the brute-force SIMD scorer is used instead of
/// Block-Max WAND. The brute-force path processes postings sequentially in
/// NEON/scalar batches of 4 with zero cursor/heap overhead, which is faster than
/// WAND for moderate posting counts. WAND's block-skip pruning only wins when the
/// total postings are large enough that it can skip significant portions.
///
/// Empirically tuned: at ~10K-16K total postings the brute-force SIMD path
/// matches or beats WAND on aarch64 (Apple M-series). Above 16K the WAND
/// block-skip savings overcome its per-cursor overhead.
const SMALL_QUERY_POSTINGS_THRESHOLD: usize = 16_384;
const TERMINATED_DOC: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
struct HeapEntry {
    doc_id: u32,
    score: f64,
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
struct ShallowBlockInfo {
    max_score: f64,
    last_doc: u32,
}

/// Reusable per-query scratch space for [`Bm25Index::search_with_context`].
///
/// Every call to [`Bm25Index::search`] allocates a fresh result buffer and
/// heap. Reusing one context across calls avoids that churn.
///
/// The context is automatically cleared at the start of each search call, so
/// there is no need to call [`clear`](Self::clear) manually between queries.
///
/// # Example
///
/// ```rust
/// use khive_bm25::{Bm25Config, Bm25Index, SearchContext};
///
/// let mut index = Bm25Index::new(Bm25Config::default());
/// index.index_document("d1", "quick brown fox").unwrap();
/// index.index_document("d2", "lazy brown dog").unwrap();
///
/// let mut ctx = SearchContext::new();
/// for query in &["quick fox", "brown dog"] {
///     let results = index.search_with_context(query, 10, &mut ctx);
///     // ctx is cleared internally and reused on the next call
/// }
/// ```
pub struct SearchContext {
    /// Vec-indexed score accumulator for brute-force path.
    /// Indexed by internal doc_id. Avoids HashMap overhead for dense ID spaces.
    /// Lazily sized on first use per query.
    score_vec: Vec<f64>,
    /// Tracks which doc_ids have non-zero scores in score_vec
    /// so we can drain results without scanning the entire Vec.
    touched_docs: Vec<u32>,
    /// Scratch buffer for sorting results before top-k truncation.
    results_buf: Vec<(u32, f64)>,
    /// Internal top-k min-heap for BMW execution.
    heap: BinaryHeap<Reverse<HeapEntry>>,
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
    ///
    /// Called automatically at the start of each
    /// [`Bm25Index::search_with_context`] invocation. You only need to call
    /// this yourself if you want to shrink the context between unrelated
    /// batches of queries.
    pub fn clear(&mut self) {
        // Reset touched entries in score_vec without zeroing the whole vec.
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

impl Bm25Index {
    /// Search for documents matching the query.
    ///
    /// Returns up to `k` documents sorted by BM25 score descending, with
    /// deterministic internal-doc-id tie-breaking identical to the brute-force
    /// scorer.
    ///
    /// For small queries (total postings < 256), falls back to exhaustive
    /// brute-force scoring. For larger queries, uses the Block-Max WAND
    /// algorithm for threshold-based pruning.
    ///
    /// # Floating-Point Boundary Conversion
    ///
    /// BM25 scoring uses `f64` internally for precision in logarithmic calculations.
    /// At the API boundary (this method), scores are converted to [`DeterministicScore`]
    /// which ensures:
    /// - Canonical representation for cross-platform consistency
    /// - Safe serialization without precision loss
    /// - Protection against NaN/Inf propagation
    ///
    /// See module-level documentation for cross-platform considerations.
    ///
    /// # Concurrency
    ///
    /// This method takes `&self` (not `&mut self`) to enable concurrent reads.
    /// The internal IDF cache and block-max metadata use interior mutability
    /// (`RwLock`) for thread-safe updates.
    ///
    /// Emits `bm25.search.duration_ms`, `bm25.search.count`, and
    /// `bm25.search.results` metrics when a sink is attached.
    ///
    /// **PROOF CORRESPONDENCE**: `khive.Retrieval.BM25.bm25_nonneg`
    /// Total BM25 score >= 0 for any query and document, since it is a sum of
    /// non-negative IDF values multiplied by non-negative TF components.
    /// Returns up to `k` (id, score) pairs sorted by BM25 score descending.
    ///
    /// The `Arc<str>` document IDs are cheaply cloneable shared references into
    /// the internal reverse-map, avoiding a heap allocation per result.  Callers
    /// that need a `DocumentId` can construct one via `DocumentId::new(&*arc)`.
    pub fn search(&self, query_text: &str, k: usize) -> Vec<(Arc<str>, DeterministicScore)> {
        let mut ctx = SearchContext::new();
        self.search_with_context(query_text, k, &mut ctx)
    }

    /// Search for documents matching the query, reusing a [`SearchContext`].
    ///
    /// Behaves identically to [`search`](Self::search) but reuses the heap
    /// memory inside `ctx` across calls, eliminating allocation churn per query.
    ///
    /// The context is automatically [`clear`](SearchContext::clear)ed at the
    /// start of each call, so callers do not need to reset it manually.
    pub fn search_with_context(
        &self,
        query_text: &str,
        k: usize,
        ctx: &mut SearchContext,
    ) -> Vec<(Arc<str>, DeterministicScore)> {
        let start = std::time::Instant::now();

        let results = self.search_inner(query_text, k, ctx);

        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::BM25_SEARCH_DURATION_MS,
                value: MetricValue::Histogram(elapsed),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::BM25_SEARCH_COUNT,
                value: MetricValue::Counter(1),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::BM25_SEARCH_RESULTS,
                value: MetricValue::Gauge(results.len() as f64),
                labels: vec![],
            },
        );

        results
    }

    /// Inner search logic (uninstrumented).
    ///
    /// Routes to brute-force for small queries and to Block-Max WAND for
    /// larger ones.
    fn search_inner(
        &self,
        query_text: &str,
        k: usize,
        ctx: &mut SearchContext,
    ) -> Vec<(Arc<str>, DeterministicScore)> {
        if k == 0 {
            ctx.clear();
            return Vec::new();
        }

        let query_tokens = self.tokenizer.tokenize(query_text);
        if query_tokens.is_empty() {
            ctx.clear();
            return Vec::new();
        }

        if self.doc_count() == 0 {
            ctx.clear();
            return Vec::new();
        }

        let total_query_postings: usize = query_tokens
            .iter()
            .map(|term| {
                self.inverted_index
                    .get(term)
                    .map(|postings| postings.len())
                    .unwrap_or(0)
            })
            .sum();

        if total_query_postings < SMALL_QUERY_POSTINGS_THRESHOLD {
            return self.search_brute_force(query_text, k, ctx);
        }

        self.ensure_block_max_metadata();
        let block_state_guard = match self.block_max_state.read() {
            Ok(guard) if guard.built_epoch == Some(self.postings_epoch) => guard,
            _ => return self.search_brute_force(query_text, k, ctx),
        };

        ctx.clear();

        let doc_count = self.doc_count();
        let avgdl = self.avg_doc_length();
        let mut cursors = Vec::with_capacity(query_tokens.len());

        let k1 = self.config.k1;
        let b = self.config.b;

        for term in &query_tokens {
            let postings = match self.inverted_index.get(term) {
                Some(postings) if !postings.is_empty() => postings,
                _ => continue,
            };
            let blocks = match block_state_guard.per_term.get(term) {
                Some(meta) if !meta.blocks.is_empty() => meta.blocks.as_slice(),
                _ => continue,
            };
            let idf = self.compute_idf(term, doc_count);
            let scorer = Bm25TermScorer::new(idf, k1, b, avgdl);
            cursors.push(TermCursor::new(postings, blocks, self.block_size, scorer));
        }

        if cursors.is_empty() {
            return Vec::new();
        }

        sort_and_prune_terminated(&mut cursors);

        while let Some((before_pivot_len, pivot_len, pivot_doc)) =
            find_pivot_doc(&cursors, current_threshold_score(&ctx.heap, k))
        {
            let threshold_score = current_threshold_score(&ctx.heap, k);
            let block_upper_bound: f64 = cursors[..pivot_len]
                .iter()
                .map(|cursor| {
                    cursor
                        .shallow_block_info(pivot_doc)
                        .map(|info| info.max_score)
                        .unwrap_or(0.0)
                })
                .sum();

            // Keep equality as competitive to preserve exact tie handling.
            if block_upper_bound < threshold_score {
                advance_one_cursor_past_block(&mut cursors, pivot_len, pivot_doc);
                if cursors.is_empty() {
                    break;
                }
                continue;
            }

            if !align_cursors(&mut cursors, pivot_doc, before_pivot_len) {
                if cursors.is_empty() {
                    break;
                }
                continue;
            }

            let score: f64 = cursors[..pivot_len]
                .iter()
                .map(|cursor| cursor.score_current(self))
                .sum();

            maybe_push_top_k(
                &mut ctx.heap,
                k,
                HeapEntry {
                    doc_id: pivot_doc,
                    score,
                },
            );

            advance_all_cursors_on_pivot(&mut cursors, pivot_len);
            if cursors.is_empty() {
                break;
            }
        }

        heap_to_results(self, ctx)
    }

    /// Exact exhaustive scorer retained as fallback and for equivalence tests.
    ///
    /// The inner loop is SIMD-batched (4-wide NEON on aarch64, scalar f32
    /// on other targets). Pre-converted f32 document lengths avoid per-scoring
    /// integer-to-float conversion.
    ///
    /// **PROOF CORRESPONDENCE**: `khive.Retrieval.BM25.tf_bounded`
    /// TF saturation: tf * (k1 + 1) / (tf + k1 * ...) < k1 + 1 for all tf >= 0.
    #[doc(hidden)]
    pub fn search_brute_force(
        &self,
        query_text: &str,
        k: usize,
        ctx: &mut SearchContext,
    ) -> Vec<(Arc<str>, DeterministicScore)> {
        ctx.clear();

        if k == 0 {
            return Vec::new();
        }

        let query_tokens = self.tokenizer.tokenize(query_text);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let doc_count = self.doc_count();
        if doc_count == 0 {
            return Vec::new();
        }

        // Pre-size the score accumulator to the maximum internal doc_id.
        // This eliminates bounds-check branches in the tight SIMD loop.
        let max_id = self.next_internal_id as usize;
        if ctx.score_vec.len() < max_id {
            ctx.score_vec.resize(max_id, 0.0);
        }

        let avgdl = self.avg_doc_length();
        let k1 = self.config.k1;
        let b = self.config.b;

        // Cache the doc_lengths_f32 slice pointer outside the term loop.
        // All internal doc_ids are < max_id which is <= doc_lengths_f32.len()
        // (maintained by set_doc_length_fast on every insert).
        let dl_f32 = &self.doc_lengths_f32;
        let scores_vec = &mut ctx.score_vec;
        let touched = &mut ctx.touched_docs;

        for term in &query_tokens {
            let postings = match self.inverted_index.get(term) {
                Some(postings) => postings,
                None => continue,
            };
            let idf = self.compute_idf(term, doc_count);
            let scorer = Bm25TermScorer::new(idf, k1, b, avgdl);

            // Extract f32 SIMD parameters from the pre-computed scorer.
            let simd_idf = scorer.idf_f32();
            let simd_k1p1 = scorer.k1_plus_1_f32();
            let simd_base = scorer.denom_base_f32();
            let simd_dl_fac = scorer.denom_dl_factor_f32();

            // SoA layout: doc_ids and term_freqs are separate contiguous arrays.
            let n = postings.len();
            let doc_ids = &postings.doc_ids;
            let tfs_arr = &postings.term_freqs;

            // On x86_64, resolve the best 8-wide scoring function once per term
            // (AVX2+FMA > AVX2 > scalar) and process in chunks of 8. On aarch64,
            // process in chunks of 4 using NEON.
            #[cfg(target_arch = "x86_64")]
            {
                let score_fn = select_score_batch_8();
                let full_chunks_8 = n / 8;

                for chunk_idx in 0..full_chunks_8 {
                    let base_idx = chunk_idx * 8;
                    let tfs: [u8; 8] = [
                        tfs_arr[base_idx],
                        tfs_arr[base_idx + 1],
                        tfs_arr[base_idx + 2],
                        tfs_arr[base_idx + 3],
                        tfs_arr[base_idx + 4],
                        tfs_arr[base_idx + 5],
                        tfs_arr[base_idx + 6],
                        tfs_arr[base_idx + 7],
                    ];
                    let d0 = doc_ids[base_idx] as usize;
                    let d1 = doc_ids[base_idx + 1] as usize;
                    let d2 = doc_ids[base_idx + 2] as usize;
                    let d3 = doc_ids[base_idx + 3] as usize;
                    let d4 = doc_ids[base_idx + 4] as usize;
                    let d5 = doc_ids[base_idx + 5] as usize;
                    let d6 = doc_ids[base_idx + 6] as usize;
                    let d7 = doc_ids[base_idx + 7] as usize;
                    let lens = [
                        dl_f32[d0], dl_f32[d1], dl_f32[d2], dl_f32[d3], dl_f32[d4], dl_f32[d5],
                        dl_f32[d6], dl_f32[d7],
                    ];
                    // SAFETY: score_fn is selected based on runtime CPU feature
                    // detection; each variant's target_feature attribute matches
                    // what was detected.
                    let batch_scores = unsafe {
                        score_fn(&tfs, &lens, simd_idf, simd_k1p1, simd_base, simd_dl_fac)
                    };

                    // Accumulate all 8 scores.
                    macro_rules! accum {
                        ($idx:expr, $d:expr) => {
                            if scores_vec[$d] == 0.0 {
                                touched.push(doc_ids[base_idx + $idx]);
                            }
                            scores_vec[$d] += batch_scores[$idx] as f64;
                        };
                    }
                    accum!(0, d0);
                    accum!(1, d1);
                    accum!(2, d2);
                    accum!(3, d3);
                    accum!(4, d4);
                    accum!(5, d5);
                    accum!(6, d6);
                    accum!(7, d7);
                }

                // Process remaining 4-7 postings in a 4-wide batch.
                let remainder_start = full_chunks_8 * 8;
                let remaining = n - remainder_start;
                if remaining >= 4 {
                    let tfs = [
                        tfs_arr[remainder_start],
                        tfs_arr[remainder_start + 1],
                        tfs_arr[remainder_start + 2],
                        tfs_arr[remainder_start + 3],
                    ];
                    let d0 = doc_ids[remainder_start] as usize;
                    let d1 = doc_ids[remainder_start + 1] as usize;
                    let d2 = doc_ids[remainder_start + 2] as usize;
                    let d3 = doc_ids[remainder_start + 3] as usize;
                    let lens = [dl_f32[d0], dl_f32[d1], dl_f32[d2], dl_f32[d3]];
                    let batch_scores =
                        score_batch_4(&tfs, &lens, simd_idf, simd_k1p1, simd_base, simd_dl_fac);
                    if scores_vec[d0] == 0.0 {
                        touched.push(doc_ids[remainder_start]);
                    }
                    scores_vec[d0] += batch_scores[0] as f64;
                    if scores_vec[d1] == 0.0 {
                        touched.push(doc_ids[remainder_start + 1]);
                    }
                    scores_vec[d1] += batch_scores[1] as f64;
                    if scores_vec[d2] == 0.0 {
                        touched.push(doc_ids[remainder_start + 2]);
                    }
                    scores_vec[d2] += batch_scores[2] as f64;
                    if scores_vec[d3] == 0.0 {
                        touched.push(doc_ids[remainder_start + 3]);
                    }
                    scores_vec[d3] += batch_scores[3] as f64;
                }
                let scalar_start = remainder_start + if remaining >= 4 { 4 } else { 0 };

                // Scalar tail for remaining 0-3 postings.
                for i in scalar_start..n {
                    let doc_id = doc_ids[i];
                    let d = doc_id as usize;
                    let doc_length = self.doc_length_fast(doc_id);
                    let term_score = scorer.score(tfs_arr[i], doc_length);
                    if scores_vec[d] == 0.0 {
                        touched.push(doc_id);
                    }
                    scores_vec[d] += term_score;
                }
            }

            // aarch64 path: 4-wide NEON batching (unchanged from original).
            #[cfg(target_arch = "aarch64")]
            {
                let full_chunks = n / 4;

                for chunk_idx in 0..full_chunks {
                    let base_idx = chunk_idx * 4;
                    let tfs = [
                        tfs_arr[base_idx],
                        tfs_arr[base_idx + 1],
                        tfs_arr[base_idx + 2],
                        tfs_arr[base_idx + 3],
                    ];
                    let d0 = doc_ids[base_idx] as usize;
                    let d1 = doc_ids[base_idx + 1] as usize;
                    let d2 = doc_ids[base_idx + 2] as usize;
                    let d3 = doc_ids[base_idx + 3] as usize;
                    let lens = [dl_f32[d0], dl_f32[d1], dl_f32[d2], dl_f32[d3]];
                    let batch_scores =
                        score_batch_4(&tfs, &lens, simd_idf, simd_k1p1, simd_base, simd_dl_fac);
                    if scores_vec[d0] == 0.0 {
                        touched.push(doc_ids[base_idx]);
                    }
                    scores_vec[d0] += batch_scores[0] as f64;
                    if scores_vec[d1] == 0.0 {
                        touched.push(doc_ids[base_idx + 1]);
                    }
                    scores_vec[d1] += batch_scores[1] as f64;
                    if scores_vec[d2] == 0.0 {
                        touched.push(doc_ids[base_idx + 2]);
                    }
                    scores_vec[d2] += batch_scores[2] as f64;
                    if scores_vec[d3] == 0.0 {
                        touched.push(doc_ids[base_idx + 3]);
                    }
                    scores_vec[d3] += batch_scores[3] as f64;
                }

                // Scalar fallback for remaining 0-3 postings.
                for i in (full_chunks * 4)..n {
                    let doc_id = doc_ids[i];
                    let d = doc_id as usize;
                    let doc_length = self.doc_length_fast(doc_id);
                    let term_score = scorer.score(tfs_arr[i], doc_length);
                    if scores_vec[d] == 0.0 {
                        touched.push(doc_id);
                    }
                    scores_vec[d] += term_score;
                }
            }

            // Generic fallback for architectures other than x86_64 and aarch64.
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            {
                for i in 0..n {
                    let doc_id = doc_ids[i];
                    let d = doc_id as usize;
                    let doc_length = self.doc_length_fast(doc_id);
                    let term_score = scorer.score(tfs_arr[i], doc_length);
                    if scores_vec[d] == 0.0 {
                        touched.push(doc_id);
                    }
                    scores_vec[d] += term_score;
                }
            }
        }

        // Drain touched_docs into results buffer.
        ctx.results_buf.clear();
        for &doc_id in &ctx.touched_docs {
            let score = ctx.score_vec[doc_id as usize];
            if score > 0.0 {
                ctx.results_buf.push((doc_id, score));
            }
        }

        // Partial sort: if we only need k results from a large set, use
        // select_nth_unstable_by to avoid fully sorting all results.
        if k < ctx.results_buf.len() {
            ctx.results_buf
                .select_nth_unstable_by(k, |a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            ctx.results_buf.truncate(k);
        }
        ctx.results_buf
            .sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        ctx.results_buf
            .iter()
            .take(k)
            .filter_map(|(internal_id, score)| {
                // resolve_internal_id returns Arc<str>; Arc::clone is an atomic
                // refcount bump — no heap allocation, no memcpy.
                let doc_id = self.resolve_internal_id(*internal_id)?;
                Some((doc_id, DeterministicScore::from_f64(*score)))
            })
            .collect()
    }

    /// Compute IDF (Inverse Document Frequency) for a term.
    ///
    /// Uses the BM25 IDF formula:
    /// ```text
    /// IDF(qi) = ln((N - n(qi) + 0.5) / (n(qi) + 0.5) + 1)
    /// ```
    ///
    /// This variant always returns non-negative IDF (Robertson-Walker variant).
    /// Uses interior mutability for cache updates to enable concurrent reads.
    ///
    /// **PROOF CORRESPONDENCE**: `khive.Retrieval.BM25.idf_nonneg`
    /// With +1 inside ln(), IDF(t) >= 0 for all terms regardless of document frequency.
    ///
    /// **PROOF CORRESPONDENCE**: `khive.Retrieval.BM25.idf_mono`
    /// Rarer terms have higher IDF: n1 < n2 implies IDF(n1) > IDF(n2).
    pub(super) fn compute_idf(&self, term: &str, doc_count: usize) -> f64 {
        use std::sync::atomic::Ordering as AtomicOrdering;

        // If N changed since the cache was last populated, invalidate everything.
        let cached_n = self
            .idf_cache
            .cached_doc_count
            .load(AtomicOrdering::Relaxed);
        if cached_n != doc_count {
            if let Ok(mut cache) = self.idf_cache.by_df.write() {
                // Double-check after acquiring the write lock to avoid races
                // where another thread already cleared + updated.
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

        // Check cache by df (read lock)
        if let Ok(cache) = self.idf_cache.by_df.read() {
            if let Some(&cached) = cache.get(&doc_freq) {
                return cached;
            }
        }

        let idf = super::idf_from_doc_freq(doc_freq, doc_count);

        // Cache by df and return (write lock)
        if let Ok(mut cache) = self.idf_cache.by_df.write() {
            cache.insert(doc_freq, idf);
        }

        idf
    }
}

// ---------------------------------------------------------------------------
// Tests: SIMD batch scoring parity and edge cases
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests_simd_scoring {
    use super::simd::*;
    use super::*;

    /// Reference scalar BM25 score for a single posting.
    fn scalar_bm25(tf: u8, doc_len: f32, idf: f32, k1p1: f32, base: f32, dl_fac: f32) -> f32 {
        let tf = tf as f32;
        let num = tf * k1p1;
        let denom = tf + base + dl_fac * doc_len;
        idf * (num / denom)
    }

    /// Compute reference scores for an arbitrary-length batch using scalar code.
    fn reference_scores(
        tfs: &[u8],
        dls: &[f32],
        idf: f32,
        k1p1: f32,
        base: f32,
        dl_fac: f32,
    ) -> Vec<f32> {
        tfs.iter()
            .zip(dls.iter())
            .map(|(&tf, &dl)| scalar_bm25(tf, dl, idf, k1p1, base, dl_fac))
            .collect()
    }

    // Test parameters (standard BM25 with k1=1.2, b=0.75, avgdl=10.0)
    const TEST_IDF: f32 = 1.5;
    const TEST_K1P1: f32 = 2.2; // k1 + 1 = 1.2 + 1
    const TEST_BASE: f32 = 0.3; // k1 * (1 - b) = 1.2 * 0.25
    const TEST_DL_FAC: f32 = 0.09; // k1 * b / avgdl = 1.2 * 0.75 / 10.0

    // -----------------------------------------------------------------------
    // Test 1: scalar_4 vs reference (parity check)
    // -----------------------------------------------------------------------

    #[test]
    fn test_score_batch_4_matches_scalar() {
        let tfs: [u8; 4] = [1, 3, 5, 10];
        let dls: [f32; 4] = [8.0, 12.0, 5.0, 20.0];

        let batch = score_batch_4(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
        let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

        for i in 0..4 {
            assert!(
                (batch[i] - reference[i]).abs() < 1e-6,
                "batch_4[{i}] = {}, expected {} (delta {})",
                batch[i],
                reference[i],
                (batch[i] - reference[i]).abs()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 2: x86_64 AVX2 8-wide vs scalar parity
    // -----------------------------------------------------------------------

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_avx2_matches_scalar_basic() {
        if !is_x86_feature_detected!("avx2") {
            eprintln!("AVX2 not available, skipping test");
            return;
        }

        let tfs: [u8; 8] = [1, 2, 3, 5, 8, 13, 21, 34];
        let dls: [f32; 8] = [5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 35.0, 40.0];

        // SAFETY: The test returns early unless AVX2 is detected, and the
        // fixed-size arrays provide all lanes consumed by the helper.
        let avx2_result =
            unsafe { score_batch_avx2(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC) };
        let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

        for i in 0..8 {
            assert!(
                (avx2_result[i] - reference[i]).abs() < 1e-6,
                "avx2[{i}] = {}, expected {} (delta {})",
                avx2_result[i],
                reference[i],
                (avx2_result[i] - reference[i]).abs()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 3: AVX2+FMA vs scalar (slightly relaxed tolerance due to FMA rounding)
    // -----------------------------------------------------------------------

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_avx2_fma_matches_scalar() {
        if !is_x86_feature_detected!("avx2") || !is_x86_feature_detected!("fma") {
            eprintln!("AVX2+FMA not available, skipping test");
            return;
        }

        let tfs: [u8; 8] = [0, 1, 127, 255, 42, 7, 99, 200];
        let dls: [f32; 8] = [1.0, 2.0, 100.0, 0.5, 10.0, 50.0, 3.0, 1000.0];

        // SAFETY: The test returns early unless AVX2+FMA is detected, and the
        // fixed-size arrays provide all lanes consumed by the helper.
        let fma_result = unsafe {
            score_batch_avx2_fma(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC)
        };
        let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

        // FMA has single rounding vs two roundings in mul+add, so allow slightly
        // more tolerance (1 ULP of f32 ~ 1.19e-7, we allow ~10 ULPs).
        for i in 0..8 {
            let tol = reference[i].abs() * 1e-6 + 1e-7;
            assert!(
                (fma_result[i] - reference[i]).abs() < tol,
                "fma[{i}] = {}, expected {} (delta {}, tol {})",
                fma_result[i],
                reference[i],
                (fma_result[i] - reference[i]).abs(),
                tol
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 4: x86_64 dispatch function selects correctly and produces correct results
    // -----------------------------------------------------------------------

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_dispatch_score_batch_8() {
        let score_fn = select_score_batch_8();

        let tfs: [u8; 8] = [3, 7, 1, 15, 0, 255, 128, 50];
        let dls: [f32; 8] = [10.0, 5.0, 20.0, 8.0, 100.0, 1.0, 15.0, 30.0];

        // SAFETY: `select_score_batch_8` only returns a target-feature helper
        // after matching runtime CPU detection; otherwise it returns scalar.
        let dispatched =
            unsafe { score_fn(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC) };
        let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

        for i in 0..8 {
            let tol = reference[i].abs() * 1e-5 + 1e-7;
            assert!(
                (dispatched[i] - reference[i]).abs() < tol,
                "dispatch[{i}] = {}, expected {} (delta {})",
                dispatched[i],
                reference[i],
                (dispatched[i] - reference[i]).abs()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 5: Edge case -- tf=0 produces zero score
    // -----------------------------------------------------------------------

    #[test]
    fn test_tf_zero_produces_zero_score() {
        let tfs_4: [u8; 4] = [0, 0, 0, 0];
        let dls_4: [f32; 4] = [10.0, 20.0, 5.0, 1.0];
        let result = score_batch_4(&tfs_4, &dls_4, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
        for val in &result {
            assert!(
                val.abs() < 1e-10,
                "tf=0 should produce ~0 score, got {}",
                val
            );
        }

        #[cfg(target_arch = "x86_64")]
        if is_x86_feature_detected!("avx2") {
            let tfs_8: [u8; 8] = [0; 8];
            let dls_8: [f32; 8] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
            // SAFETY: This branch only runs when AVX2 is detected, and the
            // fixed-size arrays provide all lanes consumed by the helper.
            let result = unsafe {
                score_batch_avx2(&tfs_8, &dls_8, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC)
            };
            for val in &result {
                assert!(
                    val.abs() < 1e-10,
                    "avx2 tf=0 should produce ~0 score, got {}",
                    val
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Test 6: Edge case -- very large doc_length
    // -----------------------------------------------------------------------

    #[test]
    fn test_large_doc_length() {
        let tfs: [u8; 4] = [5, 10, 20, 50];
        let dls: [f32; 4] = [1e6, 1e6, 1e6, 1e6];
        let result = score_batch_4(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
        let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

        for i in 0..4 {
            // Very large doc_length pushes scores toward zero but they should
            // still be positive and match scalar.
            assert!(result[i] > 0.0, "score should be positive");
            assert!(
                (result[i] - reference[i]).abs() < 1e-6,
                "large dl mismatch at [{i}]: {} vs {}",
                result[i],
                reference[i]
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 7: Edge case -- max tf (255)
    // -----------------------------------------------------------------------

    #[test]
    fn test_max_tf() {
        let tfs: [u8; 4] = [255, 255, 255, 255];
        let dls: [f32; 4] = [10.0, 10.0, 10.0, 10.0];
        let result = score_batch_4(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
        let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

        for i in 0..4 {
            assert!(
                (result[i] - reference[i]).abs() < 1e-5,
                "max tf mismatch at [{i}]: {} vs {}",
                result[i],
                reference[i]
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 8: Integration test -- brute-force search with various posting lengths
    // Exercises batch sizes 1, 7, 8, 16, 100 by indexing documents.
    // -----------------------------------------------------------------------

    #[test]
    fn test_brute_force_search_various_sizes() {
        use crate::{Bm25Config, Bm25Index};

        let mut index = Bm25Index::new(Bm25Config::default());

        // Index enough documents to exercise different batch sizes.
        // The word "alpha" appears in all 100 docs, giving a posting list of 100.
        // The word "beta" appears in 16 docs.
        // The word "gamma" appears in 8 docs.
        // The word "delta" appears in 7 docs.
        // The word "epsilon" appears in 1 doc.
        for i in 0..100 {
            let mut text = format!("alpha doc{i}");
            if i < 16 {
                text.push_str(" beta");
            }
            if i < 8 {
                text.push_str(" gamma");
            }
            if i < 7 {
                text.push_str(" delta");
            }
            if i == 0 {
                text.push_str(" epsilon");
            }
            index.index_document(format!("doc{i}"), &text).unwrap();
        }

        // Each query exercises a different posting list length through brute-force.
        let mut ctx = SearchContext::new();
        for query in &["alpha", "beta", "gamma", "delta", "epsilon"] {
            let results = index.search_with_context(query, 10, &mut ctx);
            assert!(!results.is_empty(), "query '{query}' should return results");
            // All scores should be positive.
            for (doc_id, score) in &results {
                assert!(
                    score.to_f64() > 0.0,
                    "query '{query}', doc '{doc_id}': score should be positive"
                );
            }
        }

        // Multi-term query exercises score accumulation across terms.
        let results = index.search_with_context("alpha beta gamma", 5, &mut ctx);
        assert!(!results.is_empty());
        // The first result should be a doc that contains all three terms.
        let (top_doc, _) = &results[0];
        let top_id: usize = top_doc.strip_prefix("doc").unwrap().parse().unwrap();
        assert!(
            top_id < 8,
            "top result should be a doc with all 3 terms (doc0-doc7), got doc{top_id}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 9: scalar_8 matches reference (non-SIMD path)
    // -----------------------------------------------------------------------

    #[cfg(not(target_arch = "aarch64"))]
    #[test]
    fn test_score_batch_scalar_8_matches_reference() {
        let tfs: [u8; 8] = [1, 5, 10, 20, 50, 100, 200, 255];
        let dls: [f32; 8] = [3.0, 7.0, 15.0, 25.0, 50.0, 100.0, 200.0, 500.0];

        let result = score_batch_scalar_8(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
        let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

        for i in 0..8 {
            assert!(
                (result[i] - reference[i]).abs() < 1e-7,
                "scalar_8[{i}] = {}, expected {}",
                result[i],
                reference[i]
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 10: Empty posting list handled correctly (no panic)
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_posting_list_search() {
        use crate::{Bm25Config, Bm25Index};

        let mut index = Bm25Index::new(Bm25Config::default());
        index.index_document("doc1", "hello world").unwrap();

        // Search for a term not in the index.
        let results = index.search("nonexistent", 10);
        assert!(results.is_empty());
    }
}
