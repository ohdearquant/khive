//! Search operations for BM25 index.
//!
//! # SIMD Acceleration
//!
//! The brute-force scoring path uses architecture-specific SIMD to process
//! postings in parallel:
//!
//! - **aarch64 (NEON)**: 4-wide batches using 128-bit NEON registers.
//! - **x86_64 (AVX2)**: 8-wide batches using 256-bit YMM registers, with
//!   optional FMA for fused multiply-add in the denominator computation.
//!   Detected at runtime via `is_x86_feature_detected!`.
//! - **Scalar fallback**: Used on all other targets or when AVX2 is not
//!   available at runtime.
//!
//! The dispatch happens once per term (not per batch) to avoid repeated
//! feature checks in the hot loop.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::sync::Arc;

use khive_score::DeterministicScore;

use super::{BlockMaxBlock, Bm25Index, Bm25TermScorer, PostingList};
use crate::metrics::{self, MetricEvent, MetricValue};

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

// ---------------------------------------------------------------------------
// SIMD batch BM25 scoring (4-wide)
// ---------------------------------------------------------------------------

/// Batch-score 4 postings using ARM NEON SIMD intrinsics.
///
/// Computes the BM25 formula for 4 documents simultaneously:
/// ```text
/// score[i] = idf * (tf[i] * k1_plus_1) / (tf[i] + denom_base + denom_dl_factor * doc_len[i])
/// ```
///
/// Term frequencies are provided as `u8` (clamped at indexing time) and
/// widened to f32 for SIMD arithmetic. All scoring arithmetic is done in
/// f32 for SIMD throughput. The caller is responsible for converting the
/// results back to f64 for accumulation.
///
/// # Safety
///
/// Uses `std::arch::aarch64` NEON intrinsics which require the target to
/// be an AArch64 CPU. This function is gated by `#[cfg(target_arch = "aarch64")]`
/// and is only called on ARM64 hardware.
#[cfg(target_arch = "aarch64")]
#[inline]
// SAFETY: Callers only reach this helper on aarch64, and the fixed-size array
// parameters guarantee the four term-frequency and document-length lanes exist.
unsafe fn score_batch_neon(
    term_freqs: &[u8; 4],
    doc_lengths: &[f32; 4],
    idf: f32,
    k1_plus_1: f32,
    denom_base: f32,
    denom_dl_factor: f32,
) -> [f32; 4] {
    use std::arch::aarch64::*;

    // Widen u8 term frequencies to u32, then convert to f32.
    let tfs_u32: [u32; 4] = [
        term_freqs[0] as u32,
        term_freqs[1] as u32,
        term_freqs[2] as u32,
        term_freqs[3] as u32,
    ];
    let tf = vcvtq_f32_u32(vld1q_u32(tfs_u32.as_ptr()));
    // Load 4 pre-converted f32 document lengths.
    let dl = vld1q_f32(doc_lengths.as_ptr());

    let k1p1 = vdupq_n_f32(k1_plus_1);
    let base = vdupq_n_f32(denom_base);
    let dl_fac = vdupq_n_f32(denom_dl_factor);
    let idf_v = vdupq_n_f32(idf);

    // numerator = tf * k1_plus_1
    let num = vmulq_f32(tf, k1p1);
    // denominator = tf + denom_base + denom_dl_factor * doc_len
    let denom = vaddq_f32(tf, vaddq_f32(base, vmulq_f32(dl_fac, dl)));
    // score = idf * num / denom
    let score = vmulq_f32(idf_v, vdivq_f32(num, denom));

    let mut result = [0.0f32; 4];
    vst1q_f32(result.as_mut_ptr(), score);
    result
}

/// Scalar fallback for batch scoring (4-wide).
///
/// Computes the same BM25 formula as `score_batch_neon` but using plain
/// scalar f32 arithmetic. Used when no SIMD path is available.
#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn score_batch_scalar_4(
    term_freqs: &[u8; 4],
    doc_lengths: &[f32; 4],
    idf: f32,
    k1_plus_1: f32,
    denom_base: f32,
    denom_dl_factor: f32,
) -> [f32; 4] {
    let mut result = [0.0f32; 4];
    for i in 0..4 {
        let tf = term_freqs[i] as f32;
        let num = tf * k1_plus_1;
        let denom = tf + denom_base + denom_dl_factor * doc_lengths[i];
        result[i] = idf * (num / denom);
    }
    result
}

