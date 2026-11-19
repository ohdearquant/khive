//! SQ8 scalar quantization codecs for approximate distance computation in ANN indexes.
//!
//! Two codecs with different encoding strategies:
//!
//! ## `Sq8Codec` — per-dimension affine, for dot product / cosine
//!
//! Each dimension is mapped to [0, 255] using its own observed min/max.
//! Dot product and cosine reconstruct each dimension in f64 before summing,
//! preserving small dimensions beside a much wider one.
//!
//! ## `GsSq8Codec` — global-scale affine, for L2 (Vamana acquisition)
//!
//! A single shared scale `gs = max_range_across_dims / 255` is used for all dims;
//! per-dim `min_i` offsets are still subtracted before quantizing.
//!
//! L2² in code space: `gs² × Σ (a_i - b_i)²` — exact after the lossy f32→u8 encode
//! (offsets cancel, one scalar factorizes out). No anisotropy gate or residual pass for
//! in-distribution vectors; OOD queries (components outside the trained range) fall back
//! to exact f32 in the caller (see `VamanaIndex::search`). Small-range dims contribute
//! proportionally fewer codes and proportionally less L2 signal — an honest trade-off
//! documented in ADR-052.
//!
//! # Hot-loop NEON helper (`u8_l2sq_u32`)
//!
//! `GsSq8Codec` uses `u8_l2sq_u32`: NEON `vabdq_u8` + `vmull_u8`
//! squaring or a chunked portable fallback. The integer dot helper remains
//! test-only; its shared-mean scaling loses small per-dimension weights.
//!
//! See `docs/api/codecs.md` for the full function-by-function reference and
//! `docs/design.md` for the anisotropy-gating rationale behind `GsSq8Codec`.

#[cfg(feature = "parallel")]
use rayon::prelude::*;

// ─── NEON helpers ─────────────────────────────────────────────────────────────

/// `Σ a_i * b_i` over equal-length `u8` slices as a `u32` accumulator (NEON on
/// aarch64, chunked portable fallback elsewhere). See `docs/api/codecs.md`.
///
/// Panics if the slices have different lengths. Retained only to exercise
/// the integer kernel independently; Sq8Codec no longer uses its lossy
/// shared-mean scale split.
#[cfg(test)]
#[inline(always)]
fn u8_dot_u32(a: &[u8], b: &[u8]) -> u32 {
    assert_eq!(a.len(), b.len(), "u8_dot_u32 inputs must have equal length");

    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        let n = a.len();
        let chunks = n / 16;
        let rem = n % 16;

        let mut acc0: uint32x4_t;
        let mut acc1: uint32x4_t;
        let mut acc2: uint32x4_t;
        let mut acc3: uint32x4_t;

        unsafe {
            acc0 = vdupq_n_u32(0);
            acc1 = vdupq_n_u32(0);
            acc2 = vdupq_n_u32(0);
            acc3 = vdupq_n_u32(0);

            for i in 0..chunks {
                let ap = a.as_ptr().add(i * 16);
                let bp = b.as_ptr().add(i * 16);

                let va = vld1q_u8(ap);
                let vb = vld1q_u8(bp);

                let lo_u16 = vmull_u8(vget_low_u8(va), vget_low_u8(vb));
                let hi_u16 = vmull_high_u8(va, vb);

                acc0 = vaddq_u32(acc0, vmovl_u16(vget_low_u16(lo_u16)));
                acc1 = vaddq_u32(acc1, vmovl_high_u16(lo_u16));
                acc2 = vaddq_u32(acc2, vmovl_u16(vget_low_u16(hi_u16)));
                acc3 = vaddq_u32(acc3, vmovl_high_u16(hi_u16));
            }

            let sum4 = vaddq_u32(vaddq_u32(acc0, acc1), vaddq_u32(acc2, acc3));
            let mut total = vaddvq_u32(sum4);

            for i in (n - rem)..n {
                total += a[i] as u32 * b[i] as u32;
            }
            total
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        a.chunks(8)
            .zip(b.chunks(8))
            .map(|(ac, bc)| {
                ac.iter()
                    .zip(bc.iter())
                    .map(|(&x, &y)| (x as u32) * (y as u32))
                    .sum::<u32>()
            })
            .sum()
    }
}

