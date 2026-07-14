# PR #960 eviction coordination fix

## What changed

- Added a cache-wide eviction mutex in `crates/khive-pack-git/src/cache.rs` so overlapping eviction passes are ordered without serializing distinct-key clone or fetch work.
- Changed LRU candidate handling to acquire each candidate's per-slot lock with `try_lock` before reading its marker, sizing its directory, or removing it. Active candidates are deferred instead of blocked on or deleted.
- Revalidates and remeasures a candidate under its slot guard immediately before removal, and propagates ownership-guarded removal failures.
- Added `eviction_defers_a_candidate_with_an_active_slot_mutation`, a deterministic count-cap regression test proving an active slot survives a different key's eviction pass and can complete its mutation.

## Verification

- Red phase: the new regression failed because the uncoordinated eviction deleted the active slot.
- Green phase: the regression and the five existing `evict_lru` tests pass after the fix.
- `cargo fmt --all -- --check`: pass.
- `cargo clippy -p khive-pack-git --all-targets -- -D warnings`: pass.
- `cargo check --workspace`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo test -p khive-pack-git`: pass (77 unit tests, 52 acceptance tests, 0 failures).
- `cargo test --workspace`: compilation was started but not completed within the task deadline; the affected pack suite above is complete.

## Design notes

The lock order is mutation slot lock, then eviction lock, followed only by non-blocking candidate lock probes. An eviction pass never waits for another slot while holding the eviction lock, so concurrent operations cannot form a slot-lock cycle. Because successful mutation paths all finish with a serialized eviction pass, the final overlapping pass sees earlier operations after their slot guards are released and restores the configured caps.

Domain utility: low — the accepted ADR and the concrete cache locking contract fully determined this narrow concurrency fix; no external domain composition was needed.
