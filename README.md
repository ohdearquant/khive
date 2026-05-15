# khive

**The research knowledge graph runtime.** Build domain-specific KGs that grow with your work.

[![CI](https://github.com/ohdearquant/khive/actions/workflows/ci.yml/badge.svg)](https://github.com/ohdearquant/khive/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

---

## What is khive?

khive is what your research agent uses to remember, organize, and reason about everything it learns.
Not a document store. Not a vector DB. A **typed, queryable knowledge graph** that grows as you (or
your agents) do research.

```
┌────────────────────────────────────────────────────────────┐
│  Read a paper      → extract entities & edges automatically │
│  Make a connection → link with a typed relation             │
│  Need context      → traverse the graph with GQL or SPARQL  │
│  Next session      → search your notes and the graph        │
└────────────────────────────────────────────────────────────┘
```

## Why?

Vector search stops working when you need structure. Set up Neo4j and you're 3 months into a detour.
khive is the runtime that makes the graph fall out of your work — verb-accessible, agent-native,
queryable from day one.

## Architecture

| Layer       | Stack                          | What it does                                                    |
| ----------- | ------------------------------ | --------------------------------------------------------------- |
| `frontend/` | Next.js 15, React 19, Tailwind | KG visualization, traverse UI, session frontend                 |
| `deno/`     | Deno 2, TypeScript, Hono       | HTTP server + `khive` CLI (single TS package, two entry points) |
| `mcp/`      | MCP SDK                        | Specialized MCP servers (extractors etc.) — language per tool   |
| `crates/`   | Rust                           | Storage core, query engine, MCP server binary                   |

The Rust core does what only Rust can do well: native sqlite-vec, FTS5 trigram, concurrent
connection pool, memory-local graph traversal. Everything user-facing (HTTP, CLI, agent
orchestration) is Deno + TypeScript — one codebase, edge-deployable.

Agent integration is **MCP-only**. Every caller (Python ag2, Claude Code, TS agents, custom scripts)
speaks MCP via the standard SDK. No bespoke language SDKs to maintain.

For the full design: see [ADR-003](docs/adr/ADR-003-four-layer-architecture.md) and
[ADR-011](docs/adr/ADR-011-deno-mcp-only-server.md).

## Design

khive's design is captured in 22 Architecture Decision Records (ADRs). These are the normative
contract — code implements what ADRs specify. Significant changes require an ADR first.

Key design decisions:

- **3 substrate observables**: Note, Entity, Event
  ([ADR-004](docs/adr/ADR-004-substrate-observables.md))
- **Closed taxonomies**: 6 entity kinds, 13 edge relations, 5 note kinds — compiler-enforced
- **14 verb-consolidated MCP tools** ([ADR-023](docs/adr/ADR-023-verb-consolidated-mcp-surface.md))
- **Trait-only storage** ([ADR-005](docs/adr/ADR-005-storage-capability-traits.md)) — swap backends
  without changing services

Full index: [docs/adr/README.md](docs/adr/README.md)

## Quickstart

```bash
# Full CI (same as GitHub Actions)
make ci

# Rust core
cd crates && cargo test --workspace

# Deno CLI (dev mode)
cd deno && deno task cli --help

# Deno HTTP server (watch mode)
cd deno && deno task server

# Frontend
cd frontend && pnpm install && pnpm dev
```

## Contributing

- Feature branches + PRs. Never push directly to main.
- `make ci` must pass before merge (fmt, clippy, test, no-default-features check, release build).
- Conventional commits: `feat(types): add NoteKind taxonomy`.
- Schema/interface changes require an ADR.
- See [CLAUDE.md](CLAUDE.md) for developer guide, [AGENTS.md](AGENTS.md) for agent usage guide.

## License

Apache 2.0 — see [LICENSE](LICENSE).

## Status

**Pre-alpha.** API surface and data model will change.

## Links

- Hosted: [khive.ai](https://khive.ai) (coming soon)
- For developers working on this repo: see [CLAUDE.md](CLAUDE.md)
- For agents using khive: see [AGENTS.md](AGENTS.md)