/// Scalar fallback for batch scoring (8-wide).
///
/// Computes BM25 scores for 8 postings using plain scalar f32 arithmetic.
/// Used on x86_64 when AVX2 is not available at runtime.
#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn score_batch_scalar_8(
    term_freqs: &[u8; 8],
    doc_lengths: &[f32; 8],
    idf: f32,
    k1_plus_1: f32,
    denom_base: f32,
    denom_dl_factor: f32,
) -> [f32; 8] {
    let mut result = [0.0f32; 8];
    for i in 0..8 {
        let tf = term_freqs[i] as f32;
        let num = tf * k1_plus_1;
        let denom = tf + denom_base + denom_dl_factor * doc_lengths[i];
        result[i] = idf * (num / denom);
    }
    result
}

// ---------------------------------------------------------------------------
// AVX2 batch BM25 scoring (8-wide, x86_64 only)
// ---------------------------------------------------------------------------

/// Batch-score 8 postings using AVX2 SIMD intrinsics (256-bit, 8 x f32).
///
/// Computes the BM25 formula for 8 documents simultaneously:
/// ```text
/// score[i] = idf * (tf[i] * k1_plus_1) / (tf[i] + denom_base + denom_dl_factor * doc_len[i])
/// ```
///
/// The u8 term frequencies are widened to i32 via `_mm256_cvtepu8_epi32`
/// (requires only the low 64 bits of a 128-bit register), then converted
/// to f32 via `_mm256_cvtepi32_ps`.
///
/// Uses full-precision `_mm256_div_ps` for the division. While approximate
/// reciprocal (`_mm256_rcp_ps` + Newton-Raphson) would save ~5 cycles, the
/// division is not the bottleneck here -- memory access to doc_lengths is.
/// Full precision keeps scoring deterministic with the scalar path.
///
/// # Safety
///
/// Requires the `avx2` target feature. The caller must verify AVX2 support
/// at runtime via `is_x86_feature_detected!("avx2")` before calling.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
// SAFETY: Callers must select this helper only after AVX2 runtime detection.
// Fixed-size array parameters guarantee the eight lanes read by the intrinsics.
unsafe fn score_batch_avx2(
    term_freqs: &[u8; 8],
    doc_lengths: &[f32; 8],
    idf: f32,
    k1_plus_1: f32,
    denom_base: f32,
    denom_dl_factor: f32,
) -> [f32; 8] {
    use std::arch::x86_64::*;

    // Load 8 u8 term frequencies from a 64-bit chunk into the low half of
    // a 128-bit register, then widen u8 -> i32 (AVX2) and convert i32 -> f32.
    let tfs_raw = _mm_loadl_epi64(term_freqs.as_ptr() as *const __m128i);
    let tfs_i32 = _mm256_cvtepu8_epi32(tfs_raw);
    let tf = _mm256_cvtepi32_ps(tfs_i32);

    // Load 8 contiguous f32 doc lengths.
    let dl = _mm256_loadu_ps(doc_lengths.as_ptr());

    // Broadcast scalar constants to all 8 lanes.
    let k1p1 = _mm256_set1_ps(k1_plus_1);
    let base = _mm256_set1_ps(denom_base);
    let dl_fac = _mm256_set1_ps(denom_dl_factor);
    let idf_v = _mm256_set1_ps(idf);

    // numerator = tf * k1_plus_1
    let num = _mm256_mul_ps(tf, k1p1);

    // denominator = tf + denom_base + denom_dl_factor * doc_len
    //             = tf + (denom_base + denom_dl_factor * doc_len)
    let dl_term = _mm256_mul_ps(dl_fac, dl);
    let base_plus_dl = _mm256_add_ps(base, dl_term);
    let denom = _mm256_add_ps(tf, base_plus_dl);

    // score = idf * (num / denom)
    let ratio = _mm256_div_ps(num, denom);
    let score = _mm256_mul_ps(idf_v, ratio);

    let mut result = [0.0f32; 8];
    _mm256_storeu_ps(result.as_mut_ptr(), score);
    result
}