/// `Σ (a_i - b_i)²` over equal-length `u8` slices as a `u32` accumulator
/// (NEON on aarch64, chunked portable fallback elsewhere). See
/// `docs/api/codecs.md` for the kernel breakdown.
///
/// Safety: both slices must have the same length (panics otherwise).
#[inline(always)]
pub fn u8_l2sq_u32(a: &[u8], b: &[u8]) -> u32 {
    assert_eq!(
        a.len(),
        b.len(),
        "u8_l2sq_u32 inputs must have equal length"
    );

    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        let n = a.len();
        let chunks = n / 16;
        let rem = n % 16;

        let mut acc0: uint32x4_t;
        let mut acc1: uint32x4_t;
        let mut acc2: uint32x4_t;
        let mut acc3: uint32x4_t;

        unsafe {
            acc0 = vdupq_n_u32(0);
            acc1 = vdupq_n_u32(0);
            acc2 = vdupq_n_u32(0);
            acc3 = vdupq_n_u32(0);

            for i in 0..chunks {
                let ap = a.as_ptr().add(i * 16);
                let bp = b.as_ptr().add(i * 16);

                let va = vld1q_u8(ap);
                let vb = vld1q_u8(bp);

                let diff = vabdq_u8(va, vb);

                let lo_u16 = vmull_u8(vget_low_u8(diff), vget_low_u8(diff));
                let hi_u16 = vmull_high_u8(diff, diff);

                acc0 = vaddq_u32(acc0, vmovl_u16(vget_low_u16(lo_u16)));
                acc1 = vaddq_u32(acc1, vmovl_high_u16(lo_u16));
                acc2 = vaddq_u32(acc2, vmovl_u16(vget_low_u16(hi_u16)));
                acc3 = vaddq_u32(acc3, vmovl_high_u16(hi_u16));
            }

            let sum4 = vaddq_u32(vaddq_u32(acc0, acc1), vaddq_u32(acc2, acc3));
            let mut total = vaddvq_u32(sum4);

            for i in (n - rem)..n {
                let d = (a[i] as i32) - (b[i] as i32);
                total += (d * d) as u32;
            }
            total
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        a.chunks(8)
            .zip(b.chunks(8))
            .map(|(ac, bc)| {
                ac.iter()
                    .zip(bc.iter())
                    .map(|(&x, &y)| {
                        let d = (x as i32) - (y as i32);
                        (d * d) as u32
                    })
                    .sum::<u32>()
            })
            .sum()
    }
}

// ─── Validation errors (QUANT-AUD-002) ─────────────────────────────────────────

/// Errors returned by the `try_*` codec constructors and encoders.
///
/// Public train/encode input is caller-controlled (corpus data, external
/// vectors); shape mismatches here must be a typed error rather than a panic
/// or a silently truncated/malformed encoded vector.
#[derive(Debug, Clone, PartialEq)]
pub enum QuantError {
    /// The training corpus contained zero rows.
    EmptyCorpus,
    /// `dims` was zero (flat API) or the first row was empty (row API).
    ZeroDims,
    /// A flat vector's length was not a multiple of `dims`.
    FlatLengthNotDivisible { len: usize, dims: usize },
    /// A training row's length did not match the dims established by row 0.
    RaggedRow {
        row: usize,
        expected: usize,
        got: usize,
    },
    /// A vector passed to `encode`/`encode_flat_par` did not match the
    /// codec's trained dims.
    EncodeLengthMismatch { expected: usize, got: usize },
}

impl std::fmt::Display for QuantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCorpus => write!(f, "cannot train on empty corpus"),
            Self::ZeroDims => write!(f, "dims must be > 0"),
            Self::FlatLengthNotDivisible { len, dims } => write!(
                f,
                "flat vector length {len} is not a multiple of dims {dims}"
            ),
            Self::RaggedRow { row, expected, got } => write!(
                f,
                "row {row} has length {got}, expected {expected} (dims fixed by row 0)"
            ),
            Self::EncodeLengthMismatch { expected, got } => write!(
                f,
                "vector length {got} does not match codec dims {expected}"
            ),
        }
    }
}

