# Design notes

Background for maintainers on design decisions behind the public codecs in
`src/lib.rs`. The doc-comments on those types carry the complete API contract
already — this file is historical/rationale context only.

## `GsSq8Codec` — why global-scale, not per-dimension anisotropy gating

The predecessor per-dim codec required `approx_l2_sq_fast` plus an anisotropy
gate (ratio ≤ 4.0) to achieve the integer-only hot path. The gate was
calibrated on an LCG corpus that gave ratio ≈ 4.0; real transformer embeddings
have rogue dimensions (ratio 10–32) that silently fell back to the full
residual path, defeating the purpose. Global-scale eliminates the gate
entirely — see ADR-052.

Source: `crates/khive-quant/src/lib.rs`.
