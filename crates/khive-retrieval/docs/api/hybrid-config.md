# `HybridConfig` — validation reference

`HybridConfig` (in `src/hybrid/config.rs`) controls fusion strategy, pool sizing, and
vector/keyword weighting for a hybrid search call. It deserializes through
`RawHybridConfig` + `TryFrom` so that malformed wire input can never produce a config
that panics or corrupts scoring downstream.

## Non-finite weight rejection

`vector_weight`/`keyword_weight` must be finite. Two independent layers guard this:

- **`serde_json` itself** refuses any JSON number literal it cannot represent as an
  `f64` (a bare `NaN`/`Infinity` token is not valid JSON, and `1e400` overflows `f64`)
  before `TryFrom<RawHybridConfig>` ever runs. This holds even for a plain `f64` field
  with no custom validation at all, so a test asserting only "malformed JSON is
  rejected" does not prove `TryFrom` is wired up — it proves `serde_json`'s own
  parser is.
- **`TryFrom<RawHybridConfig>`** is the real regression guard: `serde_json` cannot
  encode a literal NaN at all (`Number::from_f64(f64::NAN)` returns `None`), so the
  only way to drive a genuine non-finite value into the boundary is to construct
  `RawHybridConfig` directly with `f64::NAN`/`f64::INFINITY` and call `TryFrom` on it.
  Before this guard existed, the old plain-derive `Deserialize` accepted such a value
  silently (only a `debug_assert!` in the unrelated `with_weights` builder caught it,
  so release builds did not).

Test coverage: `test_serde_json_rejects_{nan,infinity}_literal_vector_weight` document
the (weaker) parser-level rejection; `test_try_from_rejects_nan_vector_weight` and its
infinity counterpart are the real `TryFrom` boundary tests, in `hybrid/config.rs`'s
`#[cfg(test)]` module.