impl std::error::Error for QuantError {}

/// Per-dimension min/max over row-major flat vectors; validates `dims > 0`,
/// non-empty corpus, `vectors.len()` a multiple of `dims`. Non-finite values
/// are skipped. See [`finalize_min_max`] for the empty-dimension default.
fn flat_min_max(vectors: &[f32], dims: usize) -> Result<(Vec<f32>, Vec<f32>), QuantError> {
    if dims == 0 {
        return Err(QuantError::ZeroDims);
    }
    if vectors.is_empty() {
        return Err(QuantError::EmptyCorpus);
    }
    if !vectors.len().is_multiple_of(dims) {
        return Err(QuantError::FlatLengthNotDivisible {
            len: vectors.len(),
            dims,
        });
    }

    let n = vectors.len() / dims;
    let mut min = vec![f32::INFINITY; dims];
    let mut max = vec![f32::NEG_INFINITY; dims];

    for row in 0..n {
        let v = &vectors[row * dims..(row + 1) * dims];
        for (d, &x) in v.iter().enumerate() {
            if x.is_finite() {
                if x < min[d] {
                    min[d] = x;
                }
                if x > max[d] {
                    max[d] = x;
                }
            }
        }
    }
    finalize_min_max(&mut min, &mut max);
    Ok((min, max))
}

/// Per-dimension min/max over row vectors; validates non-empty corpus,
/// `dims > 0` (row 0's length), and every row matching row 0's length.
fn row_min_max(vectors: &[Vec<f32>]) -> Result<(usize, Vec<f32>, Vec<f32>), QuantError> {
    if vectors.is_empty() {
        return Err(QuantError::EmptyCorpus);
    }
    let dims = vectors[0].len();
    if dims == 0 {
        return Err(QuantError::ZeroDims);
    }

    let mut min = vec![f32::INFINITY; dims];
    let mut max = vec![f32::NEG_INFINITY; dims];

    for (row, v) in vectors.iter().enumerate() {
        if v.len() != dims {
            return Err(QuantError::RaggedRow {
                row,
                expected: dims,
                got: v.len(),
            });
        }
        for (d, &x) in v.iter().enumerate() {
            if x.is_finite() {
                if x < min[d] {
                    min[d] = x;
                }
                if x > max[d] {
                    max[d] = x;
                }
            }
        }
    }
    finalize_min_max(&mut min, &mut max);
    Ok((dims, min, max))
}

/// Default a dimension with no finite observation to `min=0, max=1`; widen a
/// degenerate `max <= min` to a unit range so `scale`/`gs` never divide by zero.
fn finalize_min_max(min: &mut [f32], max: &mut [f32]) {
    for d in 0..min.len() {
        if !min[d].is_finite() {
            min[d] = 0.0;
        }
        if !max[d].is_finite() || max[d] <= min[d] {
            max[d] = min[d] + 1.0;
        }
    }
}

// ─── Sq8Codec (per-dim scale — dot product / cosine) ──────────────────────────

/// Per-dimension affine SQ8 codec for dot product and cosine distance.
///
/// Encodes `f32` dimensions to `u8` via: `code = round((x - min) / scale)`,
/// where `scale_i = (max_i - min_i) / 255`.
///
/// For L2 distance on Vamana builds, use [`GsSq8Codec`] instead — its global
/// shared scale makes L2 algebraically exact in code space without a residual pass.
#[derive(Debug, Clone)]
pub struct Sq8Codec {
    /// Per-dimension minimum values.
    pub min: Vec<f32>,
    /// Per-dimension scale: `(max - min) / 255`.
    pub scale: Vec<f32>,
    /// Per-dimension `scale²` precomputed for fast L2.
    pub scale_sq: Vec<f32>,
    /// Legacy per-dimension f64 `scale²` cache, retained for public-field compatibility.
    pub scale_sq_f64: Vec<f64>,
    /// Legacy mean of `scale_sq`; retained for public-field compatibility.
    pub mean_scale_sq: f32,
    /// Legacy residual: `scale_sq_i - mean_scale_sq`; distance methods do not use it.
    pub scale_sq_residual: Vec<f32>,
    /// Legacy f32 `Σ_i min_i²` correction, retained for public-field compatibility.
    pub offset_sq_sum: f32,
    /// Legacy f64 `Σ_i min_i²` cache, retained for public-field compatibility.
    pub offset_sq_sum_f64: f64,
}

