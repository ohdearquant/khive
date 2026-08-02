Verdict: APPROVE-WITH-FIXES
Findings: 0 Blocker, 0 High, 0 Medium, 1 Low

### [Low] Format the new cache-test helpers

Evidence: `crates/khive-pack-git/src/cache.rs:773` and `crates/khive-pack-git/src/cache.rs:783` contain single-line `assert!` calls that `cargo fmt --all -- --check` rewrites.

Why this matters: the repository requires rustfmt as a deterministic quality gate; the current PR fails that gate.

Suggested fix: run `cargo fmt --all` and include the resulting formatting-only changes.

## Looks Right

- The cache reset is limited to ownership-proven scratch clones and runs while the existing same-slot lock is held (`cache.rs:230-246`, `cache.rs:265-297`). The cache module is the only production mutator of those slots; the slot remains disposable rather than user state.
- `remote set-head` is best-effort, but the subsequent reset is a hard error, so a missing/unresolvable `refs/remotes/origin/HEAD` cannot silently proceed on stale history (`cache.rs:421-454`).
- The empty-walk guard executes only with a non-empty stored cursor, so a legitimate first walk remains unaffected; an unknown cursor SHA correctly reads as non-ancestral (`ingest.rs:985-1006`, `ingest.rs:1179-1196`).
- PR and issue paths freeze their cursor before later records and independently force `done = false` when stalled (`ingest.rs:1563-1728`, `ingest.rs:1745-1918`).
- The tri-state `gh_available` serializes as the intended `true` / `false` / `null` states. No in-repository consumer other than the updated kkernel human formatter reads the field; ADR-088 names it only as part of the report shape.
- Both new regressions fail against `main` and pass on this branch.

## Commands Run

- `git diff main...HEAD`, changed-file and consumer searches: reviewed the full scoped diff and adjacent contracts.
- `cargo fmt --all -- --check`: failed only at `cache.rs:773` and `cache.rs:783`.
- `CARGO_TARGET_DIR=/private/tmp/pr1646-head-target cargo clippy -q -p khive-pack-git --all-targets -- -D warnings`: passed.
- `CARGO_TARGET_DIR=/private/tmp/pr1646-head-target cargo check -p kkernel --quiet`: passed.
- `CARGO_TARGET_DIR=/private/tmp/pr1646-head-target cargo test -q -p khive-pack-git --lib`: 215 passed.
- `CARGO_TARGET_DIR=/private/tmp/pr1646-head-target cargo test -q -p khive-pack-git --test acceptance`: 56 passed.
- Baseline-only injected regressions: both failed on `main` (stale checkout remained at commit A; non-ancestor empty walk returned `done: true`) and passed on this branch.

## What I Did Not Check

- Full workspace test suite and live GitHub remote behavior.

## Re-Review Guidance

Narrow re-review after the rustfmt-only fix is sufficient.

Domain utility: MEDIUM — the Rust review guidance reinforced the Option/Result boundary check, while repository contracts and tests supplied the decisive evidence.

Khive write-back: decision `7f0e9c49-3d88-40dc-ad56-7539d979b4e8`; edge `c5c4ac82-96a0-4fbe-a339-abb0db33a58d`; memory `db4b3981-431b-4849-a593-ac43c83b9364` (edge `284c5748`); product-feedback observation `378cfe02-d30e-4c89-a949-be74d3a21b37`; feedback event `e7cc7bd4-d033-47b7-9845-4746453e2305`.
