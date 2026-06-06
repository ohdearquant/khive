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
