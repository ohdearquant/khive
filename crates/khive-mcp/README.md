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
  the entire MCP-visible surface (ADR-016); its outer envelope is closed to undeclared
  fields, while verb-specific schemas live in packs
- **Pluggable transports** — `Transport` trait + `TransportRegistry`; ships `StdioTransport`,
  open for more (e.g. Streamable HTTP) via `TransportRegistry::register`
- **Daemon-aware dispatch** — `compute_config_id` fingerprints a resolved `RuntimeConfig`
  plus captured serving policy (packs, db target, embedders, fresh-tail policy,
  backend routing, outbound policy) so a thin client only forwards to a warm daemon
  (ADR-049) when the fingerprints match;
  otherwise it falls back to local dispatch
- **Result sinking** — `RequestParams::save_to` writes results as JSONL and returns a
  manifest (`path`, `rows`, `per_column_null_counts`, `schema_fingerprint`, `checksum`,
  `summary`, optional `failures`) instead of inlining a large result set
- **Cross-backend coordinator seam** — `CoordinatorService` is a trait khive-mcp defines
  and `kkernel` implements, avoiding a dependency cycle for multi-backend link/traverse

## Usage

```rust
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveConfig, RuntimeConfig};

let runtime = khive_mcp::serve::build_single_backend_runtime(
    RuntimeConfig::default(),
    &KhiveConfig::default(),
)
.await
.expect("schema, attachment cutover, and runtime boot");
let server = KhiveMcpServer::new(runtime).expect("known packs, deps satisfied");
```

`KhiveMcpServer::new` builds the server from `runtime.config().packs`; `with_packs` takes
an explicit pack list instead. Both fail fast with `PackRegError` (naming the unknown pack
or missing dependency) rather than silently dropping packs. Once built, `serve_stdio(self)`
consumes the server and serves over stdio — the path `StdioTransport::serve` and `kkernel
mcp` both call.

Production callers must obtain that runtime from the async host builders. They inventory
secondaries, install the shared bounded hydrator, and finish the resumable V21 attachment/GC
cutover before exposing a server. Direct `KhiveRuntime::from_backend` plus
`KhiveMcpServer::new` is only for tests or a caller that has independently proved the backend is
exact-current; it does not run the application-assisted migration.

Those Phase-4b builders must not be deployed until the Phase-4a GC compatibility
release has converged across every process sharing the database/blob root and
all pre-Phase-4a processes are drained. Phase 4a makes no schema/data change; it
only refuses transactional GC unless it sees an exact completed V21 epoch.
Before cutover, every Phase-4a application-serving/read-write process must also
be quiesced or unable to access the database. A GC-only worker's completed-V21
compatibility is not general serving compatibility; start Phase-4b serving only
after exact-current topology validation.

The Phase-4b boot APIs are async source changes:
`build_server{,_with_explicit_namespace}`,
`build_registry_for_multi_backend{,_with_db_anchor}`, and
`build_server_multi_backend{,_with_db_anchor}` must now be awaited.

## Where this sits

`khive-mcp` depends on `khive-db`, `khive-runtime`, `khive-storage`, `khive-request`, and
every first-party pack crate (`khive-pack-kg`, `-gtd`, `-memory`, `-brain`, `-comm`,
`-schedule`, `-knowledge`, `-session`) so their `inventory::submit!` verb registrations link
into any binary that depends on this crate. `kkernel` is that binary: its `mcp` subcommand
parses `khive_mcp::args::Args`, builds the runtime and pack registry, and calls into
`khive_mcp::serve::run`.

Governed by [ADR-016](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md)
(the `request` tool contract) and [ADR-049](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-049-khived-daemon.md)
(the warm daemon protocol this crate's client/daemon config-fingerprint matching supports).

## License

Apache-2.0.