/// A corpus vector encoded by [`Sq8Codec`].
#[derive(Debug, Clone)]
pub struct EncodedVector {
    /// SQ8 u8 codes, one per dimension.
    pub codes: Vec<u8>,
    /// L2 norm of the original f32 vector (for cosine distance).
    pub norm: f32,
    /// Legacy f32 `Σ_i scale_i * min_i * code_i` correction.
    pub soc_sum: f32,
    /// Legacy f64 `Σ_i scale_i * min_i * code_i` cache, retained for compatibility.
    pub soc_sum_f64: f64,
    /// Legacy `Σ_i scale_sq_residual_i * code_i` precomputed at encode time.
    pub residual_dot_bias: f32,
}

impl Sq8Codec {
    fn build_from_min_max(min: Vec<f32>, max: Vec<f32>) -> Self {
        let dims = min.len();
        let scale: Vec<f32> = (0..dims).map(|d| (max[d] - min[d]) / 255.0).collect();
        let scale_sq: Vec<f32> = scale.iter().map(|s| s * s).collect();
        let scale_sq_f64: Vec<f64> = scale.iter().map(|&s| f64::from(s) * f64::from(s)).collect();
        let mean_scale_sq = scale_sq.iter().sum::<f32>() / dims as f32;
        let scale_sq_residual: Vec<f32> = scale_sq.iter().map(|&ss| ss - mean_scale_sq).collect();
        let offset_sq_sum: f32 = min.iter().map(|o| o * o).sum();
        let offset_sq_sum_f64: f64 = min.iter().map(|&o| f64::from(o) * f64::from(o)).sum();

        Self {
            min,
            scale,
            scale_sq,
            scale_sq_f64,
            mean_scale_sq,
            scale_sq_residual,
            offset_sq_sum,
            offset_sq_sum_f64,
        }
    }

    /// Train a codec from row-major flat vectors.
    ///
    /// Panics on invalid input. See [`Self::try_train_flat`] for a fallible
    /// variant that returns [`QuantError`] instead.
    pub fn train_flat(vectors: &[f32], dims: usize) -> Self {
        Self::try_train_flat(vectors, dims).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::train_flat`]. Validates `dims > 0`, a
    /// non-empty corpus, and that `vectors.len()` is a multiple of `dims`.
    pub fn try_train_flat(vectors: &[f32], dims: usize) -> Result<Self, QuantError> {
        let (min, max) = flat_min_max(vectors, dims)?;
        Ok(Self::build_from_min_max(min, max))
    }

    /// Train from a slice of row vectors (each a `Vec<f32>`).
    ///
    /// Panics on invalid input. See [`Self::try_train`] for a fallible
    /// variant that returns [`QuantError`] instead.
    pub fn train(vectors: &[Vec<f32>]) -> Self {
        Self::try_train(vectors).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::train`]. Validates a non-empty corpus,
    /// `dims > 0` (row 0's length), and that every row is the same length
    /// (rectangular corpus); a ragged row returns [`QuantError::RaggedRow`]
    /// instead of panicking on out-of-bounds indexing.
    pub fn try_train(vectors: &[Vec<f32>]) -> Result<Self, QuantError> {
        let (_dims, min, max) = row_min_max(vectors)?;
        Ok(Self::build_from_min_max(min, max))
    }

