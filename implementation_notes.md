# Implementation notes

- Kept stage 4's widened hybrid-search pool for decisiveness checks, then truncated non-decisive `Ambiguous` candidates to the effective caller limit before returning from `resolve_reference`.
- Added an end-to-end `khive-pack-kg` dispatch regression proving serialized stage-4 candidate arrays honor both `limit=1` and the default limit of 5.
- Reconciled the existing deep-search regression with the public API contract: it now verifies that a below-limit target exists in the widened decision pool but does not leak into the bounded ambiguity payload.

## Verification

- `cargo fmt --all -- --check` — passed.
- `cargo test -p khive-runtime --lib` — 851 passed, 5 ignored.
- `cargo test -p khive-pack-kg --lib` — 145 passed.
- `cargo clippy -p khive-runtime -p khive-pack-kg --all-targets -- -D warnings` — passed.
- `cargo check --workspace` — passed.
- A combined all-target package test attempt was killed by the host with exit 137 during linking; the affected lib suites and focused resolver/dispatch regressions were then run separately and passed.

Domain utility: MEDIUM — repository-local interface and regression-testing guidance reinforced testing the serialized boundary, while the code and documented `limit` contract determined the implementation.
