# ADR-028: Request Parser as a Standalone Crate

**Status**: accepted\
**Date**: 2026-05-18\
**Authors**: Ocean, lambda:khive

## Context

[ADR-020](ADR-020-request-dsl.md) introduced the `request` DSL. Its parser originally lived in
`khive-mcp/src/request_dsl.rs` — a private module of the MCP server. That placement was
defensible for v0.1 when MCP was the only transport. v0.2 makes the assumption no longer hold:

1. **Multiple transports.** An HTTP gateway, a CLI batch runner, and FFI bindings are on the
   near-term roadmap. Each would re-parse the same DSL. Coupling the parser to `khive-mcp`
   forces each new transport to either depend on the MCP crate (drags in `rmcp`, `tokio`
   features, server scaffolding) or duplicate the parser.
2. **LNDL.** Lion Natural Directive Language is the planned natural-language frontend for the
   same dispatch pipeline (parse → compile → dispatch). LNDL parses _into_ the same
   `ParsedRequest` shape this DSL parses into — they share an AST, not a parser. Keeping the
   AST + parser in their own crate makes "swap the frontend" a pure-parser change with zero
   impact on runtime or transport layering.
3. **Tooling reuse.** Linters, syntax highlighters, and policy authoring tools
   ([ADR-029](ADR-029-authorization-gate.md) imagines Rego policies over the same args)
   benefit from a parser they can depend on without dragging the MCP server.

This ADR extracts the parser into its own crate to remove those couplings.

## Decision

Create `crates/khive-request` (Apache-2.0), containing:

- `parse_request(&str) -> Result<ParsedRequest, DslError>` — entry point
- `ParsedRequest` — top-level node (single op or batch)
- `ParsedOp { tool: String, args: serde_json::Map<String, Value> }` — one op
- `DslError` — structured parse errors with position info
- `MAX_OPS = 100` — batch cap (per ADR-020)

The crate has no transport dependencies. It depends only on `serde_json` (for the JSON form and
value typing) and `thiserror`. Anything that needs to turn a DSL string into ops imports
`khive-request` and nothing else.

### `khive-mcp` becomes a consumer

`khive-mcp` re-exports nothing from `khive-request`. The server imports `parse_request`,
`ParsedOp`, and `DslError` and uses them in its `request` tool handler. No back-compat shims.

### Forward-compat hook for LNDL

LNDL is out of scope for v0.2 but the crate is shaped to hold it. When LNDL lands, it ships as a
second parser function in this crate (or a sibling crate that depends on it), producing the same
`ParsedRequest` shape. Transports remain unchanged.

## Rationale

### Why a separate crate, not a submodule of `khive-runtime`

`khive-runtime` carries storage, query, and embedding deps. A new transport (HTTP gateway, CLI)
that needs _only_ the parser shouldn't drag those in. The runtime depends on the parser, not the
other way around — it just needs `ParsedRequest` to dispatch against `VerbRegistry`. A standalone
crate keeps that dependency one-directional.

### Why not in `khive-types`

`khive-types` is `no_std`-compatible and near zero-dep. The parser needs `serde_json` and an
allocator. Putting it in `khive-types` would force every consumer of the core types to compile a
parser they don't use.

### Why this matters for license boundaries

Future transports may have different license needs (an HTTP gateway might be MIT for embedding
ease; a CLI stays Apache-2.0 like the rest of OSS). A small Apache-2.0 parser crate is reusable
across all of them without forcing license decisions at the transport layer.

## Alternatives Considered

| Alternative                                  | Pros                               | Cons                                                    | Why rejected                                                    |
| -------------------------------------------- | ---------------------------------- | ------------------------------------------------------- | --------------------------------------------------------------- |
| Keep parser in `khive-mcp` as a `pub` module | Zero refactor                      | Couples every transport to the MCP server crate         | Blocks the planned HTTP gateway / CLI without code duplication  |
| Move parser to `khive-runtime`               | Co-located with dispatch           | Drags storage/embedding deps into every parser consumer | Wrong direction — runtime should consume the parser, not own it |
| Inline parser into each transport            | Maximum transport independence     | Three copies to keep in sync                            | Parser bugs propagate to N transports                           |
| Put parser in `khive-types`                  | Co-located with `Pack` + `VerbDef` | Forces `serde_json` + allocator on every types consumer | Breaks the `no_std`-compatible invariant of `khive-types`       |

## Consequences

### Positive

- HTTP gateway, CLI, and FFI bindings can depend on the parser without bringing the MCP server.
- LNDL has a target AST + crate to slot into.
- Policy-authoring tools (Rego linters, etc.) can parse args with a small dep.
- Parser tests live with the parser, not the server's integration suite.

### Negative

- One more crate in the workspace. Mitigated: it's tiny (~560 LOC including 17 tests),
  single-file, no transitive Rust deps beyond `serde_json` and `thiserror`.

### Neutral

- ADR-020 (the DSL spec) gains a sibling implementation crate. The spec lives in the ADR; the
  crate is one possible implementation. A future ADR that changes the DSL grammar updates this
  crate.

## Implementation Status

| Step                                                                                | Where                                             | Status |
| ----------------------------------------------------------------------------------- | ------------------------------------------------- | ------ |
| New crate `khive-request`                                                           | `crates/khive-request/`                           | done   |
| Public surface: `parse_request`, `ParsedRequest`, `ParsedOp`, `DslError`, `MAX_OPS` | `crates/khive-request/src/lib.rs`                 | done   |
| Parser tests moved with parser                                                      | `crates/khive-request/src/lib.rs` (17 unit tests) | done   |
| `khive-mcp/src/server.rs` consumes it                                               | `crates/khive-mcp/Cargo.toml` workspace dep       | done   |
| ADR-020 amended with supersession note for the parser location                      | `docs/adr/ADR-020-request-dsl.md`                 | done   |

## References

- [ADR-020](ADR-020-request-dsl.md): The DSL spec (this crate implements it)
- [ADR-027](ADR-027-single-tool-mcp-surface.md): The MCP surface that consumes the parser
- LNDL (Lion Natural Directive Language): planned natural-language frontend; not yet in any
  public repo. Forward-compat target for `ParsedRequest`.
