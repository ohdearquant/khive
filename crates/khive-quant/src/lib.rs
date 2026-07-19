//! SQ8 scalar quantization codecs for approximate distance computation in ANN indexes.
//!
//! Two codecs with different encoding strategies:
//!
//! ## `Sq8Codec` — per-dimension affine, for dot product / cosine
//!
//! Each dimension is mapped to [0, 255] using its own observed min/max.
//! Dot product and cosine require per-dim scale accuracy; the residual-corrected
//! path (`approx_dot`, `approx_cosine_dist`) preserves ordinal ranking.
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
//! # Hot-loop NEON helpers (`u8_dot_u32`, `u8_l2sq_u32`)
//!
//! Both codecs share these inner functions:
//! - `u8_dot_u32`: NEON `vmull_u8` (16-wide u8→u16→u32) or chunked portable fallback.
//! - `u8_l2sq_u32`: NEON `vabdq_u8` + `vmull_u8` squaring or chunked portable fallback.
//!
//! See `docs/api/codecs.md` for the full function-by-function reference and
//! `docs/design.md` for the anisotropy-gating rationale behind `GsSq8Codec`.

#[cfg(feature = "parallel")]
use rayon::prelude::*;

// ─── NEON helpers ─────────────────────────────────────────────────────────────

/// `Σ a_i * b_i` over equal-length `u8` slices as a `u32` accumulator (NEON on
/// aarch64, chunked portable fallback elsewhere). See `docs/api/codecs.md`.
///
/// Safety: both slices must have the same length.
#[inline(always)]
fn u8_dot_u32(a: &[u8], b: &[u8]) -> u32 {
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
    /// Per-dimension `scale²` precomputed for fast L2 and dot product.
    pub scale_sq: Vec<f32>,
    /// Mean of `scale_sq` across all dimensions — used as the integer-pass multiplier.
    pub mean_scale_sq: f32,
    /// Residual: `scale_sq_i - mean_scale_sq` (zero-mean, small magnitude).
    pub scale_sq_residual: Vec<f32>,
    /// `Σ_i min_i²` precomputed for dot-product correction.
    pub offset_sq_sum: f32,
}

/// A corpus vector encoded by [`Sq8Codec`].
#[derive(Debug, Clone)]
pub struct EncodedVector {
    /// SQ8 u8 codes, one per dimension.
    pub codes: Vec<u8>,
    /// L2 norm of the original f32 vector (for cosine distance).
    pub norm: f32,
    /// `Σ_i scale_i * min_i * code_i` — per-vector correction term for dot product.
    pub soc_sum: f32,
    /// `Σ_i scale_sq_residual_i * code_i` precomputed at encode time.
    pub residual_dot_bias: f32,
}