    /// Encode a single vector into SQ8 codes + correction metadata.
    ///
    /// Panics if `v.len()` does not match the codec's trained dims. See
    /// [`Self::try_encode`] for a fallible variant that returns
    /// [`QuantError`] instead.
    pub fn encode(&self, v: &[f32]) -> EncodedVector {
        self.try_encode(v).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::encode`]. Validates `v.len()` against the
    /// codec's trained dims before encoding. See `docs/design.md` (QUANT-AUD-002)
    /// for why this check must be a typed error, not a debug-only assertion.
    pub fn try_encode(&self, v: &[f32]) -> Result<EncodedVector, QuantError> {
        let dims = self.min.len();
        if v.len() != dims {
            return Err(QuantError::EncodeLengthMismatch {
                expected: dims,
                got: v.len(),
            });
        }
        Ok(self.encode_unchecked(v))
    }

    /// Encode a vector already validated to have `v.len() == self.min.len()`.
    fn encode_unchecked(&self, v: &[f32]) -> EncodedVector {
        let dims = self.min.len();
        let mut codes = Vec::with_capacity(dims);
        let mut soc_sum = 0.0f32;
        let mut soc_sum_f64 = 0.0f64;
        let mut residual_dot_bias = 0.0f32;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();

        for (d, &x) in v.iter().enumerate() {
            let s = self.scale[d];
            let inv_s = if s > 1e-12 { 1.0 / s } else { 0.0 };
            let raw = (x - self.min[d]) * inv_s;
            let code = raw.round().clamp(0.0, 255.0) as u8;
            codes.push(code);
            soc_sum += s * self.min[d] * code as f32;
            soc_sum_f64 += f64::from(s) * f64::from(self.min[d]) * f64::from(code);
            residual_dot_bias += self.scale_sq_residual[d] * code as f32;
        }

        EncodedVector {
            codes,
            norm,
            soc_sum,
            soc_sum_f64,
            residual_dot_bias,
        }
    }

    /// Encode a batch of flat-row vectors, using Rayon when the `parallel` feature is enabled.
    ///
    /// Panics on invalid input. See [`Self::try_encode_flat_par`] for a
    /// fallible variant that returns [`QuantError`] instead.
    pub fn encode_flat_par(&self, vectors: &[f32], dims: usize) -> Vec<EncodedVector> {
        self.try_encode_flat_par(vectors, dims)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::encode_flat_par`]. Validates `dims > 0`,
    /// divisibility, and that `dims` matches the codec's trained dims before
    /// dividing `vectors.len() / dims`. See `docs/design.md` (QUANT-AUD-002).
    pub fn try_encode_flat_par(
        &self,
        vectors: &[f32],
        dims: usize,
    ) -> Result<Vec<EncodedVector>, QuantError> {
        if dims == 0 {
            return Err(QuantError::ZeroDims);
        }
        if !vectors.len().is_multiple_of(dims) {
            return Err(QuantError::FlatLengthNotDivisible {
                len: vectors.len(),
                dims,
            });
        }
        if dims != self.min.len() {
            return Err(QuantError::EncodeLengthMismatch {
                expected: self.min.len(),
                got: dims,
            });
        }
        let n = vectors.len() / dims;
        #[cfg(feature = "parallel")]
        let encoded = (0..n)
            .into_par_iter()
            .map(|i| self.encode_unchecked(&vectors[i * dims..(i + 1) * dims]))
            .collect();
        #[cfg(not(feature = "parallel"))]
        let encoded = (0..n)
            .map(|i| self.encode_unchecked(&vectors[i * dims..(i + 1) * dims]))
            .collect();
        Ok(encoded)
    }

