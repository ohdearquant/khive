# Contributing to khive

## Getting Started

### Prerequisites

- Rust toolchain (stable, via [rustup](https://rustup.rs/))
- Deno (for CLI and documentation tools)

### Build

```sh
cd crates
cargo build --workspace
```

### Test

```sh
# Rust tests
cd crates
cargo test --workspace

# Deno CLI tests
cd cli
deno task test
```

### Lint and Format

```sh
# Rust
cd crates
cargo fmt --all
cargo clippy --workspace -- -D warnings

# Deno / docs
deno fmt docs/
```

### CI

The full CI pipeline is in `scripts/ci.sh` and can be run locally with:

```sh
make ci
```

Individual targets: `make check`, `make clippy`, `make test`, `make fmt`.

## Pull Request Workflow

1. Fork the repository and create a feature branch from `main`.
2. Make your changes. All new public APIs require tests.
3. Ensure `cargo test --workspace` and `cargo clippy --workspace -- -D warnings` pass.
4. Open a pull request against `main`. Describe what changed and why.

## Architecture

Design decisions are recorded as Architecture Decision Records (ADRs) in
[`docs/adr/`](docs/adr/). Read `docs/adr/README.md` for an index and the
ADR format. Non-trivial changes to crate boundaries, public APIs, or the
pack system should be accompanied by a new or updated ADR.

## Code Style

- Follow standard Rust idioms. Clippy with `-D warnings` is enforced in CI.
- Keep public API surface minimal and well-documented.
- Avoid `unwrap()` in library code; propagate errors with `thiserror` types.
- New packs must implement `PackRuntime` and register via `inventory::submit!`.

## License

Contributions are accepted under the [Apache-2.0 license](LICENSE).
