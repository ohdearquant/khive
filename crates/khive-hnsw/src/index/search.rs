use crate::NodeId;
use khive_score::DeterministicScore;

use super::HnswIndex;
use crate::config::DistanceMetric;
use crate::distance::{cosine_distance_from_parts, score_from_distance, OrderedF32};
use crate::error::{validate_finite_vector, Result, RetrievalError};
use crate::metrics::{self, MetricEvent, MetricValue};
use crate::search_context::HnswSearchContext;

/// Index size below which exact linear scan beats graph traversal for Cosine/Dot.
/// Empirically measured crossover for dim=384, ef_search=80: HNSW graph ≈ exact scan at ~4K nodes.
/// Using 3K keeps us firmly in the "exact wins" zone while leaving 5K+ to the graph.
const EXACT_SCAN_THRESHOLD: usize = 3_000;

// ---------------------------------------------------------------------------
// Inlined distance function type
// ---------------------------------------------------------------------------

/// Distance function signature: `(query, query_norm, vector, vector_norm) -> distance`.
/// Resolved once per search to avoid per-neighbor `match` dispatch.
type DistanceFn = fn(&[f32], f32, &[f32], f32) -> f32;

/// Resolve `DistanceMetric` to a concrete function pointer once per search.
#[inline]
fn resolve_distance_fn(metric: DistanceMetric) -> DistanceFn {
    match metric {
        DistanceMetric::Cosine => |a, a_norm, b, b_norm| {
            let dot = lattice_embed::simd::dot_product(a, b);
            cosine_distance_from_parts(dot, a_norm, b_norm)
        },
        DistanceMetric::Dot => |a, _a_norm, b, _b_norm| -lattice_embed::simd::dot_product(a, b),
        DistanceMetric::L2 => {
            |a, _a_norm, b, _b_norm| lattice_embed::simd::squared_euclidean_distance(a, b)
        }
        // Fall back to cosine for future variants.
        _ => |a, a_norm, b, b_norm| {
            let dot = lattice_embed::simd::dot_product(a, b);
            cosine_distance_from_parts(dot, a_norm, b_norm)
        },
    }
}

// ---------------------------------------------------------------------------
// Batch-4 distance helpers (query-vs-4-candidates HNSW fast path)
// ---------------------------------------------------------------------------

/// Check if a cached sqrt norm is ≈ 1.0 (1e-4 threshold on the squared norm).
#[inline]
fn cached_norm_is_unit(norm: f32) -> bool {
    norm.is_finite() && ((norm * norm) - 1.0).abs() < 1e-4
}

/// Convert four dot products to HNSW distances using cached candidate norms.
/// Unit-norm shortcut for Cosine; negates for Dot (min-distance ordering).
#[inline]
fn hnsw_distance_batch4_from_dots(
    metric: DistanceMetric,
    dots: [f32; 4],
    query_norm: f32,
    query_is_unit: bool,
    norms: [f32; 4],
) -> [f32; 4] {
    match metric {
        DistanceMetric::Cosine => {
            let mut out = [0.0f32; 4];
            for j in 0..4 {
                out[j] = if query_is_unit && cached_norm_is_unit(norms[j]) {
                    1.0 - dots[j].clamp(-1.0, 1.0)
                } else {
                    cosine_distance_from_parts(dots[j], query_norm, norms[j])
                };
            }
            out
        }
        DistanceMetric::Dot => [-dots[0], -dots[1], -dots[2], -dots[3]],
        _ => unreachable!("hnsw_distance_batch4_from_dots: unexpected metric"),
    }
}

// ---------------------------------------------------------------------------
// Software prefetch helpers
// ---------------------------------------------------------------------------