    /// Encode a batch of row vectors, using Rayon when the `parallel` feature is enabled.
    ///
    /// Panics if any row's length does not match the codec's trained dims.
    /// See [`Self::try_encode_par`] for a fallible variant that returns
    /// [`QuantError`] instead.
    pub fn encode_par(&self, vectors: &[Vec<f32>]) -> Vec<EncodedVector> {
        self.try_encode_par(vectors)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::encode_par`]. Validates every row's length
    /// against the codec's trained dims before dispatching to the thread pool.
    pub fn try_encode_par(&self, vectors: &[Vec<f32>]) -> Result<Vec<EncodedVector>, QuantError> {
        let dims = self.min.len();
        for v in vectors {
            if v.len() != dims {
                return Err(QuantError::EncodeLengthMismatch {
                    expected: dims,
                    got: v.len(),
                });
            }
        }
        #[cfg(feature = "parallel")]
        let encoded = vectors
            .par_iter()
            .map(|v| self.encode_unchecked(v))
            .collect();
        #[cfg(not(feature = "parallel"))]
        let encoded = vectors.iter().map(|v| self.encode_unchecked(v)).collect();
        Ok(encoded)
    }

    /// Approximate dot product between two encoded vectors (same codec).
    ///
    /// Each dimension contributes `(s·a + min)·(s·b + min)` in f64 before
    /// dimensions are summed. Keeping the affine correction within each
    /// dimension avoids cancellation that erases a narrow dimension when
    /// another dimension has much larger offsets.
    /// Panics if either encoded vector has a different length from this codec.
    #[inline]
    pub fn approx_dot(&self, a: &EncodedVector, b: &EncodedVector) -> f32 {
        let dims = self.scale_sq.len();
        assert_eq!(
            a.codes.len(),
            dims,
            "approx_dot input codes must match codec dims"
        );
        assert_eq!(
            b.codes.len(),
            dims,
            "approx_dot input codes must match codec dims"
        );
        let dot: f64 = self
            .scale
            .iter()
            .zip(self.min.iter())
            .zip(a.codes.iter())
            .zip(b.codes.iter())
            .map(|(((&scale, &min), &ac), &bc)| {
                let scale = f64::from(scale);
                let min = f64::from(min);
                let a_value = scale.mul_add(f64::from(ac), min);
                let b_value = scale.mul_add(f64::from(bc), min);
                a_value * b_value
            })
            .sum();
        dot as f32
    }

    /// Approximate cosine distance between two encoded vectors (same codec).
    ///
    /// Returns `1 - dot / (norm_a * norm_b)`. Falls back to 1.0 for zero norms.
    #[inline]
    pub fn approx_cosine_dist(&self, a: &EncodedVector, b: &EncodedVector) -> f32 {
        let denom = a.norm * b.norm;
        if !denom.is_finite() || denom <= 0.0 {
            return 1.0;
        }
        let dot = self.approx_dot(a, b);
        let cosine = (dot / denom).clamp(-1.0, 1.0);
        1.0 - cosine
    }

    /// Approximate squared L2 distance with direct per-dimension weights.
    ///
    /// Full-precision identity: `||a-b||² = Σ scale_sq_i * (a_i-b_i)²`.
    /// Offsets cancel because both vectors share the same codec.
    ///
    /// Nonnegative terms are accumulated in f64 and rounded to f32 once.
    /// This avoids cancellation between a shared f32 mean and residuals in
    /// strongly anisotropic corpora.
    /// Panics if either encoded vector has a different length from this codec.
    ///
    /// For Vamana L2 acquisition use [`GsSq8Codec::l2_sq`] — algebraically exact
    /// in code space and uses the integer NEON path.
    #[inline]
    pub fn approx_l2_sq(&self, a: &EncodedVector, b: &EncodedVector) -> f32 {
        let dims = self.scale_sq.len();
        assert_eq!(
            a.codes.len(),
            dims,
            "approx_l2_sq input codes must match codec dims"
        );
        assert_eq!(
            b.codes.len(),
            dims,
            "approx_l2_sq input codes must match codec dims"
        );
        let weighted: f64 = self
            .scale_sq
            .iter()
            .zip(a.codes.iter())
            .zip(b.codes.iter())
            .map(|((&weight, &ac), &bc)| {
                let delta = f64::from(ac) - f64::from(bc);
                f64::from(weight) * delta * delta
            })
            .sum();
        weighted as f32
    }

    /// Number of dimensions.
    pub fn dims(&self) -> usize {
        self.min.len()
    }
}

// ─── GsSq8Codec (global-scale — L2 / Vamana acquisition) ─────────────────────

/// Global-scale SQ8 codec for L2 distance — the Vamana acquisition path.
///
/// A single shared scale `gs = max_range_across_dims / 255` is used for all
/// dims; per-dim offsets are still subtracted before quantizing. Encoding is
/// **lossy** (rounded + clamped to u8); L2² in code space is exact after that
/// lossy encode, but round-trip error vs. true f32 L2² can reach ~15% for
/// anisotropic/OOD data — no residual pass, no gate, no silent fallback.
/// Callers needing correctness on OOD queries must check
/// [`Self::is_in_distribution`] and fall back to exact f32 themselves.
/// See `docs/api/codecs.md` for the full accuracy discussion and
/// `docs/design.md` for why this replaced the earlier per-dim anisotropy-gated design.
#[derive(Debug, Clone)]
pub struct GsSq8Codec {
    /// Per-dimension minimum values.
    pub min: Vec<f32>,
    /// Global scale: `max_range / 255` where `max_range = max_i(max_i - min_i)`.
    pub gs: f32,
    /// `gs²` precomputed for L2.
    pub gs_sq: f32,
    /// Anisotropy ratio measured at train time: `max(range_i) / min(nonzero range_i)`.
    /// Informational only — never used for dispatch decisions.
    pub anisotropy_ratio: f32,
}

/// A corpus vector encoded by [`GsSq8Codec`].
#[derive(Debug, Clone)]
pub struct GsEncodedVector {
    /// SQ8 u8 codes, one per dimension.
    pub codes: Vec<u8>,
}

impl GsSq8Codec {
    fn build_from_min_max(min: Vec<f32>, max: Vec<f32>) -> Self {
        let dims = min.len();
        let ranges: Vec<f32> = (0..dims).map(|d| max[d] - min[d]).collect();
        let max_range = ranges.iter().cloned().fold(0.0f32, f32::max);
        let gs = if max_range > 1e-12 {
            max_range / 255.0
        } else {
            1.0 / 255.0
        };

        let min_range_nonzero = ranges
            .iter()
            .cloned()
            .filter(|&r| r > 1e-12)
            .fold(f32::INFINITY, f32::min);
        let anisotropy_ratio = if min_range_nonzero.is_finite() && min_range_nonzero > 0.0 {
            max_range / min_range_nonzero
        } else {
            1.0
        };

        Self {
            min,
            gs,
            gs_sq: gs * gs,
            anisotropy_ratio,
        }
    }

    /// Train from row-major flat vectors.
    ///
    /// Panics on invalid input. See [`Self::try_train_flat`] for a fallible
    /// variant that returns [`QuantError`] instead.
    pub fn train_flat(vectors: &[f32], dims: usize) -> Self {
        Self::try_train_flat(vectors, dims).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::train_flat`]. Validates `dims > 0`, a
    /// non-empty corpus, and that `vectors.len()` is a multiple of `dims`.
    pub fn try_train_flat(vectors: &[f32], dims: usize) -> Result<Self, QuantError> {
        let (min, max) = flat_min_max(vectors, dims)?;
        Ok(Self::build_from_min_max(min, max))
    }

