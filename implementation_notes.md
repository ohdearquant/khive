# Implementation notes

- Added an order-preserving SHA-256 fingerprint of `RuntimeConfig::git_write.allowed` to the shared daemon `config_id` builder in `crates/khive-mcp/src/server.rs`.
- Normalized absent and explicitly empty git-write sections through their shared empty `RuntimeConfig` representation, matching the fail-closed policy semantics.
- Added deterministic/change/order regression coverage in `crates/khive-mcp/tests/integration.rs` and a live daemon rejection/fallback regression in `crates/khive-mcp/src/daemon.rs`.

## Verification

- `cargo fmt --all`
- `cargo test -p khive-mcp` (217 library tests and 115 integration tests passed)
- `cargo clippy -p khive-mcp --all-targets -- -D warnings`

Domain utility: low. Repository ADRs, source contracts, and existing daemon tests determined the implementation; composed typestate guidance was not materially relevant.
