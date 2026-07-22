# Contributing to khive

khive is developed internally by its maintainers and **does not accept external
contributions**. External pull requests, issues, and review comments are not
monitored and may be closed without review; repository interactions are limited
to collaborators.

The source is published for transparency and for building on top of khive under
its license terms — use it, fork it, embed it. If you believe you have found a
security issue, contact the maintainers privately rather than opening a public
issue.

## Building from source

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

## For maintainers

Design decisions are recorded as design records maintained outside this
repository. Non-trivial changes to crate boundaries, public APIs, or the pack
system need an approved design record before code lands. Feature branches and
pull requests only; never push directly to `main`.

### Code style

- Follow standard Rust idioms. Clippy with `-D warnings` is enforced in CI.
- Keep public API surface minimal and well-documented.
- Avoid `unwrap()` in library code; propagate errors with `thiserror` types.
- New packs must implement `PackRuntime` and register via `inventory::submit!`.

## License

This project is licensed under the [Business Source License 1.1](LICENSE). By submitting a contribution, you grant the Licensor a perpetual, irrevocable, worldwide, royalty-free license to use, reproduce, modify, distribute, sublicense, and relicense your contribution, including the right to license it commercially and to designate future Change Dates and Change Licenses for it under the Business Source License or any other license. You represent that you have the right to grant this license for your contribution.