    /// Train from a slice of row vectors.
    ///
    /// Panics on invalid input. See [`Self::try_train`] for a fallible
    /// variant that returns [`QuantError`] instead.
    pub fn train(vectors: &[Vec<f32>]) -> Self {
        Self::try_train(vectors).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::train`]. Validates a non-empty corpus,
    /// `dims > 0` (row 0's length), and that every row is the same length
    /// (rectangular corpus); a ragged row returns [`QuantError::RaggedRow`]
    /// instead of panicking on out-of-bounds indexing.
    pub fn try_train(vectors: &[Vec<f32>]) -> Result<Self, QuantError> {
        let (_dims, min, max) = row_min_max(vectors)?;
        Ok(Self::build_from_min_max(min, max))
    }

    /// Encode a single vector.
    ///
    /// Panics if `v.len()` does not match the codec's trained dims. See
    /// [`Self::try_encode`] for a fallible variant that returns
    /// [`QuantError`] instead.
    #[inline]
    pub fn encode(&self, v: &[f32]) -> GsEncodedVector {
        self.try_encode(v).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::encode`]. Validates `v.len()` against the
    /// codec's trained dims before encoding — an unchecked shape mismatch
    /// (e.g. `v = &[]`) could otherwise score as a false exact match; see
    /// `docs/design.md` (QUANT-AUD-002).
    pub fn try_encode(&self, v: &[f32]) -> Result<GsEncodedVector, QuantError> {
        let dims = self.min.len();
        if v.len() != dims {
            return Err(QuantError::EncodeLengthMismatch {
                expected: dims,
                got: v.len(),
            });
        }
        Ok(self.encode_unchecked(v))
    }

