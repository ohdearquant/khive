# SQ8 codec reference

Two scalar-quantization codecs in `src/lib.rs` map `f32` vector components to
`u8` codes so distance computation can run on integer hardware paths (NEON on
aarch64, chunked widening multiply elsewhere). Both are trained on a corpus
of vectors and then encode individual vectors against the trained min/scale.

## `Sq8Codec` — per-dimension affine (dot product / cosine)

Each dimension `d` gets its own scale: `scale_d = (max_d - min_d) / 255`.
Encoding: `code_d = round((x_d - min_d) / scale_d)`, clamped to `[0, 255]`.

Fields:

| Field                | Meaning                                                          |
| --------------------- | ----------------------------------------------------------------- |
| `min`                 | per-dimension minimum observed at train time                     |
| `scale`               | per-dimension `(max - min) / 255`                                 |
| `scale_sq`            | `scale²` per dimension, precomputed for L2/dot                    |
| `mean_scale_sq`       | mean of `scale_sq` across dims — the integer-pass multiplier      |
| `scale_sq_residual`   | `scale_sq_i - mean_scale_sq`, zero-mean and small magnitude       |
| `offset_sq_sum`       | `Σ min_i²`, precomputed for the dot-product correction term       |

`EncodedVector` carries, alongside `codes`, the per-vector correction terms
(`norm`, `soc_sum`, `residual_dot_bias`) needed to reconstruct a
full-precision-corrected distance from two encoded vectors without
re-touching the original `f32` data.

### `approx_dot`

