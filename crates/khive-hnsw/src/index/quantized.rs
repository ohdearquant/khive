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
/// Flat `Vec<i8>` arena parallel to `nodes`; used for approximate candidate filtering (~3x faster).
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

    /// Quantize a float vector and append it; symmetric `[-max_abs, max_abs]` → `[-127, 127]`.
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

    /// Compute approximate INT8 dot product, returning result in f32 scale.
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

    /// Compute approximate INT8 cosine distance; returns `1 - cosine_similarity`.
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

/// Raw INT8 dot product via SIMD; returns unscaled f32 result (caller handles scale factor).
#[inline]
pub(crate) fn int8_dot_product_raw(a: &[i8], b: &[i8]) -> f32 {
    lattice_embed::simd::dot_product_i8_raw(a, b)
}
