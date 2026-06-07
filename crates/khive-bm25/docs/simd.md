# BM25 SIMD Scoring

The brute-force scoring path uses architecture-specific SIMD to process postings in parallel.

## Platform Support

- **aarch64 (NEON)**: 4-wide batches using 128-bit NEON registers.
- **x86_64 (AVX2)**: 8-wide batches using 256-bit YMM registers, with optional FMA for fused
  multiply-add in the denominator computation. Detected at runtime via
  `is_x86_feature_detected!`.
- **Scalar fallback**: Used on all other targets or when AVX2 is not available at runtime.

## Dispatch Strategy

The SIMD/scalar dispatch happens once per term (not per batch) to avoid repeated feature checks in
the hot loop. For large posting lists (above `WAND_THRESHOLD`), Block-Max WAND skipping is
preferred over brute-force scoring regardless of SIMD availability.

See `src/index/search/simd.rs` for implementation.

## Why Not Reuse lattice-embed SIMD?

`lattice-embed` has highly optimized SIMD kernels (dot product, cosine, L2 distance) used by
`khive-hnsw`. These are reduction-oriented (two vectors → one scalar) and operate on f32/i8
inputs. BM25 scoring is lane-preserving (4/8 postings → 4/8 independent scores), starts from
u8 term frequencies, and computes a fused rational formula — structurally incompatible.

## Future: AVX-512

If AVX-512 support (16-wide) is added, adopt the `OnceLock<fn_ptr>` runtime dispatch pattern
from `lattice-embed::simd` (~30 LOC) instead of the current compile-time `#[cfg(target_arch)]`
guards. AVX-512 requires runtime feature detection since it cannot be assumed on all x86_64.
