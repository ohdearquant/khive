//! SIMD batch BM25 scoring functions.
//!
//! Provides 4-wide (NEON/scalar) and 8-wide (AVX2/scalar) batch scoring
//! implementations. See `docs/simd.md` for platform support and dispatch strategy.

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
pub(super) unsafe fn score_batch_neon(
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
pub(super) fn score_batch_scalar_4(
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
pub(super) fn score_batch_scalar_8(
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
pub(super) unsafe fn score_batch_avx2(
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
pub(super) unsafe fn score_batch_avx2_fma(
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
pub(super) type ScoreBatch8Fn = unsafe fn(&[u8; 8], &[f32; 8], f32, f32, f32, f32) -> [f32; 8];

/// Select the best 8-wide scoring function for the current CPU.
///
/// Priority: AVX2+FMA > AVX2 > scalar fallback.
/// Called once per term, the returned function pointer is used for all
/// batches within that term's posting list.
#[cfg(target_arch = "x86_64")]
#[inline]
pub(super) fn select_score_batch_8() -> ScoreBatch8Fn {
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
pub(super) fn score_batch_4(
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
