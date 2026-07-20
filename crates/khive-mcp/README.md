# khive-mcp

The khive MCP server library — a single `request` tool that parses the verb-dispatch
DSL and routes each operation through a `VerbRegistry`.

This crate ships no binary of its own; [`kkernel`](https://crates.io/crates/kkernel)'s
`mcp` subcommand is the frontend that builds a `KhiveMcpServer` from CLI args and serves
it. `khive-mcp` owns the server type, the transports it can be served over, the pack
force-linking, and the daemon protocol — `kkernel` owns argument parsing entry and process
lifetime.

## Features

- **One tool, `request`** — `RequestParams { ops, presentation, format, save_to, .. }` is
  the entire MCP-visible surface (ADR-016); verb-specific schemas live in packs, not here
- **Pluggable transports** — `Transport` trait + `TransportRegistry`; ships `StdioTransport`,
  open for more (e.g. Streamable HTTP) via `TransportRegistry::register`
- **Daemon-aware dispatch** — `compute_config_id` fingerprints a resolved `RuntimeConfig`
  (packs, db target, embedders, backend routing, outbound policy) so a thin
  client only forwards to a warm daemon (ADR-049) when the fingerprints match;
  otherwise it falls back to local dispatch
- **Result sinking** — `RequestParams::save_to` writes results as JSONL and returns a
  manifest (`path`, `rows`, `per_column_null_counts`, `schema_fingerprint`, `checksum`)
  instead of inlining a large result set
- **Cross-backend coordinator seam** — `CoordinatorService` is a trait khive-mcp defines
  and `kkernel` implements, avoiding a dependency cycle for multi-backend link/traverse

## Usage

```rust
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, RuntimeConfig};

let runtime = KhiveRuntime::new(RuntimeConfig::default()).expect("valid config");
let server = KhiveMcpServer::new(runtime).expect("known packs, deps satisfied");
```

`KhiveMcpServer::new` builds the server from `runtime.config().packs`; `with_packs` takes
an explicit pack list instead. Both fail fast with `PackRegError` (naming the unknown pack
or missing dependency) rather than silently dropping packs. Once built, `serve_stdio(self)`
consumes the server and serves over stdio — the path `StdioTransport::serve` and `kkernel
mcp` both call.

## Where this sits

`khive-mcp` depends on `khive-db`, `khive-runtime`, `khive-storage`, `khive-request`, and
every first-party pack crate (`khive-pack-kg`, `-gtd`, `-memory`, `-brain`, `-comm`,
`-schedule`, `-session`) so their `inventory::submit!` verb registrations link
into any binary that depends on this crate. `kkernel` is that binary: its `mcp` subcommand
parses `khive_mcp::args::Args`, builds the runtime and pack registry, and calls into
`khive_mcp::serve::run`.

Governed by [ADR-016](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md)
(the `request` tool contract) and [ADR-049](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-049-khived-daemon.md)
(the warm daemon protocol this crate's client/daemon config-fingerprint matching supports).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
