# PR #972 round 3 implementation notes

- Captured the HOME-derived database anchor once alongside `RuntimeConfig` resolution and threaded it through normal MCP boot, coordinator-attached `kkernel mcp` boot, and multi-backend registry validation.
- Changed `assert_db_anchor_consistent` to compare against the caller-provided captured path, eliminating its process-environment re-read. Existing programmatic construction helpers retain their signatures and derive the anchor from the already-resolved `RuntimeConfig`, never from HOME.
- Added deterministic regressions for both boot paths. Each resolves the runtime config under one HOME, changes HOME before registry construction, and verifies boot succeeds with the original anchor. Both tests failed against the pre-fix implementation and pass after the change.
- Updated `kkernel exec` to use the same captured-anchor validator contract, so no retained validator call re-resolves HOME.

Verification from `crates/`:

- `cargo fmt --all` and `cargo fmt --all -- --check` — passed.
- `cargo test -p khive-mcp` — 329 tests passed.
- `cargo test -p kkernel coordinator_boot_uses_anchor_captured_by_runtime_config -- --nocapture` — passed.
- `cargo test -p khive-runtime assert_db_anchor_consistent_tests` — 4 tests passed.
- `cargo clippy -p khive-mcp --all-targets -- -D warnings` — passed.
- `cargo clippy -p kkernel --all-targets -- -D warnings` — passed.
- `git diff --check` — passed.

Domain utility: medium. The composed Rust testing/runtime briefing reinforced isolating mutable process state in deterministic fixtures; repository source and the review trace supplied the concrete API design.
