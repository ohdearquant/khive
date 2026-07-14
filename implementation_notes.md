# PR #963 Round 4 implementation notes

- Removed the confirmed-respawn-failure bridge trace's executable-path field, free-form detail, and `khived.log` tail read from `crates/khive-mcp/src/daemon.rs`.
- Replaced raw diagnostics with typed `spawn_error` / `exited_before_bind` categories, the stable `respawn_failed` reason, and optional numeric OS/exit codes.
- Extended strict and non-strict regression coverage to capture formatted tracing events as well as serialized `McpError` output, proving seeded daemon-log sentinels and the absolute executable path reach neither channel.
- Preserved ADR-049 Amendment 2 behavior: confirmed respawn failures still reject in both modes, with no change to fallback tiering or the strict-mode marker.

Verification from `crates/`:

- `cargo fmt --all`
- `cargo test -p khive-mcp` — 215 unit + 113 integration tests passed
- `cargo clippy -p khive-mcp --all-targets -- -D warnings`

Domain utility: MEDIUM — the composed Rust observability guidance supported structured event capture; ADR-049 Amendment 2 and the review finding determined the security boundary and rejection semantics.