impl Sq8Codec {
    fn build_from_min_max(min: Vec<f32>, max: Vec<f32>) -> Self {
        let dims = min.len();
        let scale: Vec<f32> = (0..dims).map(|d| (max[d] - min[d]) / 255.0).collect();
        let scale_sq: Vec<f32> = scale.iter().map(|s| s * s).collect();
        let mean_scale_sq = scale_sq.iter().sum::<f32>() / dims as f32;
        let scale_sq_residual: Vec<f32> = scale_sq.iter().map(|&ss| ss - mean_scale_sq).collect();
        let offset_sq_sum: f32 = min.iter().map(|o| o * o).sum();

        Self {
            min,
            scale,
            scale_sq,
            mean_scale_sq,
            scale_sq_residual,
            offset_sq_sum,
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
        let mut residual_dot_bias = 0.0f32;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();

        for (d, &x) in v.iter().enumerate() {
            let s = self.scale[d];
            let inv_s = if s > 1e-12 { 1.0 / s } else { 0.0 };
            let raw = (x - self.min[d]) * inv_s;
            let code = raw.round().clamp(0.0, 255.0) as u8;
            codes.push(code);
            soc_sum += s * self.min[d] * code as f32;
            residual_dot_bias += self.scale_sq_residual[d] * code as f32;
        }

        EncodedVector {
            codes,
            norm,
            soc_sum,
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
    /// Full-precision correction identity (same min/scale for both):
    /// `dot(a, b) = Σ s²·a·b + soc_a + soc_b + offset_sq_sum`
    ///
    /// The integer pass (`u8_dot_u32`) computes `raw = Σ a_i*b_i` as `u32` using
    /// NEON (16-wide on aarch64). The scale correction then applies `mean_scale_sq`
    /// plus a compact per-dim residual f32 pass for accuracy.
    #[inline]
    pub fn approx_dot(&self, a: &EncodedVector, b: &EncodedVector) -> f32 {
        let raw = u8_dot_u32(&a.codes, &b.codes) as f32;
        let residual_hot: f32 = self
            .scale_sq_residual
            .iter()
            .zip(a.codes.iter())
            .zip(b.codes.iter())
            .map(|((r, &ac), &bc)| r * (ac as f32) * (bc as f32))
            .sum();
        self.mean_scale_sq * raw + residual_hot + a.soc_sum + b.soc_sum + self.offset_sq_sum
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

    /// Approximate squared L2 distance — per-dim residual corrected.
    ///
    /// Full-precision identity: `||a-b||² = Σ scale_sq_i * (a_i-b_i)²`.
    /// Offsets cancel because both vectors share the same codec.
    ///
    /// The integer pass (`u8_l2sq_u32`) computes `raw = Σ (a_i-b_i)²` using NEON
    /// `vabdq_u8` + `vmull_u8`. The residual correction keeps ordinal accuracy
    /// across anisotropic corpora.
    ///
    /// For Vamana L2 acquisition use [`GsSq8Codec::l2_sq`] — algebraically exact
    /// in code space and ~2× faster (no residual pass).
    #[inline]
    pub fn approx_l2_sq(&self, a: &EncodedVector, b: &EncodedVector) -> f32 {
        let raw = u8_l2sq_u32(&a.codes, &b.codes) as f32;
        let residual_hot: f32 = self
            .scale_sq_residual
            .iter()
            .zip(a.codes.iter())
            .zip(b.codes.iter())
            .map(|((r, &ac), &bc)| {
                let d = (ac as i32) - (bc as i32);
                r * (d as f32) * (d as f32)
            })
            .sum();
        self.mean_scale_sq * raw + residual_hot
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
mod tests {
    use super::*;

    fn rand_vecs(n: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut h = seed;
        (0..n)
            .map(|_| {
                (0..dims)
                    .map(|_| {
                        h = h
                            .wrapping_mul(0x6c62_272e_07bb_0142)
                            .wrapping_add(0x62b8_2175_62d9_6b1a);
                        let bits = (h >> 33) as u32;
                        (bits as f32) / (u32::MAX as f32) * 2.0 - 1.0
                    })
                    .collect()
            })
            .collect()
    }

    fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    fn l2_sq_f32(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
    }

    // ── QUANT-AUD-002: validation regression tests ──────────────────────────

    #[test]
    fn sq8_try_train_ragged_rows_returns_error_not_panic() {
        let vecs = vec![vec![0.0], vec![1.0, 2.0]];
        let err = Sq8Codec::try_train(&vecs).expect_err("ragged rows must be rejected");
        assert_eq!(
            err,
            QuantError::RaggedRow {
                row: 1,
                expected: 1,
                got: 2
            }
        );
    }

    #[test]
    fn gs_try_train_ragged_rows_returns_error_not_panic() {
        let vecs = vec![vec![0.0], vec![1.0, 2.0]];
        let err = GsSq8Codec::try_train(&vecs).expect_err("ragged rows must be rejected");
        assert_eq!(
            err,
            QuantError::RaggedRow {
                row: 1,
                expected: 1,
                got: 2
            }
        );
    }

    #[test]
    fn sq8_try_train_empty_corpus_returns_error() {
        let vecs: Vec<Vec<f32>> = vec![];
        assert_eq!(
            Sq8Codec::try_train(&vecs).unwrap_err(),
            QuantError::EmptyCorpus
        );
    }

    #[test]
    fn sq8_try_train_flat_zero_dims_returns_error() {
        assert_eq!(
            Sq8Codec::try_train_flat(&[1.0, 2.0], 0).unwrap_err(),
            QuantError::ZeroDims
        );
    }

    #[test]
    fn gs_try_train_flat_zero_dims_returns_error() {
        assert_eq!(
            GsSq8Codec::try_train_flat(&[1.0, 2.0], 0).unwrap_err(),
            QuantError::ZeroDims
        );
    }

    #[test]
    fn sq8_try_train_flat_remainder_returns_error() {
        // 5 elements is not a multiple of dims=2.
        let err = Sq8Codec::try_train_flat(&[1.0, 2.0, 3.0, 4.0, 5.0], 2).unwrap_err();
        assert_eq!(err, QuantError::FlatLengthNotDivisible { len: 5, dims: 2 });
    }

    #[test]
    fn gs_try_train_flat_remainder_returns_error() {
        let err = GsSq8Codec::try_train_flat(&[1.0, 2.0, 3.0, 4.0, 5.0], 2).unwrap_err();
        assert_eq!(err, QuantError::FlatLengthNotDivisible { len: 5, dims: 2 });
    }

    #[test]
    fn sq8_try_encode_shorter_input_returns_error_not_malformed_vector() {
        let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
        let err = codec.try_encode(&[0.0, 1.0]).unwrap_err();
        assert_eq!(
            err,
            QuantError::EncodeLengthMismatch {
                expected: 4,
                got: 2
            }
        );
    }

    #[test]
    fn sq8_try_encode_longer_input_returns_error() {
        let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
        let err = codec.try_encode(&[0.0, 1.0, 2.0, 3.0, 4.0]).unwrap_err();
        assert_eq!(
            err,
            QuantError::EncodeLengthMismatch {
                expected: 4,
                got: 5
            }
        );
    }

    #[test]
    fn gs_try_encode_empty_input_returns_error_not_malformed_vector() {
        // QUANT-AUD-002: previously encode(&[]) on a trained (dims=1) codec
        // silently returned an EMPTY code vector in release builds (the
        // length check was debug_assert-only), which is_in_distribution
        // vacuously accepted and l2_sq scored as 0.0. Must now be a typed
        // error, never a malformed zero-length code vector.
        let codec = GsSq8Codec::train_flat(&[1.0, 2.0, 3.0, 4.0], 1);
        let err = codec.try_encode(&[]).unwrap_err();
        assert_eq!(
            err,
            QuantError::EncodeLengthMismatch {
                expected: 1,
                got: 0
            }
        );
    }

    #[test]
    fn sq8_try_encode_flat_par_zero_dims_returns_error() {
        let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
        assert_eq!(
            codec.try_encode_flat_par(&[1.0, 2.0], 0).unwrap_err(),
            QuantError::ZeroDims
        );
    }

    #[test]
    fn gs_try_encode_flat_par_zero_dims_returns_error() {
        let codec = GsSq8Codec::train(&rand_vecs(10, 4, 1));
        assert_eq!(
            codec.try_encode_flat_par(&[1.0, 2.0], 0).unwrap_err(),
            QuantError::ZeroDims
        );
    }

    #[test]
    fn sq8_try_encode_flat_par_remainder_returns_error() {
        let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
        let flat: Vec<f32> = (0..9).map(|i| i as f32).collect(); // 9 not a multiple of 4
        assert_eq!(
            codec.try_encode_flat_par(&flat, 4).unwrap_err(),
            QuantError::FlatLengthNotDivisible { len: 9, dims: 4 }
        );
    }

    #[test]
    fn sq8_try_encode_flat_par_dims_mismatch_returns_error() {
        let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
        let flat: Vec<f32> = (0..6).map(|i| i as f32).collect();
        let err = codec.try_encode_flat_par(&flat, 3).unwrap_err();
        assert_eq!(
            err,
            QuantError::EncodeLengthMismatch {
                expected: 4,
                got: 3
            }
        );
    }

    #[test]
    #[should_panic(expected = "row 1 has length 2, expected 1")]
    fn sq8_train_still_panics_with_typed_message_on_ragged_rows() {
        // The panicking convenience wrapper is preserved for existing callers,
        // but must now surface the typed QuantError message rather than
        // panicking from a raw out-of-bounds index.
        let _ = Sq8Codec::train(&[vec![0.0], vec![1.0, 2.0]]);
    }

    #[test]
    fn sq8_try_train_and_encode_roundtrip_matches_panicking_api() {
        let vecs = rand_vecs(20, 8, 7);
        let a = Sq8Codec::train(&vecs);
        let b = Sq8Codec::try_train(&vecs).expect("valid corpus must train");
        assert_eq!(a.min, b.min);
        assert_eq!(a.scale, b.scale);
        let ea = a.encode(&vecs[0]);
        let eb = b.try_encode(&vecs[0]).expect("valid vector must encode");
        assert_eq!(ea.codes, eb.codes);
    }

    // ── Sq8Codec tests ──────────────────────────────────────────────────────

    #[test]
    fn encode_decode_roundtrip_is_bounded() {
        let vecs = rand_vecs(100, 32, 42);
        let codec = Sq8Codec::train(&vecs);
        for v in &vecs {
            let ev = codec.encode(v);
            assert_eq!(ev.codes.len(), v.len());
            for (d, &code) in ev.codes.iter().enumerate() {
                let decoded = code as f32 * codec.scale[d] + codec.min[d];
                let err = (decoded - v[d]).abs();
                assert!(
                    err <= codec.scale[d] + 1e-5,
                    "dim {d}: err={err} scale={}",
                    codec.scale[d]
                );
            }
        }
    }

    #[test]
    fn approx_dot_relative_error_bounded() {
        let vecs = rand_vecs(200, 64, 77);
        let codec = Sq8Codec::train(&vecs);
        let encoded: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

        let mut max_rel_err = 0.0f32;
        for i in 0..vecs.len() {
            for j in (i + 1)..vecs.len().min(i + 10) {
                let true_dot = dot_f32(&vecs[i], &vecs[j]);
                let approx = codec.approx_dot(&encoded[i], &encoded[j]);
                let denom = true_dot.abs().max(1e-3);
                let rel = (approx - true_dot).abs() / denom;
                if rel > max_rel_err {
                    max_rel_err = rel;
                }
            }
        }
        assert!(
            max_rel_err < 0.15,
            "max relative dot error {max_rel_err:.4} >= 0.15"
        );
    }

    #[test]
    fn approx_l2_sq_relative_error_bounded() {
        let vecs = rand_vecs(200, 64, 88);
        let codec = Sq8Codec::train(&vecs);
        let encoded: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

        let mut max_rel_err = 0.0f32;
        for i in 0..vecs.len() {
            for j in (i + 1)..vecs.len().min(i + 10) {
                let true_l2 = l2_sq_f32(&vecs[i], &vecs[j]);
                let approx = codec.approx_l2_sq(&encoded[i], &encoded[j]);
                let denom = true_l2.max(1e-6);
                let rel = (approx - true_l2).abs() / denom;
                if rel > max_rel_err {
                    max_rel_err = rel;
                }
            }
        }
        assert!(
            max_rel_err < 0.15,
            "max relative L2² error {max_rel_err:.4} >= 0.15"
        );
    }

    #[test]
    fn order_preservation_triplets_cosine() {
        let vecs = rand_vecs(300, 64, 99);
        let codec = Sq8Codec::train(&vecs);
        let encoded: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

        let n = vecs.len();
        let mut agree = 0usize;
        let mut total = 0usize;

        for anchor in 0..50 {
            let a = &vecs[anchor];
            let ea = &encoded[anchor];
            for b_idx in 0..n {
                for c_idx in (b_idx + 1)..n.min(b_idx + 5) {
                    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let norm_b: f32 = vecs[b_idx].iter().map(|x| x * x).sum::<f32>().sqrt();
                    let norm_c: f32 = vecs[c_idx].iter().map(|x| x * x).sum::<f32>().sqrt();

                    let cos_ab = dot_f32(a, &vecs[b_idx]) / (norm_a * norm_b).max(1e-9);
                    let cos_ac = dot_f32(a, &vecs[c_idx]) / (norm_a * norm_c).max(1e-9);
                    let dist_ab_true = 1.0 - cos_ab;
                    let dist_ac_true = 1.0 - cos_ac;

                    let dist_ab_approx = codec.approx_cosine_dist(ea, &encoded[b_idx]);
                    let dist_ac_approx = codec.approx_cosine_dist(ea, &encoded[c_idx]);

                    if (dist_ab_true - dist_ac_true).abs() < 0.01 {
                        continue;
                    }

                    let true_closer_b = dist_ab_true < dist_ac_true;
                    let approx_closer_b = dist_ab_approx < dist_ac_approx;
                    if true_closer_b == approx_closer_b {
                        agree += 1;
                    }
                    total += 1;
                }
            }
        }

        let rate = agree as f64 / total.max(1) as f64;
        assert!(
            rate >= 0.95,
            "order preservation {rate:.3} < 0.95 ({agree}/{total})"
        );
    }

    #[test]
    fn order_preservation_triplets_l2() {
        let vecs = rand_vecs(300, 64, 101);
        let codec = Sq8Codec::train(&vecs);
        let encoded: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

        let n = vecs.len();
        let mut agree = 0usize;
        let mut total = 0usize;

        for anchor in 0..50 {
            let a = &vecs[anchor];
            let ea = &encoded[anchor];
            for b_idx in 0..n {
                for c_idx in (b_idx + 1)..n.min(b_idx + 5) {
                    let dist_ab_true = l2_sq_f32(a, &vecs[b_idx]);
                    let dist_ac_true = l2_sq_f32(a, &vecs[c_idx]);

                    let dist_ab_approx = codec.approx_l2_sq(ea, &encoded[b_idx]);
                    let dist_ac_approx = codec.approx_l2_sq(ea, &encoded[c_idx]);

                    if (dist_ab_true - dist_ac_true).abs() < 0.001 {
                        continue;
                    }

                    let true_closer_b = dist_ab_true < dist_ac_true;
                    let approx_closer_b = dist_ab_approx < dist_ac_approx;
                    if true_closer_b == approx_closer_b {
                        agree += 1;
                    }
                    total += 1;
                }
            }
        }

        let rate = agree as f64 / total.max(1) as f64;
        assert!(
            rate >= 0.95,
            "L2 order preservation {rate:.3} < 0.95 ({agree}/{total})"
        );
    }

    #[test]
    fn train_flat_matches_train_rows() {
        let vecs = rand_vecs(50, 16, 123);
        let flat: Vec<f32> = vecs.iter().flatten().copied().collect();

        let codec_rows = Sq8Codec::train(&vecs);
        let codec_flat = Sq8Codec::train_flat(&flat, 16);

        for d in 0..16 {
            assert!((codec_rows.min[d] - codec_flat.min[d]).abs() < 1e-6);
            assert!((codec_rows.scale[d] - codec_flat.scale[d]).abs() < 1e-6);
        }
    }

    #[test]
    fn encode_par_matches_sequential() {
        let vecs = rand_vecs(50, 32, 555);
        let codec = Sq8Codec::train(&vecs);

        let seq: Vec<EncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();
        let par = codec.encode_par(&vecs);

        assert_eq!(seq.len(), par.len());
        for (s, p) in seq.iter().zip(par.iter()) {
            assert_eq!(s.codes, p.codes);
            assert!((s.soc_sum - p.soc_sum).abs() < 1e-5);
        }
    }

    #[test]
    fn sq8_try_encode_par_short_row_returns_error_not_panic() {
        let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
        let mut rows = rand_vecs(5, 4, 2);
        rows[3] = vec![0.0, 1.0];
        let err = codec.try_encode_par(&rows).unwrap_err();
        assert_eq!(
            err,
            QuantError::EncodeLengthMismatch {
                expected: 4,
                got: 2
            }
        );
    }

    #[test]
    fn sq8_try_encode_par_long_row_returns_error_not_panic() {
        let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
        let mut rows = rand_vecs(5, 4, 2);
        rows[3] = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0];
        let err = codec.try_encode_par(&rows).unwrap_err();
        assert_eq!(
            err,
            QuantError::EncodeLengthMismatch {
                expected: 4,
                got: 6
            }
        );
    }

    #[test]
    #[should_panic(expected = "vector length 2 does not match codec dims 4")]
    fn sq8_encode_par_still_panics_with_typed_message_on_short_row() {
        let codec = Sq8Codec::train(&rand_vecs(10, 4, 1));
        let mut rows = rand_vecs(5, 4, 2);
        rows[3] = vec![0.0, 1.0];
        let _ = codec.encode_par(&rows);
    }

    #[test]
    fn u8_dot_u32_matches_scalar() {
        let a: Vec<u8> = (0u8..=255).take(384).collect();
        let b: Vec<u8> = (0u8..=255).rev().take(384).collect();
        let scalar: u32 = a
            .iter()
            .zip(b.iter())
            .map(|(&x, &y)| x as u32 * y as u32)
            .sum();
        assert_eq!(u8_dot_u32(&a, &b), scalar, "u8_dot_u32 mismatch");
    }

    #[test]
    fn u8_helpers_tail_path_max_diff() {
        for len in [1usize, 7, 15, 17, 100, 383] {
            let a = vec![255u8; len];
            let b = vec![0u8; len];
            assert_eq!(u8_l2sq_u32(&a, &b), len as u32 * 255 * 255, "l2 len={len}");
            assert_eq!(u8_dot_u32(&a, &a), len as u32 * 255 * 255, "dot len={len}");
        }
    }

    #[test]
    fn u8_l2sq_u32_matches_scalar() {
        let a: Vec<u8> = (0u8..=255).take(384).collect();
        let b: Vec<u8> = (0u8..=255).rev().take(384).collect();
        let scalar: u32 = a
            .iter()
            .zip(b.iter())
            .map(|(&x, &y)| {
                let d = (x as i32) - (y as i32);
                (d * d) as u32
            })
            .sum();
        assert_eq!(u8_l2sq_u32(&a, &b), scalar, "u8_l2sq_u32 mismatch");
    }

    #[test]
    #[should_panic(expected = "u8_l2sq_u32 inputs must have equal length")]
    fn u8_l2sq_u32_rejects_shorter_second_slice() {
        let a = [1u8; 16];
        let b = [2u8; 1];

        let _ = u8_l2sq_u32(&a, &b);
    }

    // ── GsSq8Codec tests ────────────────────────────────────────────────────

    /// Regression counterexample (2026-06-12): ranges [0,1] and [0,1e6].
    ///
    /// Without global-scale, the per-dim fast path reversed near/far ordering by >6 OOM.
    /// With GsSq8Codec the global scale is dominated by the wide dim; the narrow dim
    /// loses code resolution but contributes proportionally little to L2 — ordering is preserved.
    #[test]
    fn gs_l2_sq_anisotropic_ordering_preserved() {
        let corpus = vec![
            vec![0.0f32, 0.0f32],    // origin
            vec![1.0f32, 1.0f32],    // near: exact L2² = 2.0
            vec![1.0f32, 4001.0f32], // far: exact L2² ~ 16_000_002
        ];
        let codec = GsSq8Codec::train(&corpus);

        let enc_origin = codec.encode(&corpus[0]);
        let enc_near = codec.encode(&corpus[1]);
        let enc_far = codec.encode(&corpus[2]);

        let d_near = codec.l2_sq(&enc_origin, &enc_near);
        let d_far = codec.l2_sq(&enc_origin, &enc_far);

        assert!(
            d_near < d_far,
            "GsSq8Codec reversed near/far on anisotropic corpus: near={d_near} far={d_far} \
             (anisotropy_ratio={:.1})",
            codec.anisotropy_ratio
        );
    }

    #[test]
    fn gs_l2_sq_isotropic_small_error() {
        let vecs = rand_vecs(200, 64, 202);
        let codec = GsSq8Codec::train(&vecs);
        let encoded: Vec<GsEncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

        let mut max_rel = 0.0f32;
        for i in 0..vecs.len() {
            for j in (i + 1)..vecs.len().min(i + 10) {
                let true_l2 = l2_sq_f32(&vecs[i], &vecs[j]);
                let approx = codec.l2_sq(&encoded[i], &encoded[j]);
                let denom = true_l2.max(1e-6);
                let rel = (approx - true_l2).abs() / denom;
                if rel > max_rel {
                    max_rel = rel;
                }
            }
        }
        assert!(
            max_rel < 0.15,
            "GsSq8Codec max relative L2² error {max_rel:.4} >= 0.15"
        );
    }

    #[test]
    fn gs_train_flat_matches_train_rows() {
        let vecs = rand_vecs(50, 16, 321);
        let flat: Vec<f32> = vecs.iter().flatten().copied().collect();

        let codec_rows = GsSq8Codec::train(&vecs);
        let codec_flat = GsSq8Codec::train_flat(&flat, 16);

        assert!((codec_rows.gs - codec_flat.gs).abs() < 1e-7);
        for d in 0..16 {
            assert!((codec_rows.min[d] - codec_flat.min[d]).abs() < 1e-6);
        }
    }

    #[test]
    fn gs_l2_sq_order_preservation_triplets() {
        let vecs = rand_vecs(300, 64, 303);
        let codec = GsSq8Codec::train(&vecs);
        let encoded: Vec<GsEncodedVector> = vecs.iter().map(|v| codec.encode(v)).collect();

        let n = vecs.len();
        let mut agree = 0usize;
        let mut total = 0usize;

        for anchor in 0..50 {
            let a = &vecs[anchor];
            let ea = &encoded[anchor];
            for b_idx in 0..n {
                for c_idx in (b_idx + 1)..n.min(b_idx + 5) {
                    let dist_ab_true = l2_sq_f32(a, &vecs[b_idx]);
                    let dist_ac_true = l2_sq_f32(a, &vecs[c_idx]);

                    let dist_ab_approx = codec.l2_sq(ea, &encoded[b_idx]);
                    let dist_ac_approx = codec.l2_sq(ea, &encoded[c_idx]);

                    if (dist_ab_true - dist_ac_true).abs() < 0.001 {
                        continue;
                    }

                    let true_closer_b = dist_ab_true < dist_ac_true;
                    let approx_closer_b = dist_ab_approx < dist_ac_approx;
                    if true_closer_b == approx_closer_b {
                        agree += 1;
                    }
                    total += 1;
                }
            }
        }

        let rate = agree as f64 / total.max(1) as f64;
        assert!(
            rate >= 0.95,
            "GsSq8Codec L2 order preservation {rate:.3} < 0.95 ({agree}/{total})"
        );
    }
}
