# PR #972 implementation notes

- Updated `crates/khive-mcp/src/serve.rs` so multi-backend construction resolves the database anchor once, then validates the runtime configuration against that captured value. This removes the `HOME` time-of-check/time-of-use window while preserving the existing mismatch error and `:memory:` behavior.
- Replaced the ambient-`HOME` and guard-characterization tests with a deterministic regression that changes `HOME` immediately after anchor capture and exercises the duplicate-SQLite-path server construction. Reintroducing an internal anchor re-read makes this test fail.
- Kept the change internal to the MCP bootstrap; no MCP wire, storage schema, or public function signature changed.

Verification:

- `cargo test -p khive-mcp duplicate_sqlite_paths_use_anchor_captured_before_home_changes -- --nocapture` — passed.
- Removal check with the old anchor re-read restored temporarily — failed on the two deliberately different `HOME` anchors, as intended.
- `cargo test -p khive-mcp` — 329 tests passed.
- `cargo clippy -p khive-mcp --all-targets -- -D warnings` — passed.
- `cargo fmt --all` — completed; check-mode verification passed before commit.

Domain utility: medium. The state-isolation briefing reinforced treating process environment as mutable shared state and passing the resolved dependency through the construction boundary.
