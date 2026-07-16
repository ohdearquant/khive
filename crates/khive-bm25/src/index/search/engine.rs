//! BM25 search engine: brute-force SIMD and Block-Max WAND paths.

use std::sync::Arc;

use khive_score::DeterministicScore;

use super::super::{Bm25Index, Bm25TermScorer};
use crate::metrics::{self, MetricEvent, MetricValue};

use super::context::{HeapEntry, SearchContext};
use super::cursor::TermCursor;
use super::helpers::{
    advance_all_cursors_on_pivot, advance_one_cursor_past_block, align_cursors,
    current_threshold_score, find_pivot_doc, heap_to_results, maybe_push_top_k,
    sort_and_prune_terminated,
};
use super::simd::score_batch_4;
#[cfg(target_arch = "x86_64")]
use super::simd::select_score_batch_8;

/// Posting count below which brute-force SIMD is used instead of Block-Max WAND.
const SMALL_QUERY_POSTINGS_THRESHOLD: usize = 16_384;

impl Bm25Index {
    /// Return at most `k` matches by descending score and deterministic ID tie-break.
    ///
    /// Empty queries and `k == 0` return no results. See `crates/khive-bm25/docs/api/search.md`.
    pub fn search(&self, query_text: &str, k: usize) -> Vec<(Arc<str>, DeterministicScore)> {
        let mut ctx = SearchContext::new();
        self.search_with_context(query_text, k, &mut ctx)
    }

    /// Search with reusable scratch storage; the context is cleared but retains capacity.
    ///
    /// See `crates/khive-bm25/docs/api/search.md`.
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

    /// Routes to brute-force for small queries and Block-Max WAND for larger ones.
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

    /// Exhaustively score all matching postings with platform-selected SIMD or scalar code.
    ///
    /// Results use the same ordering as [`Self::search`]. See `crates/khive-bm25/docs/api/search.md`.
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

        let max_id = self.next_internal_id as usize;
        if ctx.score_vec.len() < max_id {
            ctx.score_vec.resize(max_id, 0.0);
        }

        let avgdl = self.avg_doc_length();
        let k1 = self.config.k1;
        let b = self.config.b;

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

            let simd_idf = scorer.idf_f32();
            let simd_k1p1 = scorer.k1_plus_1_f32();
            let simd_base = scorer.denom_base_f32();
            let simd_dl_fac = scorer.denom_dl_factor_f32();

            let n = postings.len();
            let doc_ids = &postings.doc_ids;
            let tfs_arr = &postings.term_freqs;

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

        ctx.results_buf.clear();
        for &doc_id in &ctx.touched_docs {
            let score = ctx.score_vec[doc_id as usize];
            if score > 0.0 {
                ctx.results_buf.push((doc_id, score));
            }
        }

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
                // Arc::clone is a refcount bump — no heap allocation.
                let doc_id = self.resolve_internal_id(*internal_id)?;
                Some((doc_id, DeterministicScore::from_f64(*score)))
            })
            .collect()
    }
}