Full-precision correction identity (both vectors share one codec's min/scale):

```text
dot(a, b) = Σ scale_i² · a_i · b_i + soc_a + soc_b + offset_sq_sum
```

The integer pass (`u8_dot_u32`) computes `raw = Σ a_i·b_i` as a `u32` (NEON
16-wide on aarch64). The scale correction applies `mean_scale_sq · raw` plus a
compact per-dimension residual `f32` pass (`scale_sq_residual`) for accuracy,
then adds each vector's precomputed `soc_sum` and the shared `offset_sq_sum`.

### `approx_cosine_dist`

`1 - dot / (norm_a * norm_b)`, clamped to `[-1, 1]` before subtracting from 1.
Returns `1.0` (maximally distant) when either norm is zero or the ratio is
non-finite, instead of dividing by zero.

### `approx_l2_sq`

Full-precision identity: `||a-b||² = Σ scale_sq_i · (a_i - b_i)²` — offset
terms cancel because both vectors share the same codec. The integer pass
(`u8_l2sq_u32`) computes `raw = Σ (a_i-b_i)²`; the residual correction keeps
ordinal accuracy across anisotropic corpora. For Vamana's L2 acquisition path
prefer `GsSq8Codec::l2_sq` instead — it is algebraically exact in code space
and roughly 2x faster because it skips the residual pass entirely.

## `GsSq8Codec` — global-scale affine (L2 / Vamana acquisition)

A single shared scale `gs = max_range_across_dims / 255` is used for every
dimension; per-dimension `min_i` offsets are still subtracted before
quantizing, so codes span the full `[0, 255]` range only for the widest
dimension — narrower dimensions get proportionally fewer codes and
proportionally less influence on the L2 sum. That's an intentional,
documented trade-off (see `../design.md`), not an oversight.

```text
||a-b||² ≈ gs² · Σ (a_i - b_i)²
```

This is *exact* in code space (offset terms cancel, `gs²` factorizes) once
the lossy `f32→u8` encode has already happened — but the round-trip error
relative to the true `f32` L2² can reach roughly 15% for anisotropic or
out-of-distribution (OOD) data. Recall safety is established empirically by
probe tests (`gs_l2_sq_anisotropic_ordering_preserved`,
`gs_l2_sq_isotropic_small_error`), not by an exactness argument — there is no
residual pass, no anisotropy gate, and no silent fallback baked into the
codec itself. Callers that need a correctness guarantee on OOD queries must
check `is_in_distribution` and fall back to exact `f32` computation
themselves (see `VamanaIndex::search`).

`anisotropy_ratio` (`max(range_i) / min(nonzero range_i)`, measured at train
time) is informational only — nothing in this crate dispatches on it.

### `is_in_distribution`

Returns `true` when every component of `v` falls inside the trained range
`[min_d, min_d + 255*gs]`, i.e. encoding `v` would clamp no dimension.
`false` means at least one dimension is OOD and the ~15% error bound above
may not hold for that vector.

## `QuantError` and the `try_*` / panicking pairs

Every `train`/`encode` entry point has a `try_*` fallible twin. The
panicking wrapper (e.g. `Sq8Codec::train`) exists only for call sites that
already guarantee valid input and want to skip the `Result`; it calls the
`try_*` variant and panics with the `QuantError`'s `Display` message.

`QuantError` variants:

| Variant                     | Raised when                                                          |
| ---------------------------- | --------------------------------------------------------------------- |
| `EmptyCorpus`                | training corpus has zero rows                                        |
| `ZeroDims`                   | `dims` is zero (flat API) or row 0 is empty (row API)                |
| `FlatLengthNotDivisible`     | a flat vector's length isn't a multiple of `dims`                    |
| `RaggedRow`                  | a training row's length doesn't match the dims fixed by row 0        |
| `EncodeLengthMismatch`       | a vector passed to `encode`/`encode_flat_par` doesn't match trained dims |

### Why the fallible variants exist (QUANT-AUD-002)

Before this audit pass, several of these checks were `debug_assert!`-only,
which compiles out in release builds. Two concrete failure modes motivated
making every one of them a typed, always-checked error:

- `Sq8Codec::encode`/`GsSq8Codec::encode` on a length-mismatched vector could
  silently produce a truncated or malformed code vector instead of panicking,
  in release builds.
- `GsSq8Codec::encode(&[])` on a trained (`dims=1`) codec used to return an
  **empty** code vector in release builds. `is_in_distribution` vacuously
  accepted it (an empty iterator's `all()` is `true`), and `l2_sq` scored the
  comparison as `0.0` — a query that looked like a perfect match by
  accident of an unvalidated shape mismatch.
- `encode_flat_par` divided `vectors.len() / dims` without checking
  `dims != 0` first (a panic on the division) and, when `vectors.len()` was
  not a multiple of `dims`, silently dropped the trailing partial row instead
  of reporting a shape error.

The panicking convenience wrappers (`train`, `encode`, `encode_par`, …) are
kept for existing callers that want a `panic!` on invalid input, but now
panic with the typed `QuantError` message rather than an out-of-bounds index
or a silently wrong result.

## NEON hot-loop helpers

`u8_dot_u32` and `u8_l2sq_u32` are the two inner kernels both codecs share.

- **`u8_dot_u32`**: `Σ a_i * b_i` over `u8` slices, accumulated as `u32`. On
  `aarch64`, uses NEON `vmull_u8` (16-wide `u8→u16` widening multiply per
  iteration, split across 4 accumulator lanes) with a scalar tail loop for
  the remainder. Elsewhere, a chunked (8-wide) portable widening-multiply
  fallback.
- **`u8_l2sq_u32`**: `Σ (a_i - b_i)²` over `u8` slices, accumulated as `u32`.
  On `aarch64`, NEON `vabdq_u8` (absolute difference) feeding `vmull_u8`
  (squaring), same 4-lane accumulation and scalar tail. Elsewhere, the
  portable chunked fallback. Panics if the two slices have different
  lengths (`assert_eq!` at the top — this is a hard programmer-error
  contract, not a caller-input validation path, so it stays a panic rather
  than a `QuantError`).

Both are `#[inline(always)]`; `u8_l2sq_u32` is `pub` (used directly by
`GsSq8Codec::l2_sq` and by callers outside this crate that need the raw
integer kernel), `u8_dot_u32` is private to this module.

Measured cost: the NEON `l2_sq` path runs roughly 13 ns at 384 dimensions.