/// AVX2 + FMA variant: uses fused multiply-add for the denominator.
///
/// `denom = tf + fma(denom_dl_factor, doc_len, denom_base)`
///
/// FMA provides a single-rounding result (vs two roundings for mul+add),
/// which may produce slightly different scores from the non-FMA path
/// (within f32 ULP). The performance difference is marginal since div_ps
/// dominates, but FMA is free when available and reduces instruction count.
///
/// # Safety
///
/// Requires both `avx2` and `fma` target features.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2", enable = "fma")]
#[inline]
// SAFETY: Callers must select this helper only after AVX2+FMA runtime detection.
// Fixed-size array parameters guarantee the eight lanes read by the intrinsics.
unsafe fn score_batch_avx2_fma(
    term_freqs: &[u8; 8],
    doc_lengths: &[f32; 8],
    idf: f32,
    k1_plus_1: f32,
    denom_base: f32,
    denom_dl_factor: f32,
) -> [f32; 8] {
    use std::arch::x86_64::*;

    let tfs_raw = _mm_loadl_epi64(term_freqs.as_ptr() as *const __m128i);
    let tfs_i32 = _mm256_cvtepu8_epi32(tfs_raw);
    let tf = _mm256_cvtepi32_ps(tfs_i32);

    let dl = _mm256_loadu_ps(doc_lengths.as_ptr());

    let k1p1 = _mm256_set1_ps(k1_plus_1);
    let base = _mm256_set1_ps(denom_base);
    let dl_fac = _mm256_set1_ps(denom_dl_factor);
    let idf_v = _mm256_set1_ps(idf);

    let num = _mm256_mul_ps(tf, k1p1);

    // FMA: denom_dl_factor * doc_len + denom_base (single rounding)
    let base_plus_dl = _mm256_fmadd_ps(dl_fac, dl, base);
    let denom = _mm256_add_ps(tf, base_plus_dl);

    let ratio = _mm256_div_ps(num, denom);
    let score = _mm256_mul_ps(idf_v, ratio);

    let mut result = [0.0f32; 8];
    _mm256_storeu_ps(result.as_mut_ptr(), score);
    result
}

/// Function pointer type for 8-wide batch scoring on x86_64.
///
/// Resolved once per term based on runtime CPU feature detection,
/// avoiding repeated `is_x86_feature_detected!` checks in the hot loop.
#[cfg(target_arch = "x86_64")]
// SAFETY: Values of this type are only produced by `select_score_batch_8`,
// which pairs each unsafe target-feature function with matching CPU detection.
type ScoreBatch8Fn = unsafe fn(&[u8; 8], &[f32; 8], f32, f32, f32, f32) -> [f32; 8];

/// Select the best 8-wide scoring function for the current CPU.
///
/// Priority: AVX2+FMA > AVX2 > scalar fallback.
/// Called once per term, the returned function pointer is used for all
/// batches within that term's posting list.
#[cfg(target_arch = "x86_64")]
#[inline]
fn select_score_batch_8() -> ScoreBatch8Fn {
    if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
        score_batch_avx2_fma
    } else if is_x86_feature_detected!("avx2") {
        score_batch_avx2
    } else {
        // Scalar fallback when no AVX2.
        |tfs, dls, idf, k1p1, base, dl_fac| score_batch_scalar_8(tfs, dls, idf, k1p1, base, dl_fac)
    }
}

/// Dispatch batch scoring to the appropriate 4-wide implementation.
///
/// On aarch64 uses NEON SIMD; on other architectures uses scalar f32.
#[inline]
fn score_batch_4(
    term_freqs: &[u8; 4],
    doc_lengths: &[f32; 4],
    idf: f32,
    k1_plus_1: f32,
    denom_base: f32,
    denom_dl_factor: f32,
) -> [f32; 4] {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: We are on aarch64 (checked by cfg). NEON is baseline on all
        // AArch64 CPUs (ARMv8-A mandates Advanced SIMD). The input slices are
        // [T; 4] arrays so alignment and length are guaranteed.
        unsafe {
            score_batch_neon(
                term_freqs,
                doc_lengths,
                idf,
                k1_plus_1,
                denom_base,
                denom_dl_factor,
            )
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        score_batch_scalar_4(
            term_freqs,
            doc_lengths,
            idf,
            k1_plus_1,
            denom_base,
            denom_dl_factor,
        )
    }
}

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

struct TermCursor<'a> {
    postings: &'a PostingList,
    blocks: &'a [BlockMaxBlock],
    pos: usize,
    block_size: usize,
    scorer: Bm25TermScorer,
}

impl<'a> TermCursor<'a> {
    #[inline]
    fn new(
        postings: &'a PostingList,
        blocks: &'a [BlockMaxBlock],
        block_size: usize,
        scorer: Bm25TermScorer,
    ) -> Self {
        Self {
            postings,
            blocks,
            pos: 0,
            block_size,
            scorer,
        }
    }

