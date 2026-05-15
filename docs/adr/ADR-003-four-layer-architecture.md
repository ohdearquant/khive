# ADR-003: Four-Layer Architecture (Frontend / Deno / MCP / Crates)

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

khive is a research knowledge graph runtime. It has multiple consumers — humans browsing the graph,
AI agents running research pipelines, MCP clients calling tools. Each consumer needs a different
interface but all share the same underlying KG state.

We need an architecture that:

1. Keeps the agent-callable surface clean (MCP-first design).
2. Allows humans to browse the graph visually.
3. Hosts research orchestration close to where it iterates fastest.
4. Keeps the storage core in a language where native code matters.

## Decision

**Four layers in this repo, each with a clear language and contract:**

```
┌──────────────────────────────────────────────────────────────┐
│  frontend/  — Next.js 15 + React 19 + Tailwind               │
│  Visual KG explorer, traverse UI, research session frontend  │
└──────────────────────────────────────────────────────────────┘
                            ↕ HTTP
┌──────────────────────────────────────────────────────────────┐
│  deno/      — Deno 2 + TypeScript                            │
│  HTTP gateway + `khive` CLI in one package                   │
│  src/server.ts (HTTP), src/cli.ts (compiles to a binary)     │
└──────────────────────────────────────────────────────────────┘
                            ↕ MCP stdio
┌──────────────────────────────────────────────────────────────┐
│  crates/khive-mcp  — Rust stdio MCP server                   │
│  The only user-facing Rust binary; embeds khive-runtime      │
└──────────────────────────────────────────────────────────────┘
                            ↕ in-process
┌──────────────────────────────────────────────────────────────┐
│  crates/    — Rust storage + query core                      │
│  khive-types, khive-score, khive-storage, khive-db,          │
│  khive-query, khive-runtime                                   │
└──────────────────────────────────────────────────────────────┘
```

External agents (Claude Code, Python ag2, custom TS, etc.) call `khive-mcp` directly via the
standard MCP SDK. There is **no language-specific khive SDK** — MCP is the universal contract.
ADR-011 documents the rationale for the Deno + MCP-only choice.

## Rationale

### Why four layers?

Each layer has a distinct responsibility and a clear interface contract:

- **Frontend**: visual experience, no business logic. Calls the Deno HTTP gateway.
- **Deno**: HTTP gateway + CLI in one TypeScript package. Spawns `khive-mcp` as a child process and
  forwards MCP calls. Edge-deployable; compiles the CLI to a standalone binary via `deno compile`.
- **MCP (Rust binary)**: protocol surface for agent-callable tools. The one Rust binary that ships
  to users. Wraps `khive-runtime` so every MCP tool is a thin handler over a typed Rust operation.
- **Crates**: shared Rust crates implementing storage, scoring, query compilation, and runtime
  composition. No user-facing binaries here other than `khive-mcp`.

### Why MCP-first?

The primary user of khive is an AI agent doing research. MCP is the native interface for agent tool
calls. By making MCP the primary surface, the frontend and CLI become consumers of the same tools
agents use — eliminating "this works for agents but not for the UI" drift.

### Why Deno (not Python or Node)?

- One TypeScript codebase covers server + CLI without needing two project setups.
- `deno compile` produces standalone CLI binaries with zero runtime install.
- Edge-deployable to Deno Deploy, Cloudflare Workers, or any Docker host.
- No language SDK to maintain — the JS/TS ecosystem speaks MCP natively via
  `@modelcontextprotocol/sdk`.

### Why Rust for the storage core?

- Native sqlite-vec, FTS5 trigram, concurrent connection pool, memory-local graph traversal.
- Deterministic scoring via fixed-point integers (ADR-006).
- Pure-Rust embedding inference via `lattice-embed` (no Python, no GPU required for default model).
- One Rust binary (`khive-mcp`) ships to users; everything else in TypeScript.

## Alternatives Considered

| Alternative                                         | Pros                       | Cons                                                                                  | Why rejected                                                           |
| --------------------------------------------------- | -------------------------- | ------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| Monolith (single Rust binary, CLI + server in Rust) | Simple deployment          | UI/server iteration is painful in Rust; no edge deployment story                      | Wrong tool for the application layer                                   |
| Python server                                       | Researcher familiarity     | Maintaining a second language for one layer; redundant with MCP for agent integration | Deno covers both server + CLI in one TypeScript codebase (see ADR-011) |
| Single TypeScript stack (Node)                      | One language top to bottom | No native sqlite-vec + FTS5 quality; can't deterministic-score                        | Wrong tool for the storage layer                                       |
| GraphQL instead of MCP                              | Standard, widely tooled    | Not native to LLM tool calling; would need adapter layer                              | MCP is the right abstraction for agent-first design                    |

## Consequences

### Positive

- Each layer can be developed and deployed independently.
- Agents and humans see the same tools (no drift between APIs).
- One Rust binary on the user's machine + one TypeScript runtime — clean install footprint.
- Edge-deployable HTTP gateway via Deno Deploy / Cloudflare Workers / any Docker host.

### Negative

- Two language ecosystems to track (Rust + TypeScript). Mitigated: each layer has a clear language,
  no cross-language code sharing within a layer.
- Local development requires the `khive-mcp` binary on `PATH` for Deno to spawn. Mitigated:
  `make local` builds + installs.

### Neutral

- Production deployment options vary by layer (Deno Deploy for the gateway, static export for the
  frontend, native binary for `khive-mcp`).

## Implementation

| Directory                                       | Purpose                                                       | Language          |
| ----------------------------------------------- | ------------------------------------------------------------- | ----------------- |
| `frontend/`                                     | Next.js 15 App Router                                         | TypeScript        |
| `deno/`                                         | HTTP gateway + CLI (two entry points, one package)            | TypeScript        |
| `mcp/`                                          | Specialized MCP servers (paper extractors etc., one per tool) | Language per tool |
| `crates/khive-mcp`                              | The stdio MCP binary                                          | Rust              |
| `crates/{types,score,storage,db,query,runtime}` | Storage + query core                                          | Rust              |
| `docs/`                                         | User-facing docs, ADRs                                        | Markdown          |

### Anti-patterns this prevents

- **Building entity CRUD in Deno** — STOP, call `khive-mcp` via the MCP client.
- **Writing the CLI in Rust** — STOP, it's Deno (`deno compile` to a binary).
- **Adding a language SDK** — STOP, MCP is the universal contract.

## References

- ADR-001: Entity Kind Taxonomy (used by all layers)
- ADR-002: Edge Ontology (used by all layers)
- ADR-005: Storage Capability Traits (the trait surface inside `crates/khive-storage`)
- ADR-011: Deno + MCP-Only (server-layer rationale)
- README.md (architecture overview)
- CLAUDE.md (development guide)
