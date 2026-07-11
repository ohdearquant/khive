# Fix summary — PR #854 codex fix round

## HIGH — `Sq8Codec::encode_par` unchecked panic path

`encode_par` mapped each row through `self.encode`, which unwraps
`try_encode` and panics on a length-mismatched row after already
dispatching work to the rayon thread pool.

- Added `Sq8Codec::try_encode_par(&self, vectors: &[Vec<f32>]) -> Result<Vec<EncodedVector>, QuantError>`
  (`crates/khive-quant/src/lib.rs`). It validates every row's length
  against the codec's trained dims *before* handing anything to
  `par_iter`, returning `QuantError::EncodeLengthMismatch` on the first
  bad row instead of panicking mid-batch.
- `encode_par` is now a thin panicking wrapper: `self.try_encode_par(vectors).unwrap_or_else(|e| panic!("{e}"))`,
  documented as such (mirrors the existing `train`/`try_train`,
  `encode`/`try_encode`, `encode_flat_par`/`try_encode_flat_par` pairs).
- `GsSq8Codec` has no `encode_par` (only `encode_flat_par`, which was
  already fallible from the original PR), so no change needed there.

### Regression tests added

- `sq8_try_encode_par_short_row_returns_error_not_panic` — a row shorter
  than the trained dims returns `EncodeLengthMismatch` instead of
  panicking.
- `sq8_try_encode_par_long_row_returns_error_not_panic` — a row longer
  than the trained dims returns `EncodeLengthMismatch`.
- `sq8_encode_par_still_panics_with_typed_message_on_short_row` — confirms
  the panicking `encode_par` wrapper still panics, now with the typed
  `QuantError` message instead of an out-of-bounds/index panic.

## LOW — em dashes in added doc comments

Replaced all 14 em dashes (`—`) introduced by this PR's added doc
comments in `crates/khive-quant/src/lib.rs` with ASCII punctuation
(periods + "See", semicolons, or commas), across both `Sq8Codec` and
`GsSq8Codec` doc blocks (`train_flat`, `train`, `encode`,
`encode_flat_par`, and the new `encode_par`/`try_encode_par` docs).
Pre-existing em dashes elsewhere in the file (module-level docs, section
banners, math notation predating this PR) were left untouched — out of
this PR's diff scope.

## Verification (scoped, run from `crates/`)

- `cargo fmt -p khive-quant` — clean.
- `cargo clippy -p khive-quant --all-targets -- -D warnings` — clean.
- `cargo test -p khive-quant` — 34 unit tests pass (31 prior + 3 new),
  plus the existing integration test suites.
- `cargo check -p khive-vamana` — clean (downstream consumer, unaffected
  by the API-additive change).