    #[inline]
    fn is_terminated(&self) -> bool {
        self.pos >= self.postings.len()
    }

    #[inline]
    fn doc(&self) -> u32 {
        if self.pos < self.postings.doc_ids.len() {
            self.postings.doc_ids[self.pos]
        } else {
            TERMINATED_DOC
        }
    }

    #[inline]
    fn current_doc_id(&self) -> u32 {
        self.postings.doc_ids[self.pos]
    }

    #[inline]
    fn current_term_freq(&self) -> u8 {
        self.postings.term_freqs[self.pos]
    }

    #[inline]
    fn current_block_idx(&self) -> Option<usize> {
        if self.is_terminated() {
            None
        } else {
            Some(self.pos / self.block_size)
        }
    }

    #[inline]
    fn remaining_max_score(&self) -> f64 {
        self.current_block_idx()
            .and_then(|idx| self.blocks.get(idx))
            .map(|block| block.suffix_max_score)
            .unwrap_or(0.0)
    }

    #[inline]
    fn advance(&mut self) -> u32 {
        if !self.is_terminated() {
            self.pos += 1;
        }
        self.doc()
    }

    #[inline]
    fn seek(&mut self, target_doc: u32) -> u32 {
        if self.is_terminated() {
            return TERMINATED_DOC;
        }
        if self.doc() >= target_doc {
            return self.doc();
        }

        let rel = self.postings.doc_ids[self.pos..].partition_point(|&id| id < target_doc);
        self.pos += rel;
        self.doc()
    }

    #[inline]
    fn shallow_block_info(&self, target_doc: u32) -> Option<ShallowBlockInfo> {
        let current_block_idx = self.current_block_idx()?;
        let rel =
            self.blocks[current_block_idx..].partition_point(|block| block.max_doc_id < target_doc);
        let block = self.blocks.get(current_block_idx + rel)?;
        Some(ShallowBlockInfo {
            max_score: block.max_score_contribution,
            last_doc: block.max_doc_id,
        })
    }

