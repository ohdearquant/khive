# ADR-027: Single Tool MCP Surface — One `request`, Many Verbs

**Status**: accepted\
**Date**: 2026-05-18\
**Authors**: Ocean, lambda:khive

## Context

[ADR-023](ADR-023-verb-consolidated-mcp-surface.md) enumerated 11 verb tools as the v0.1 MCP
surface (`create`, `get`, `list`, `update`, `delete`, `merge`, `search`, `link`, `neighbors`,
`traverse`, `query`). Each was a separate `#[tool]` handler in `khive-mcp`. With ADR-025 packs,
any third-party pack would add more — and the next first-party pack (GTD, planned in a follow-up
ADR) would push the surface past 16 tools.

Two costs dominate at that scale:

1. **Tool-list latency.** MCP clients fetch the catalog at session start. Each tool definition is
   100–500 tokens of JSON schema. Sixteen tools is several KB of context the agent pays for
   before producing a single useful token.
2. **Composition friction.** Independent tool calls block on the client between dispatches.
   Agents that want to "do these three things together" need either a batch tool per verb or a
   generic batcher.

[ADR-020](ADR-020-request-dsl.md) introduced the `request` DSL — a batch tool that internally
dispatches function-call ops. ADR-020 _added_ `request`; it did not commit to _removing_ the flat
tools. This ADR makes the consolidation explicit.

## Decision

The khive-mcp server exposes **exactly one MCP tool**: `request`. All pack verbs (kg's `create`,
`get`, …, and verbs from any additional packs registered into the `VerbRegistry`) are reached
through `request(ops="…")` per ADR-020. No flat verb tools are advertised.

### Tool discovery

A client calling `list_tools` sees one tool. Its description carries the verb catalog:

```
Run one or more khive verbs in a single MCP call.

ops syntax:
  Single op   : verb(name=value, name=value)
  Batch       : [verb(...), verb(...)]                — parallel, max 100
  JSON form   : [{"tool":"verb","args":{...}}, ...]   — equivalent

Verbs registered on this server:
  create — Create an entity or note ...
  get    — Fetch any record ...
  ...
```

The catalog is **dynamic** — `KhiveMcpServer::list_tools` walks the runtime's
`VerbRegistry::all_verbs()` and appends the lines to the `request` tool's description. The same
catalog is also surfaced in the server's `instructions` for clients that read it there. Any pack
registered into the `VerbRegistry` appears in the catalog with no code change in the server.

### Plugin scoping (forward-looking)

The single-tool surface is the substrate for plugin scoping. The mechanism — `KHIVE_PACKS` env var
selecting which packs to register into the registry — lands together with the second built-in
pack in a follow-up PR. v0.2 of this ADR ships the surface; v0.2 of the runtime currently
registers `kg` only, and the catalog reflects exactly what's registered. Multi-pack composition
(`KHIVE_PACKS=kg,gtd`) is the same machinery once a second pack ships.

## Rationale

### Why one tool

- **Context economy.** One `request` description ≈ 600 tokens of stable text. Sixteen flat tools
  ≈ several KB of redundant schema. For long sessions the saved context compounds.
- **No "is it batchable?" question.** Any verb is batched the moment it's registered. No per-tool
  batch variant to author or document.
- **Composition.** Agents can mix verbs in one call (create + link + search) — impossible across
  discrete tool invocations without N round trips.
- **Forward-compatible.** New packs add to the catalog without touching the server or growing the
  tool list.

### Why not "request plus the popular flat tools" as a transition

Two-mode surfaces fragment the agent's mental model. The DSL is dense enough
(`create(kind="entity", name="LoRA")` ≈ a flat call) that there's no usability reason to keep
flat tools alive. Tool-list parsimony is a one-shot benefit worth taking immediately.

### Why a dynamic catalog (vs. hard-coded description)

The pack system (ADR-025) is the substrate for third-party verbs. A hard-coded catalog would
lock the description to the kg built-in; dynamic generation from `VerbRegistry::all_verbs()`
makes the server agnostic about which packs are loaded.

## Alternatives Considered

| Alternative                                  | Pros                             | Cons                                                 | Why rejected                                   |
| -------------------------------------------- | -------------------------------- | ---------------------------------------------------- | ---------------------------------------------- |
| Keep flat tools alongside `request`          | Familiar for non-batching agents | Doubles the surface; ambiguity over which to use     | Surface bloat with no usability win            |
| One tool per pack (e.g. `kg`, `gtd`)         | Pack-scoped discovery            | Per-pack tools duplicate the DSL description         | `request` scopes via the loaded pack set       |
| `request` for batch, flat tools for solo ops | "Right tool for the job"         | Two mental models; flat tools still bloat the list   | `request` handles solo ops natively            |
| Hard-coded verb catalog in `request` desc    | Simpler server                   | Description rots when packs change; blocks 3rd-party | Dynamic generation is one read of the registry |

## Consequences

### Positive

- Tool list size is constant (1) regardless of how many packs are loaded.
- Agents learn one calling convention; pack vocabulary is data, not API surface.
- Plugin authors don't author tools — they author packs, and the server surfaces them.
- The single dispatch site simplifies later cross-cutting work (auth gate per ADR-029, audit
  obligations, tracing).

### Negative

- Clients that pattern-match tool names (some IDEs, some test harnesses) see only `request`.
  They can't grep for `create` / `assign` / etc. in the tool list. The verb catalog in the
  description is machine-readable enough that this is recoverable.
- The DSL is a slight learning curve over MCP-native flat calls. Mitigated by the verb catalog
  and per-pack `SKILL.md` plugin docs.

### Neutral

- ADR-023's verb taxonomy stays authoritative (the verbs themselves and their semantics). What
  changed is exposure — verbs are reached through `request`, not as discrete tools.
- The DSL itself (syntax, parser, error reporting) is the concern of ADR-020 + ADR-028 (the
  parser-crate ADR). This ADR is about MCP surface policy, not parser implementation.

## Implementation Status

| Step                                                                     | Where                                                     | Status                        |
| ------------------------------------------------------------------------ | --------------------------------------------------------- | ----------------------------- |
| Single `#[tool] request` handler                                         | `crates/khive-mcp/src/server.rs`                          | done                          |
| Dynamic verb catalog from `VerbRegistry`                                 | `KhiveMcpServer::verb_catalog()`                          | done                          |
| Catalog reaches `tools/list` description (not only `instructions`)       | `KhiveMcpServer::list_tools` override                     | done                          |
| Removed per-verb tool param files                                        | `crates/khive-mcp/src/tools/` (only `request.rs` remains) | done                          |
| Built-in pack registration                                               | `crates/khive-mcp/src/server.rs`                          | kg only in v0.2 of this ADR   |
| `KHIVE_PACKS` env / `--pack` CLI + multi-pack registration               | `crates/khive-mcp/src/server.rs`, `main.rs`               | planned (lands with gtd pack) |
| Plugin authoring via `KHIVE_PACKS` env                                   | `marketplace/<pack>/plugin.json`                          | planned (lands with gtd pack) |
| Integration tests: `list_tools` returns `[request]` with dynamic catalog | `crates/khive-mcp/tests/integration.rs`                   | done                          |

## References

- [ADR-020](ADR-020-request-dsl.md): Request DSL (the syntax + parser this tool wraps)
- [ADR-023](ADR-023-verb-consolidated-mcp-surface.md): Verb taxonomy (semantics still authoritative)
- [ADR-025](ADR-025-pack-standard.md): Pack standard (vocabulary mechanism)
- [ADR-028](ADR-028-request-parser-crate.md): Why the parser lives in its own crate