    /// Encode a vector already validated to have `v.len() == self.min.len()`.
    #[inline]
    fn encode_unchecked(&self, v: &[f32]) -> GsEncodedVector {
        let inv_gs = if self.gs > 1e-12 { 1.0 / self.gs } else { 0.0 };
        let codes = v
            .iter()
            .enumerate()
            .map(|(d, &x)| ((x - self.min[d]) * inv_gs).round().clamp(0.0, 255.0) as u8)
            .collect();
        GsEncodedVector { codes }
    }

    /// Encode a batch of flat-row vectors, using Rayon when the `parallel` feature is enabled.
    ///
    /// Panics on invalid input. See [`Self::try_encode_flat_par`] for a
    /// fallible variant that returns [`QuantError`] instead.
    pub fn encode_flat_par(&self, vectors: &[f32], dims: usize) -> Vec<GsEncodedVector> {
        self.try_encode_flat_par(vectors, dims)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible variant of [`Self::encode_flat_par`]. Validates `dims > 0`,
    /// divisibility, and that `dims` matches the codec's trained dims before
    /// dividing `vectors.len() / dims`. See `docs/design.md` (QUANT-AUD-002).
    pub fn try_encode_flat_par(
        &self,
        vectors: &[f32],
        dims: usize,
    ) -> Result<Vec<GsEncodedVector>, QuantError> {
        if dims == 0 {
            return Err(QuantError::ZeroDims);
        }
        if !vectors.len().is_multiple_of(dims) {
            return Err(QuantError::FlatLengthNotDivisible {
                len: vectors.len(),
                dims,
            });
        }
        if dims != self.min.len() {
            return Err(QuantError::EncodeLengthMismatch {
                expected: self.min.len(),
                got: dims,
            });
        }
        let n = vectors.len() / dims;
        #[cfg(feature = "parallel")]
        let encoded = (0..n)
            .into_par_iter()
            .map(|i| self.encode_unchecked(&vectors[i * dims..(i + 1) * dims]))
            .collect();
        #[cfg(not(feature = "parallel"))]
        let encoded = (0..n)
            .map(|i| self.encode_unchecked(&vectors[i * dims..(i + 1) * dims]))
            .collect();
        Ok(encoded)
    }

    /// Approximate squared L2 distance.
    ///
    /// `||a-b||² ≈ gs² × Σ (a_i - b_i)²`
    ///
    /// Exact in code space (offset terms cancel, `gs²` factorizes) after the
    /// lossy f32→u8 encode. Per-round-trip L2 error can reach ~15%; recall
    /// safety is established by probe, not by this formula.
    /// The NEON path runs ~13 ns at 384-d.
    #[inline]
    pub fn l2_sq(&self, a: &GsEncodedVector, b: &GsEncodedVector) -> f32 {
        self.gs_sq * u8_l2sq_u32(&a.codes, &b.codes) as f32
    }

    /// [`Self::l2_sq`] over raw code slices, for callers that store codes in a
    /// flat (possibly memory-mapped) buffer rather than per-vector allocations.
    #[inline]
    pub fn l2_sq_codes(&self, a: &[u8], b: &[u8]) -> f32 {
        self.gs_sq * u8_l2sq_u32(a, b) as f32
    }

    /// Number of dimensions.
    pub fn dims(&self) -> usize {
        self.min.len()
    }

    /// Returns `true` if every component of `v` falls within the trained range
    /// `[min_d, min_d + 255 * gs]` (i.e., encoding would produce no clamping).
    ///
    /// When this returns `false` at least one dimension is out-of-distribution;
    /// callers that need correctness guarantees should fall back to exact f32.
    #[inline]
    pub fn is_in_distribution(&self, v: &[f32]) -> bool {
        let max_code = 255.0 * self.gs;
        v.iter()
            .zip(self.min.iter())
            .all(|(&x, &mn)| x >= mn && x <= mn + max_code)
    }
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