    #[inline]
    fn score_current(&self, index: &Bm25Index) -> f64 {
        if self.is_terminated() {
            return 0.0;
        }
        let doc_id = self.current_doc_id();
        let term_freq = self.current_term_freq();
        let doc_length = index.doc_length_fast(doc_id);
        self.scorer.score(term_freq, doc_length)
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
    /// **PROOF CORRESPONDENCE**: `Lion.Retrieval.BM25.bm25_nonneg`
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
            Ok(guard) if guard.built_epoch == self.postings_epoch => guard,
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
    /// **PROOF CORRESPONDENCE**: `Lion.Retrieval.BM25.tf_bounded`
    /// TF saturation: tf * (k1 + 1) / (tf + k1 * ...) < k1 + 1 for all tf >= 0.
    pub(crate) fn search_brute_force(
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
    /// **PROOF CORRESPONDENCE**: `Lion.Retrieval.BM25.idf_nonneg`
    /// With +1 inside ln(), IDF(t) >= 0 for all terms regardless of document frequency.
    ///
    /// **PROOF CORRESPONDENCE**: `Lion.Retrieval.BM25.idf_mono`
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

fn heap_to_results(
    index: &Bm25Index,
    ctx: &mut SearchContext,
) -> Vec<(Arc<str>, DeterministicScore)> {
    ctx.results_buf.clear();

    while let Some(Reverse(entry)) = ctx.heap.pop() {
        ctx.results_buf.push((entry.doc_id, entry.score));
    }

    ctx.results_buf
        .sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    ctx.results_buf
        .iter()
        .filter_map(|(internal_id, score)| {
            // resolve_internal_id returns Arc<str>; clone = atomic refcount bump.
            let doc_id = index.resolve_internal_id(*internal_id)?;
            Some((doc_id, DeterministicScore::from_f64(*score)))
        })
        .collect()
}

fn current_threshold_score(heap: &BinaryHeap<Reverse<HeapEntry>>, k: usize) -> f64 {
    if heap.len() < k {
        0.0
    } else {
        heap.peek().map(|entry| entry.0.score).unwrap_or(0.0)
    }
}

fn maybe_push_top_k(heap: &mut BinaryHeap<Reverse<HeapEntry>>, k: usize, candidate: HeapEntry) {
    if k == 0 {
        return;
    }

    if heap.len() < k {
        heap.push(Reverse(candidate));
        return;
    }

    let should_replace = heap.peek().map(|worst| candidate > worst.0).unwrap_or(true);
    if should_replace {
        let _ = heap.pop();
        heap.push(Reverse(candidate));
    }
}

fn find_pivot_doc(cursors: &[TermCursor<'_>], threshold: f64) -> Option<(usize, usize, u32)> {
    let mut upper_bound_sum = 0.0;
    let mut before_pivot_len = 0usize;
    let mut pivot_doc = TERMINATED_DOC;

    while before_pivot_len < cursors.len() {
        upper_bound_sum += cursors[before_pivot_len].remaining_max_score();
        if upper_bound_sum >= threshold {
            pivot_doc = cursors[before_pivot_len].doc();
            break;
        }
        before_pivot_len += 1;
    }

    if pivot_doc == TERMINATED_DOC {
        return None;
    }

    let mut pivot_len = before_pivot_len + 1;
    while pivot_len < cursors.len() && cursors[pivot_len].doc() == pivot_doc {
        pivot_len += 1;
    }

    Some((before_pivot_len, pivot_len, pivot_doc))
}

fn align_cursors(
    cursors: &mut Vec<TermCursor<'_>>,
    pivot_doc: u32,
    before_pivot_len: usize,
) -> bool {
    debug_assert_ne!(pivot_doc, TERMINATED_DOC);

    for idx in (0..before_pivot_len).rev() {
        let new_doc = cursors[idx].seek(pivot_doc);
        if new_doc != pivot_doc {
            sort_and_prune_terminated(cursors);
            return false;
        }
    }

    true
}

fn advance_all_cursors_on_pivot(cursors: &mut Vec<TermCursor<'_>>, pivot_len: usize) {
    for cursor in &mut cursors[..pivot_len] {
        cursor.advance();
    }
    sort_and_prune_terminated(cursors);
}

/// Advance one cursor past the current block when the block-level upper
/// bound is below the threshold.
///
/// Selects the cursor whose current block ends **earliest** (minimum
/// `last_doc`) among the pivot cursors. This minimizes skip distance and
/// is the correct BMW cursor selection strategy -- advancing past the
/// smallest block boundary guarantees forward progress with minimal
/// overshoot. The seek target is that earliest block end + 1, bounded
/// by the smallest doc_id among non-pivot cursors so we do not overshoot
/// documents that other cursors still reference.
fn advance_one_cursor_past_block(
    cursors: &mut Vec<TermCursor<'_>>,
    pivot_len: usize,
    pivot_doc: u32,
) {
    let mut cursor_to_seek = None;
    let mut earliest_block_end = TERMINATED_DOC;
    let mut doc_to_seek_after = TERMINATED_DOC;

    for (idx, cursor) in cursors[..pivot_len].iter().enumerate() {
        if let Some(info) = cursor.shallow_block_info(pivot_doc) {
            if info.last_doc < doc_to_seek_after {
                doc_to_seek_after = info.last_doc;
            }
            // Select the cursor with the earliest block end (minimum last_doc).
            // This minimizes skip distance for optimal BMW pruning.
            if info.last_doc < earliest_block_end {
                earliest_block_end = info.last_doc;
                cursor_to_seek = Some(idx);
            }
        }
    }

    if doc_to_seek_after != TERMINATED_DOC {
        doc_to_seek_after = doc_to_seek_after.saturating_add(1);
    }

    for cursor in &cursors[pivot_len..] {
        let doc = cursor.doc();
        if doc < doc_to_seek_after {
            doc_to_seek_after = doc;
        }
    }

    if let Some(idx) = cursor_to_seek {
        // Ensure forward progress: if the non-pivot cap reduced doc_to_seek_after
        // to at or below the cursor's current position, the seek would be a no-op.
        // This can happen when a non-pivot cursor points to a doc_id smaller than
        // the block-end target (e.g., a short posting list cursor at doc 3 while
        // the chosen cursor is already at doc 150). Force at least +1 advance.
        let current_doc = cursors[idx].doc();
        if doc_to_seek_after <= current_doc {
            doc_to_seek_after = current_doc.saturating_add(1);
        }
        cursors[idx].seek(doc_to_seek_after);
    }

    sort_and_prune_terminated(cursors);
}

fn sort_and_prune_terminated(cursors: &mut Vec<TermCursor<'_>>) {
    cursors.retain(|cursor| !cursor.is_terminated());
    cursors.sort_by_key(|cursor| cursor.doc());
}

// ---------------------------------------------------------------------------
// Tests: SIMD batch scoring parity and edge cases
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests_simd_scoring {
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
