//! INT8 quantized vector arena for fast approximate distance computation.

/// Per-vector quantization metadata (symmetric quantization).
///
/// Stored alongside the flat `Vec<i8>` arena. Each vector's quantized data
/// is at `[internal_id * dims .. (internal_id + 1) * dims]` in the arena.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QuantMeta {
    /// Scale factor: `float_value = int8_value / scale`.
    /// Symmetric quantization maps `[-max_abs, max_abs]` to `[-127, 127]`.
    pub scale: f32,
    /// Pre-computed L2 norm of the original f32 vector.
    pub norm: f32,
}

/// INT8 quantized vector arena for HNSW search acceleration.
///
/// Stores quantized vectors in a flat `Vec<i8>` arena with the same ordering
/// as the main `nodes` vector. Used for fast approximate distance computation
/// during the candidate filtering phase of search.
///
/// # Two-Phase Search Strategy
///
/// 1. **Phase 1 (INT8)**: Compute approximate distance using quantized vectors.
///    This is ~3x faster than f32 distance computation (11ns vs 34ns for 384d).
/// 2. **Phase 2 (f32)**: For candidates that pass the approximate threshold,
///    compute precise f32 distance for final ranking.
///
/// This skip pattern avoids f32 distance computation for obviously distant
/// neighbors, providing significant speedup at scale (50K+ vectors).
#[derive(Debug, Clone)]
pub(crate) struct QuantizedArena {
    /// Flat INT8 vector data. Vector `i` starts at `i * dims`.
    pub data: Vec<i8>,
    /// Per-vector quantization metadata, indexed by internal ID.
    pub meta: Vec<QuantMeta>,
    /// Vector dimensionality (cached for bounds checking).
    pub dims: usize,
}

impl QuantizedArena {
    /// Create a new empty quantized arena for the given dimensionality.
    pub(crate) fn new(dims: usize) -> Self {
        Self {
            data: Vec::new(),
            meta: Vec::new(),
            dims,
        }
    }

    /// Quantize a float vector and append it to the arena.
    ///
    /// Uses symmetric quantization: `[-max_abs, max_abs]` -> `[-127, 127]`.
    /// Returns the index of the newly added vector (should match the internal ID).
    pub(crate) fn push(&mut self, vector: &[f32], norm: f32) -> usize {
        debug_assert_eq!(vector.len(), self.dims);

        // Single-pass min/max over finite values
        let mut max_abs: f32 = 0.0;
        for &v in vector {
            if v.is_finite() {
                let abs = v.abs();
                if abs > max_abs {
                    max_abs = abs;
                }
            }
        }

        // Symmetric quantization: scale maps max_abs to 127
        let scale = if max_abs > 1e-10 {
            127.0 / max_abs
        } else {
            1.0 // Near-zero vector
        };

        // Quantize and append to flat arena
        self.data.reserve(self.dims);
        for &v in vector {
            let q = if v.is_finite() {
                (v * scale).round().clamp(-127.0, 127.0) as i8
            } else {
                0i8
            };
            self.data.push(q);
        }

        let idx = self.meta.len();
        self.meta.push(QuantMeta { scale, norm });
        idx
    }

    /// Update the quantized vector at the given index.
    pub(crate) fn update(&mut self, idx: usize, vector: &[f32], norm: f32) {
        debug_assert_eq!(vector.len(), self.dims);
        debug_assert!(idx < self.meta.len());

        let mut max_abs: f32 = 0.0;
        for &v in vector {
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

        let offset = idx * self.dims;
        for (i, &v) in vector.iter().enumerate() {
            self.data[offset + i] = if v.is_finite() {
                (v * scale).round().clamp(-127.0, 127.0) as i8
            } else {
                0i8
            };
        }

        self.meta[idx] = QuantMeta { scale, norm };
    }

    /// Get the quantized data slice for a given internal ID.
    #[inline]
    pub(crate) fn get_data(&self, idx: usize) -> &[i8] {
        let offset = idx * self.dims;
        &self.data[offset..offset + self.dims]
    }

    /// Compute approximate INT8 dot product between two quantized vectors,
    /// returning the result in the original f32 scale.
    ///
    /// Uses SIMD-accelerated INT8 dot product from khive-embed.
    #[inline]
    #[allow(dead_code)] // Available for Dot metric path (future)
    pub fn dot_product_approx(&self, a_idx: usize, b_data: &[i8], b_scale: f32) -> f32 {
        let a_data = self.get_data(a_idx);
        let a_meta = &self.meta[a_idx];
        let denom = a_meta.scale * b_scale;
        if denom == 0.0 || !denom.is_finite() {
            return 0.0;
        }
        int8_dot_product_raw(a_data, b_data) / denom
    }

    /// Compute approximate INT8 cosine distance between a stored vector and
    /// a query's quantized form.
    ///
    /// Returns distance (1 - cosine_similarity), comparable to the f32 path.
    #[inline]
    pub fn cosine_distance_approx(
        &self,
        idx: usize,
        query_i8: &[i8],
        query_scale: f32,
        query_norm: f32,
    ) -> f32 {
        let meta = &self.meta[idx];
        let denom_scale = meta.scale * query_scale;
        if denom_scale == 0.0 || !denom_scale.is_finite() {
            return 1.0;
        }
        let norm_denom = meta.norm * query_norm;
        if norm_denom <= 0.0 || !norm_denom.is_finite() {
            return 1.0;
        }
        let dot = int8_dot_product_raw(self.get_data(idx), query_i8) / denom_scale;
        1.0 - (dot / norm_denom)
    }

    /// Clear the arena (used by rebuild/clear).
    pub(crate) fn clear(&mut self) {
        self.data.clear();
        self.meta.clear();
    }
}

/// Raw INT8 dot product using SIMD from khive-embed.
///
/// Zero-allocation path: takes raw `&[i8]` slices and returns the integer
/// dot product as f32 (no scale factor division). The caller handles scaling.
///
/// Uses the same SIMD backend as `dot_product_i8` (NEON/AVX2/AVX-512 VNNI)
/// but without constructing `QuantizedVector` wrappers.
#[inline]
pub(crate) fn int8_dot_product_raw(a: &[i8], b: &[i8]) -> f32 {
    lattice_embed::simd::dot_product_i8_raw(a, b)
}
