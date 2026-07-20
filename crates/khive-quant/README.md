# khive-quant

SQ8 scalar quantization codecs for approximate distance computation in ANN
indexes. Two codecs, chosen by which distance metric the index needs:
`Sq8Codec` (per-dimension affine scale, for dot product / cosine) and
`GsSq8Codec` (global shared scale, for L2 — the Vamana acquisition path).

## Usage

```rust
use khive_quant::Sq8Codec;

let corpus: Vec<Vec<f32>> = vec![
    vec![0.1, 0.9, 0.4],
    vec![0.8, 0.2, 0.6],
];

let codec = Sq8Codec::train(&corpus);
let encoded: Vec<_> = corpus.iter().map(|v| codec.encode(v)).collect();

let dot = codec.approx_dot(&encoded[0], &encoded[1]);
let cosine_dist = codec.approx_cosine_dist(&encoded[0], &encoded[1]);
```

`train` / `train_flat` compute per-dimension `min`/`max` from the corpus and
derive `scale_i = (max_i - min_i) / 255`; `encode` maps each `f32` dimension to
a `u8` code via `round((x - min_i) / scale_i)`. `encode_par` / `encode_flat_par`
parallelize encoding across a batch with `rayon`. `approx_dot`,
`approx_cosine_dist`, and `approx_l2_sq` reconstruct the original-scale
distance from `u8` codes using a residual-corrected integer pass, preserving
ordinal ranking against the exact `f32` computation.

## GsSq8Codec — the Vamana acquisition path

`GsSq8Codec` uses one shared scale `gs = max_range_across_dims / 255` for every
dimension (per-dim `min_i` offsets are still subtracted before quantizing).
This makes squared L2 in code space `gs² * sum((a_i - b_i)^2)` algebraically
exact after the lossy `f32` -> `u8` encode — the offset terms cancel and `gs²`
factorizes out, so `GsSq8Codec::l2_sq` needs no residual pass or anisotropy
gate. The trade-off is honest, not hidden: narrow-range dimensions get fewer
`u8` levels and contribute proportionally less L2 signal than the wide-range
dimensions that set `gs`. `GsSq8Codec::is_in_distribution` flags query vectors
whose components fall outside the trained range so a caller can fall back to
exact `f32` distance for out-of-distribution queries — see
`VamanaIndex::search`.

## Hot-loop kernels

`u8_dot_u32` and `u8_l2sq_u32` are the shared inner loops both codecs use:
`u8_dot_u32` computes `sum(a_i * b_i)` as a `u32` accumulator via NEON
`vmull_u8` (aarch64) or a chunked portable widening fallback elsewhere;
`u8_l2sq_u32` computes `sum((a_i - b_i)^2)` via NEON `vabdq_u8` + `vmull_u8`
squaring, or the equivalent portable fallback.

## Where this sits

Built on `rayon` only — no khive-* dependencies. Consumed today by
[khive-vamana](https://crates.io/crates/khive-vamana) for its SQ8-quantized
acquisition path. Governed by
[ADR-052](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-052-ann-production-lifecycle.md),
which documents why the predecessor per-dimension L2 codec (with an
anisotropy gate calibrated on a synthetic corpus) silently fell back to a full
residual pass on real transformer embeddings, and why the global-scale design
eliminates the gate entirely.

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
