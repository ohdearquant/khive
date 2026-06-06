//! SIMD batch BM25 scoring: 4-wide NEON/scalar and 8-wide AVX2/scalar implementations.

// ---------------------------------------------------------------------------
// SIMD batch BM25 scoring (4-wide)
// ---------------------------------------------------------------------------

/// Batch-score 4 postings using ARM NEON. Safety: aarch64 only; NEON is baseline on ARMv8-A.
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

/// 4-wide scalar BM25 scoring fallback (non-aarch64).
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

/// 8-wide scalar BM25 scoring fallback (non-aarch64).
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

/// Batch-score 8 postings using AVX2. Safety: requires avx2 runtime detection.
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

/// Batch-score 8 postings using AVX2+FMA. Safety: requires avx2+fma runtime detection.
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

/// 8-wide scoring function pointer; resolved once per term for hot-loop dispatch.
#[cfg(target_arch = "x86_64")]
// SAFETY: Values of this type are only produced by `select_score_batch_8`,
// which pairs each unsafe target-feature function with matching CPU detection.
pub(super) type ScoreBatch8Fn = unsafe fn(&[u8; 8], &[f32; 8], f32, f32, f32, f32) -> [f32; 8];

/// Select best 8-wide scorer: AVX2+FMA > AVX2 > scalar.
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

/// 4-wide batch scorer: NEON on aarch64, scalar otherwise.
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
