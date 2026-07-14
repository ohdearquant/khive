# PR #860 round-5 implementation notes

- `crates/khive-pack-git/src/write_handlers.rs` now records supplementary write audits for all validation and detached-HEAD failures after the repository argument is parseable.
- `crates/khive-pack-git/src/write_argv.rs` internally prefixes commit paths with Git's `:(literal)` pathspec magic while retaining relative-path, traversal, NUL, and size checks.
- `crates/khive-mcp/src/serve.rs` now loads and validates non-embedding config for explicit namespaces, preserving `[git_write]` while CLI actor/namespace precedence remains authoritative.
- Handler, argv, and bootstrap regressions cover force denial, malformed paths, invalid refs, detached HEAD, literal magic/special/Unicode filenames, and no-embed explicit-actor policy propagation.
- Public docs describe the mandatory fail-closed handler allowlist and use neutral example identities and paths.

Verification completed from `crates/`:

- `cargo fmt --all`
- `cargo test -p khive-pack-git -p khive-runtime -p khive-mcp`
- `cargo clippy -p khive-pack-git -p khive-runtime -p khive-mcp --all-targets -- -D warnings`
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p khive-pack-git`

Domain utility: low. The composed GitOps audit domain reinforced complete audit trails, but the ADR and repository contracts determined the concrete implementation.
