# PR #975 implementation notes

## Change

- Relaxed the `tx_age_sweep_own_entry_survives_concurrent_older_registration`
  setup assertion so it verifies the test's own span is not the process-wide
  oldest entry without assuming the local decoy is globally oldest.
- Updated the regression-test documentation to describe the interleaving-safe
  invariant. The decoy handle remains live for the whole test.

## Verification

- `cargo fmt --manifest-path crates/Cargo.toml --all -- --check` — passed.
- `cargo check --manifest-path crates/Cargo.toml --workspace` — passed.
- `cargo clippy --manifest-path crates/Cargo.toml --workspace --all-targets -- -D warnings` — passed.
- `cargo test --manifest-path crates/Cargo.toml -p khive-db tx_age_sweep_ -- --nocapture` — 10 passed.
- `cargo test --manifest-path crates/Cargo.toml -p khive-db` — 345 passed across unit and contract tests.
- `cargo test --manifest-path crates/Cargo.toml --workspace` — compiled successfully and began running; the environment terminated it with exit 137 after the first crate's 58 tests passed. No test assertion failed before termination.

## Domain utility

`medium` — the concurrency-test briefing reinforced that the regression oracle
must express an interleaving-independent invariant; the repository source and
review finding determined the exact code change.
