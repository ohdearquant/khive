# khive-quant design

Rationale and incident history behind the SQ8 codecs in `src/lib.rs`. For the
function-by-function technical reference (formulas, fields, error taxonomy),
see `api/codecs.md`.

## `GsSq8Codec` — why global-scale, not per-dimension anisotropy gating

The predecessor per-dimension codec required an `approx_l2_sq_fast` path plus
an anisotropy gate (ratio ≤ 4.0) to reach an integer-only hot path. The gate
was calibrated on an LCG-generated synthetic corpus that happened to produce
ratio ≈ 4.0 across dimensions. Real transformer embeddings have rogue
dimensions with ratio 10–32, which silently tripped the gate and fell back to
the full residual-correction path on every comparison — defeating the entire
point of the fast path without any visible error or degradation, just lost
throughput.

`GsSq8Codec` eliminates the gate entirely by using a single global scale
across all dimensions, at the cost of an honest, bounded accuracy trade-off
on narrow dimensions (documented in `api/codecs.md`). See ADR-052 for the
full design decision record.

## QUANT-AUD-002 — typed validation instead of debug-only assertions

Several `try_*` fallible entry points (`try_encode`, `try_encode_flat_par`,
`try_encode_par`, and the corresponding `try_train*` constructors) were added
to replace `debug_assert!`-only shape checks that compiled out in release
builds. The concrete bug this closed: `GsSq8Codec::encode(&[])` on a trained
1-dimensional codec used to return an empty code vector in release builds,
which `is_in_distribution` vacuously accepted (`all()` over an empty iterator
is `true`) and `l2_sq` then scored as an exact match (`0.0`) — a
shape-mismatched query silently looked like a perfect hit. All such paths now
return `QuantError` instead of producing a malformed or truncated result. See
`api/codecs.md` for the full `QuantError` variant list and per-function
detail.