/// Prefetch a memory region into L1 data cache.
/// Advisory hint — hardware silently ignores invalid addresses; pointer need not be valid.
#[inline(always)]
fn prefetch_read_data(ptr: *const f32) {
    #[cfg(target_arch = "aarch64")]
    {
        // PRFM PLDL1KEEP: prefetch for load, L1 data cache, temporal
        // SAFETY: `PRFM` is an advisory hint instruction — the hardware silently
        // ignores invalid or unmapped addresses. No validity guarantee on `ptr`
        // is required; the instruction cannot fault. The `nostack` and
        // `preserves_flags` options ensure the asm does not clobber the stack
        // pointer or NZCV flags.
        unsafe {
            core::arch::asm!(
                "prfm pldl1keep, [{x}]",
                x = in(reg) ptr,
                options(nostack, preserves_flags)
            );
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `_mm_prefetch` is a prefetch hint — the CPU silently ignores
        // invalid or unmapped addresses and the instruction cannot fault.
        // No alignment or validity requirement on `ptr` is imposed by the x86
        // ISA for prefetch instructions.
        unsafe {
            core::arch::x86_64::_mm_prefetch(ptr as *const i8, core::arch::x86_64::_MM_HINT_T0);
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = ptr;
    }
}

impl HnswIndex {
    /// Search for k nearest neighbors sorted by descending score; tombstones filtered automatically.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(NodeId, DeterministicScore)>> {
        let start = std::time::Instant::now();

        let result = self.search_inner(query, k);

        // Emit metrics
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_SEARCH_DURATION_MS,
                value: MetricValue::Histogram(elapsed),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_SEARCH_COUNT,
                value: MetricValue::Counter(1),
                labels: vec![],
            },
        );
        if let Ok(ref results) = result {
            metrics::emit(
                &self.metrics,
                MetricEvent {
                    name: metrics::names::HNSW_SEARCH_RESULTS,
                    value: MetricValue::Gauge(results.len() as f64),
                    labels: vec![],
                },
            );
        }

        result
    }

    /// Search using a pre-allocated context to avoid per-query heap allocation.
    pub fn search_with_context(
        &self,
        query: &[f32],
        k: usize,
        ctx: &mut HnswSearchContext,
    ) -> Result<Vec<(NodeId, DeterministicScore)>> {
        let start = std::time::Instant::now();

        let result = self.search_inner_with_ctx(query, k, ctx);

        // Emit metrics
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_SEARCH_DURATION_MS,
                value: MetricValue::Histogram(elapsed),
                labels: vec![],
            },
        );
        metrics::emit(
            &self.metrics,
            MetricEvent {
                name: metrics::names::HNSW_SEARCH_COUNT,
                value: MetricValue::Counter(1),
                labels: vec![],
            },
        );
        if let Ok(ref results) = result {
            metrics::emit(
                &self.metrics,
                MetricEvent {
                    name: metrics::names::HNSW_SEARCH_RESULTS,
                    value: MetricValue::Gauge(results.len() as f64),
                    labels: vec![],
                },
            );
        }

        result
    }

    /// Inner search logic (uninstrumented), allocating fresh buffers.
    fn search_inner(&self, query: &[f32], k: usize) -> Result<Vec<(NodeId, DeterministicScore)>> {
        let ef = self.config.ef_search.max(k);
        let mut ctx = HnswSearchContext::new(ef);
        self.search_inner_with_ctx(query, k, &mut ctx)
    }

    /// Inner search logic using a caller-provided search context.
    fn search_inner_with_ctx(
        &self,
        query: &[f32],
        k: usize,
        ctx: &mut HnswSearchContext,
    ) -> Result<Vec<(NodeId, DeterministicScore)>> {
        if query.len() != self.config.dimensions {
            return Err(RetrievalError::DimensionMismatch {
                expected: self.config.dimensions,
                actual: query.len(),
            });
        }
        validate_finite_vector(query)?;

        if self.nodes.is_empty() {
            return Ok(Vec::new());
        }

        // H3: exact scan beats graph traversal for small Cosine/Dot indexes.
        if self.nodes.len() <= EXACT_SCAN_THRESHOLD
            && matches!(
                self.config.metric,
                DistanceMetric::Cosine | DistanceMetric::Dot
            )
        {
            return self.exact_scan_top_k(query, k);
        }

        let entry_point = match self.entry_point {
            Some(ep) => ep,
            None => return Ok(Vec::new()),
        };

        // The entry point should always be live because `delete()` calls
        // `repair_entry_point_after_delete` to maintain this invariant.
        // The check below is a defensive fallback for indexes that were
        // constructed before this fix or restored from old snapshots.
        let effective_entry = if self.is_tombstoned(entry_point) {
            match self.find_live_neighbor(entry_point) {
                Some(alt) => alt,
                None => return Ok(Vec::new()), // All nodes are tombstoned
            }
        } else {
            entry_point
        };

        self.search_from_entry_with_ctx(query, k, effective_entry, ctx)
    }

    /// Search from a specific entry point with tombstone filtering, using pre-allocated context.
    fn search_from_entry_with_ctx(
        &self,
        query: &[f32],
        k: usize,
        entry_point: usize,
        ctx: &mut HnswSearchContext,
    ) -> Result<Vec<(NodeId, DeterministicScore)>> {
        let query_norm = query.iter().map(|x| x * x).sum::<f32>().sqrt();
        let (effective_k, effective_ef) = self.compute_overscan(k);

        // Search from top layer
        let mut current_nearest = vec![entry_point];

        // Traverse upper layers (greedy search with ef=1)
        for l in (1..=self.max_level).rev() {
            self.search_layer_inner_ctx(query, query_norm, &current_nearest, 1, l, false, ctx);
            if !ctx.result_buf.is_empty() {
                current_nearest = vec![ctx.result_buf[0].1];
            }
        }

        // If final entry point is tombstoned after upper-layer traversal,
        // find a live neighbor. This is rare since the entry point invariant
        // ensures we start from a live node, but upper-layer greedy search
        // can land on a tombstoned node if it was tombstoned after insertion.
        let final_entry = current_nearest[0];
        if self.is_tombstoned(final_entry) {
            match self.find_live_neighbor(final_entry) {
                Some(alt) => current_nearest = vec![alt],
                None => return Ok(Vec::new()), // All nodes tombstoned
            }
        }

        // Search layer 0 with full ef, filtering tombstones.
        // effective_ef is already scaled up by the tombstone ratio.
        let ef = effective_ef.max(effective_k);
        self.search_layer_inner_ctx(query, query_norm, &current_nearest, ef, 0, true, ctx);

        // Convert internal IDs to external NodeId with DeterministicScore.
        // For L2, the internal search used squared_euclidean_distance for ordering;
        // recover the true L2 distance before converting to similarity.
        let is_l2 = self.config.metric == DistanceMetric::L2;
        let search_results: Vec<(NodeId, DeterministicScore)> = ctx
            .result_buf
            .iter()
            .filter(|(_, iid)| !self.is_tombstoned(*iid))
            .take(k)
            .map(|(dist, iid)| {
                let true_dist = if is_l2 { dist.max(0.0).sqrt() } else { *dist };
                (
                    self.external_id(*iid),
                    score_from_distance(true_dist, self.config.metric),
                )
            })
            .collect();

        Ok(search_results)
    }

    /// Exact linear scan for small indexes (n <= EXACT_SCAN_THRESHOLD) using batch-4 SIMD dot.
    /// Cosine and Dot metrics only — enforced by the early-exit in `search_inner_with_ctx`.
    fn exact_scan_top_k(
        &self,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(NodeId, DeterministicScore)>> {
        if k == 0 {
            return Ok(Vec::new());
        }

        let dot4 = lattice_embed::simd::resolved_dot_product_batch4_kernel();
        let dot1 = lattice_embed::simd::resolved_dot_product_kernel();

        let query_norm = query.iter().map(|x| x * x).sum::<f32>().sqrt();
        let query_is_unit = cached_norm_is_unit(query_norm);
        let metric = self.config.metric;
        let n = self.nodes.len();

        let mut scored: Vec<(usize, DeterministicScore)> = Vec::with_capacity(n);
        let mut i = 0usize;

        while i + 4 <= n {
            let dots = dot4(
                query,
                &self.nodes[i].vector,
                &self.nodes[i + 1].vector,
                &self.nodes[i + 2].vector,
                &self.nodes[i + 3].vector,
            );
            let norms = [
                self.nodes[i].norm,
                self.nodes[i + 1].norm,
                self.nodes[i + 2].norm,
                self.nodes[i + 3].norm,
            ];
            let dists =
                hnsw_distance_batch4_from_dots(metric, dots, query_norm, query_is_unit, norms);
            for (j, &dist) in dists.iter().enumerate() {
                if !self.is_tombstoned(i + j) {
                    scored.push((i + j, score_from_distance(dist, metric)));
                }
            }
            i += 4;
        }
        while i < n {
            if !self.is_tombstoned(i) {
                let dot = dot1(query, &self.nodes[i].vector);
                let dist = if query_is_unit && cached_norm_is_unit(self.nodes[i].norm) {
                    1.0 - dot.clamp(-1.0, 1.0)
                } else {
                    match metric {
                        DistanceMetric::Cosine => {
                            cosine_distance_from_parts(dot, query_norm, self.nodes[i].norm)
                        }
                        DistanceMetric::Dot => -dot,
                        _ => unreachable!(),
                    }
                };
                scored.push((i, score_from_distance(dist, metric)));
            }
            i += 1;
        }

        if scored.is_empty() {
            return Ok(Vec::new());
        }

        let effective_k = k.min(scored.len());
        if scored.len() > effective_k {
            // Tie-break by external NodeId to match graph-search determinism.
            scored.select_nth_unstable_by(effective_k - 1, |(iid_a, a), (iid_b, b)| {
                match b.cmp(a) {
                    std::cmp::Ordering::Equal => {
                        self.external_id(*iid_a).cmp(&self.external_id(*iid_b))
                    }
                    other => other,
                }
            });
            scored.truncate(effective_k);
        }
        // Sort descending by score; equal scores broken by external NodeId (matches graph search).
        scored.sort_by(|(iid_a, a), (iid_b, b)| match b.cmp(a) {
            std::cmp::Ordering::Equal => self.external_id(*iid_a).cmp(&self.external_id(*iid_b)),
            other => other,
        });

        Ok(scored
            .into_iter()
            .map(|(iid, score)| (self.external_id(iid), score))
            .collect())
    }

    /// Find a live (non-tombstoned) neighbor node; O(M·L) typical, O(N) fallback only when all neighbors are dead.
    fn find_live_neighbor(&self, node_id: usize) -> Option<usize> {
        let node = &self.nodes[node_id];
        // Check neighbors from highest layer down (higher layers have better
        // graph coverage, so finding a live neighbor there is preferable).
        for layer in (0..node.neighbors.len()).rev() {
            for &neighbor_id in &node.neighbors[layer] {
                if !self.is_tombstoned(neighbor_id) {
                    return Some(neighbor_id);
                }
            }
        }

        // Extremely rare fallback: all neighbors tombstoned. O(N) scan.
        (0..self.nodes.len()).find(|&iid| !self.is_tombstoned(iid))
    }

    /// Compute overscan factors `(effective_k, effective_ef)` scaled by tombstone ratio.
    /// Both k and ef_search are scaled so beam width compensates for dead nodes in traversal.
    pub(super) fn compute_overscan(&self, k: usize) -> (usize, usize) {
        let stats = self.tombstone_stats();
        if stats.tombstone_count == 0 {
            return (k, self.config.ef_search);
        }

        let live_ratio = stats.live_nodes as f64 / stats.total_nodes.max(1) as f64;
        if live_ratio <= 0.0 {
            return (k, self.config.ef_search);
        }

        let inv_live = 1.0 / live_ratio;

        // Use saturating arithmetic to avoid overflow when inv_live is very large.
        // Cap at k * 4 and ef_search * 4 using saturating_mul.
        let overscan_k = (k as f64 * inv_live).ceil() as usize;
        let k_cap = k.saturating_mul(4);
        let effective_k = overscan_k.min(k_cap).max(k);

        let overscan_ef = (self.config.ef_search as f64 * inv_live).ceil() as usize;
        let ef_cap = self.config.ef_search.saturating_mul(4);
        let effective_ef = overscan_ef.min(ef_cap).max(self.config.ef_search);

        (effective_k, effective_ef)
    }

    /// Search a single layer for nearest neighbors; allocates fresh buffers.
    /// Returns `(distance, internal_id)` pairs.
    pub(crate) fn search_layer(
        &self,
        query: &[f32],
        query_norm: f32,
        entry_points: &[usize],
        ef: usize,
        layer: usize,
    ) -> Vec<(f32, usize)> {
        self.search_layer_inner(query, query_norm, entry_points, ef, layer, false)
    }

    /// Internal search with tombstone filtering option; allocates fresh buffers for the insert path.
    pub(super) fn search_layer_inner(
        &self,
        query: &[f32],
        query_norm: f32,
        entry_points: &[usize],
        ef: usize,
        layer: usize,
        filter_tombstones: bool,
    ) -> Vec<(f32, usize)> {
        let mut ctx = HnswSearchContext::new(ef);
        self.search_layer_inner_ctx(
            query,
            query_norm,
            entry_points,
            ef,
            layer,
            filter_tombstones,
            &mut ctx,
        );
        // Move out of ctx to avoid clone
        std::mem::take(&mut ctx.result_buf)
    }

    /// Core search using pre-allocated buffers; writes sorted results into `ctx.result_buf`.
    // REASON: argument count reflects distinct algorithm degrees of freedom; wrapper struct adds hot-path indirection.
    #[allow(clippy::too_many_arguments)]
    fn search_layer_inner_ctx(
        &self,
        query: &[f32],
        query_norm: f32,
        entry_points: &[usize],
        ef: usize,
        layer: usize,
        filter_tombstones: bool,
        ctx: &mut HnswSearchContext,
    ) {
        // Reset buffers without deallocating
        ctx.clear();
        ctx.ensure_capacity(ef, self.nodes.len());

        // Pre-compute: skip tombstone checks when there are no tombstones.
        let check_tombstones = filter_tombstones && self.tombstone_count > 0;

        // Resolve distance function once -- eliminates per-neighbor match dispatch.
        let distance_fn = resolve_distance_fn(self.config.metric);

        // Resolve batch-4 dot kernel once for Cosine/Dot metrics.
        let metric = self.config.metric;
        let use_dot_batch4 = matches!(metric, DistanceMetric::Cosine | DistanceMetric::Dot);
        let dot4_kernel = lattice_embed::simd::resolved_dot_product_batch4_kernel();
        let query_is_unit = cached_norm_is_unit(query_norm);

        // ---------------------------------------------------------------
        // INT8 quantized pre-filter setup (only for Cosine on layer 0)
        // ---------------------------------------------------------------
        // The quantized path is only used when:
        // 1. use_quantized is enabled
        // 2. We're on layer 0 (densest layer, most distance computations)
        // 3. The metric is Cosine (the only one we have INT8 distance for)
        // 4. The quantized arena is populated
        //
        // For upper layers (ef=1, greedy), the overhead of quantization is
        // not worthwhile since we evaluate very few candidates.
        let use_quant = self.use_quantized
            && layer == 0
            && self.config.metric == DistanceMetric::Cosine
            && !self.quantized.meta.is_empty();

        // Pre-quantize the query vector once if we're using the INT8 path.
        // This avoids re-quantizing per neighbor.
        let (query_i8, query_scale) = if use_quant {
            let mut max_abs: f32 = 0.0;
            for &v in query {
                if v.is_finite() {
                    let abs = v.abs();
                    if abs > max_abs {
                        max_abs = abs;
                    }
                }
            }
            let scale = if max_abs > 1e-10 {
                127.0 / max_abs
            } else {
                1.0
            };
            let quantized: Vec<i8> = query
                .iter()
                .map(|&v| {
                    if v.is_finite() {
                        (v * scale).round().clamp(-127.0, 127.0) as i8
                    } else {
                        0i8
                    }
                })
                .collect();
            (quantized, scale)
        } else {
            (Vec::new(), 0.0)
        };

        // Mark entry points as visited
        ctx.visited.visit_all(entry_points.iter().copied());

        // Initialize with entry points (always use f32 for entry points)
        for &ep in entry_points {
            if check_tombstones && self.is_tombstoned(ep) {
                continue;
            }
            let node = &self.nodes[ep];
            let dist = distance_fn(query, query_norm, &node.vector, node.norm);
            ctx.candidates
                .push(std::cmp::Reverse((OrderedF32(dist), ep)));
            ctx.results.push((OrderedF32(dist), ep));
        }

        // Track the worst distance in the result set to avoid heap peek per neighbor.
        let mut worst_dist = ctx
            .results
            .peek()
            .map(|(OrderedF32(d), _)| *d)
            .unwrap_or(f32::MAX);

        // Scratch buffer for batching neighbor processing.
        // Each entry: (internal_id, vector_ptr, vector_len, norm).
        let mut batch: Vec<(usize, *const f32, usize, f32)> = Vec::with_capacity(32);

        while let Some(std::cmp::Reverse((OrderedF32(c_dist), c_id))) = ctx.candidates.pop() {
            // Early termination: if the closest candidate is worse than the
            // worst result and we have enough results, we're done.
            if c_dist > worst_dist && ctx.results.len() >= ef {
                break;
            }

            // Explore neighbors -- direct array index, no HashMap lookup
            let node = &self.nodes[c_id];
            if layer < node.neighbors.len() {
                let neighbors = &node.neighbors[layer];

                // Phase 1: Collect unvisited neighbors.
                // O(1) visited check via generation counter, O(1) node access via array index.
                batch.clear();
                for &neighbor_id in neighbors {
                    if ctx.visited.visit(neighbor_id) {
                        if check_tombstones && self.is_tombstoned(neighbor_id) {
                            continue;
                        }

                        // INT8 pre-filter: skip neighbors that are clearly worse
                        // than the current worst result. Uses a 10% margin to
                        // account for quantization error.
                        if use_quant && ctx.results.len() >= ef {
                            let approx_dist = self.quantized.cosine_distance_approx(
                                neighbor_id,
                                &query_i8,
                                query_scale,
                                query_norm,
                            );
                            // Only skip if the approximate distance exceeds the
                            // worst by more than the quantization margin (10%).
                            // This ensures we never miss a true nearest neighbor.
                            if approx_dist > worst_dist * 1.1 + 0.01 {
                                continue;
                            }
                        }

                        let neighbor = &self.nodes[neighbor_id];
                        batch.push((
                            neighbor_id,
                            neighbor.vector.as_ptr(),
                            neighbor.vector.len(),
                            neighbor.norm,
                        ));
                    }
                }

                // Phase 2: Compute f32 distances with batch-4 SIMD + prefetch pipelining.
                //
                // For Cosine/Dot: process 4 candidates at once via the batch-4 dot kernel,
                // converting raw dots to HNSW distances (with unit-norm shortcut for cosine).
                // Remainder (< 4) and L2/other metrics use the per-pair distance_fn path.
                //
                // Heap updates are applied in original batch order to preserve HNSW recall
                // and deterministic neighbor ordering.
                if let Some(&(_, ptr, _, _)) = batch.first() {
                    prefetch_read_data(ptr);
                }

                let mut bi = 0;

                // Batch-4 fast path (Cosine / Dot metrics only).
                if use_dot_batch4 {
                    while bi + 4 <= batch.len() {
                        // Prefetch the entry 4 slots ahead to hide memory latency.
                        if bi + 4 < batch.len() {
                            prefetch_read_data(batch[bi + 4].1);
                        }

                        let (id0, p0, l0, n0) = batch[bi];
                        let (id1, p1, l1, n1) = batch[bi + 1];
                        let (id2, p2, l2, n2) = batch[bi + 2];
                        let (id3, p3, l3, n3) = batch[bi + 3];

                        if l0 == query.len()
                            && l1 == query.len()
                            && l2 == query.len()
                            && l3 == query.len()
                        {
                            // SAFETY: pointers from live &Vec<f32> within this &self borrow.
                            let v0 = unsafe { std::slice::from_raw_parts(p0, l0) };
                            let v1 = unsafe { std::slice::from_raw_parts(p1, l1) };
                            let v2 = unsafe { std::slice::from_raw_parts(p2, l2) };
                            let v3 = unsafe { std::slice::from_raw_parts(p3, l3) };

                            let dots = dot4_kernel(query, v0, v1, v2, v3);
                            let dists = hnsw_distance_batch4_from_dots(
                                metric,
                                dots,
                                query_norm,
                                query_is_unit,
                                [n0, n1, n2, n3],
                            );

                            for (neighbor_id, dist) in [
                                (id0, dists[0]),
                                (id1, dists[1]),
                                (id2, dists[2]),
                                (id3, dists[3]),
                            ] {
                                if !(ctx.results.len() >= ef && dist > worst_dist) {
                                    ctx.candidates
                                        .push(std::cmp::Reverse((OrderedF32(dist), neighbor_id)));
                                    ctx.results.push((OrderedF32(dist), neighbor_id));
                                    if ctx.results.len() > ef {
                                        ctx.results.pop();
                                    }
                                    if let Some(&(OrderedF32(d), _)) = ctx.results.peek() {
                                        worst_dist = d;
                                    }
                                }
                            }
                            bi += 4;
                            continue;
                        }

                        // Dimension mismatch — fall through to scalar remainder.
                        break;
                    }
                }

                // Scalar remainder: 0-3 leftover entries after batch-4,
                // or all entries for L2 / other metrics.
                while bi < batch.len() {
                    let (neighbor_id, vec_ptr, vec_len, norm) = batch[bi];

                    if bi + 1 < batch.len() {
                        let (_, next_ptr, next_len, _) = batch[bi + 1];
                        prefetch_read_data(next_ptr);
                        if next_len > 16 {
                            prefetch_read_data(next_ptr.wrapping_add(16));
                        }
                    }

                    // SAFETY: vec_ptr and vec_len come from a live `&Vec<f32>`
                    // obtained from `self.nodes[neighbor_id]` within this same `&self`
                    // borrow. The Vec is immutable (&self) and is not mutated between
                    // Phase 1 (pointer capture) and Phase 2 (dereference).
                    let neighbor_vec = unsafe { std::slice::from_raw_parts(vec_ptr, vec_len) };
                    let dist = distance_fn(query, query_norm, neighbor_vec, norm);

                    if !(ctx.results.len() >= ef && dist > worst_dist) {
                        ctx.candidates
                            .push(std::cmp::Reverse((OrderedF32(dist), neighbor_id)));
                        ctx.results.push((OrderedF32(dist), neighbor_id));
                        if ctx.results.len() > ef {
                            ctx.results.pop();
                        }
                        if let Some(&(OrderedF32(d), _)) = ctx.results.peek() {
                            worst_dist = d;
                        }
                    }
                    bi += 1;
                }
            }
        }

        // Drain results into the scratch buffer and sort.
        // Tie-break by external NodeId for deterministic ordering.
        ctx.result_buf.clear();
        ctx.result_buf
            .extend(ctx.results.drain().map(|(d, iid)| (d.0, iid)));
        ctx.result_buf
            .sort_by(|a, b| match OrderedF32(a.0).cmp(&OrderedF32(b.0)) {
                std::cmp::Ordering::Equal => self.external_id(a.1).cmp(&self.external_id(b.1)),
                other => other,
            });
    }
}
