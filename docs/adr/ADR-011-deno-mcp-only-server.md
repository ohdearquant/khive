# ADR-011: Deno User Surfaces + Rust Runtime + MCP-Only Integration

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

The four-layer architecture (ADR-003) defines the layer boundaries but not the specific language for
the server/CLI layer. This ADR fills that in.

Two questions need a decision:

1. **What language hosts the HTTP gateway, the CLI, and the research-orchestration glue?**
2. **What programmatic interface do other-language agents (Python ag2, custom TS scripts, Claude
   Code) use to reach khive?**

For (1): the candidate languages are Python (with FastAPI + an agent framework like ag2) or a
TypeScript runtime (Deno / Node / Bun). The actual responsibilities of the layer are HTTP routing,
MCP client calls into the Rust runtime, and orchestration of LLM agent loops. None of these are
inherently Python-bound — they're all glue code. The ML ecosystem advantage of Python doesn't apply
because all ML lives in `lattice` (Rust).

For (2): shipping a per-language SDK (a Python `khive` package, a TS `khive` package, etc.) creates
ongoing drift — every Rust runtime change has to be re-exposed in every SDK. Every breaking change
needs a coordinated release. The wrappers slowly diverge from the Rust API.

## Decision

**Two changes:**

1. **The server is Deno + TypeScript**, not Python + FastAPI.
2. **Agent integration is MCP-only.** No Python SDK. No language-specific bindings. Every caller
   speaks MCP.

### Stack

```
crates/             Rust storage + runtime
├── khive-types, khive-score, khive-storage, khive-db, khive-query
├── khive-runtime   (Rust library — storage + query glue)
└── khive-mcp       (ONLY Rust binary — stdio MCP server)

deno/               Single Deno + TypeScript package
├── deno.json       (tasks, deps, fmt config)
├── src/server.ts   HTTP gateway + research orchestration entry
├── src/cli.ts      `khive` command-line entry (deno compile → binary)
├── src/api/        HTTP route handlers
├── src/commands/   CLI subcommand handlers
├── src/mcp/        MCP client wrapper — talks to `khive-mcp`
├── src/research/   Agent orchestration (research sessions)
└── src/types/      Shared TypeScript types

mcp/                Specialized MCP servers (extractors etc.) — language per tool
frontend/           Next.js — already TypeScript
```

### How callers reach the KG

| Caller                                  | Protocol                 | Library                                          |
| --------------------------------------- | ------------------------ | ------------------------------------------------ |
| Frontend (Next.js)                      | HTTP                     | `fetch` to Deno server                           |
| AI agent (Claude Code, ag2, custom)     | MCP stdio                | Standard MCP client SDK                          |
| CLI user                                | MCP stdio (via Deno CLI) | `khive` Deno-compiled binary, spawns `khive-mcp` |
| Deno server (research sessions)         | MCP stdio                | `@modelcontextprotocol/sdk`                      |
| Python user wanting programmatic access | MCP stdio                | `mcp` Python package                             |

There is no separate Python or TypeScript SDK to maintain. MCP is the universal interface.

### Why only one Rust binary

The Rust crates exist for **storage and query performance** — native sqlite-vec, native FTS5
trigram, parking_lot connection pool, memory-local graph traversal. Throwing those away means giving
up the KG performance ceiling.

But everything _above_ the runtime — HTTP routing, CLI argument parsing, agent orchestration — is
glue code. Glue belongs in Deno where iteration speed wins and code is shared between server and
CLI.

So `khive-mcp` is the only user-facing Rust binary. It surfaces the runtime over stdio MCP. Deno
consumes it from both the server and the CLI.

## Rationale

### Why Deno over Python for the server

| Concern                        | Deno                                                       | Python                                       |
| ------------------------------ | ---------------------------------------------------------- | -------------------------------------------- |
| **Language unification**       | TS for frontend + server                                   | TS frontend, Python server (two stacks)      |
| **Deployment**                 | Single Deno binary, edge-deployable (Workers, Deno Deploy) | Python venv + uvicorn process                |
| **Cold start**                 | ~50ms                                                      | ~500ms (FastAPI + ASGI)                      |
| **Type sharing with frontend** | Zod schemas / shared types possible                        | None (need codegen or duplicate definitions) |
| **ML ecosystem**               | Weaker (but irrelevant — ML is in lattice)                 | Stronger (but unused here)                   |
| **Agent frameworks**           | LangChain.js exists; raw SDKs are clean                    | ag2 mature, langchain mature                 |

The agent-framework question is the only real tradeoff. ag2 has good orchestration patterns. But:

- ag2's value is patterns, not API; TS can copy.
- Raw Anthropic/OpenAI SDKs are excellent in TypeScript.
- For khive's research orchestration (recursive paper exploration, agent teams), the orchestration
  logic is ~500 LOC. Reimplementing in TS is a day of work, not a barrier.

The deployment story tips the scale. "khive can run on the edge" is a real selling point for the
GitHub-for-KGs positioning — users could host their own KG instance on Cloudflare Workers' free
tier. Python can't do that.

### Why MCP-only

Maintaining a Python SDK creates drift:

- Every new feature in the Rust runtime needs to be re-exposed in Python.
- Every breaking change in the Rust runtime needs a coordinated Python release.
- The Python wrappers become subtly different from the Rust API over time.

MCP is the universal abstraction. By making it the _only_ programmatic interface:

- Adding a feature = updating the Rust MCP tool definitions. Every caller sees the new feature
  instantly.
- Bugs are fixed in one place.
- The interface is well-specified (MCP protocol) rather than ad-hoc.

The trade-off: MCP has slight overhead per call (JSON-RPC over stdio). For research-KG operations
(entity CRUD, traverse, query), this overhead is negligible (~100µs) compared to the work being
done.

### Why the CLI is also Deno (not a second Rust binary)

A Rust CLI that embeds `khive-runtime` directly is the obvious alternative. We didn't pick it
because:

- The CLI is one-shot command-line work. Startup cost is dominated by MCP spawn overhead (~20ms),
  not Deno cold start (~50ms). Total ~70ms is acceptable for `khive entity add`.
- A Deno CLI **shares code with the Deno server** — the same MCP client wrapper, the same TS types,
  the same orchestration helpers. One TS codebase covers both surfaces.
- Binary size (Deno compile ~80MB vs Rust ~10MB) is irrelevant for an installed tool.
- One less Rust binary to maintain.

So there is **exactly one** Rust binary: `khive-mcp`. Everything user-facing (HTTP server, CLI) is
Deno.

The Rust crates that remain:

- `khive-runtime` (library — embedded by `khive-mcp` only)
- `khive-mcp` (binary — stdio MCP server)
- (storage layer: types, score, storage, db, query — libraries)

## Alternatives Considered

| Alternative                       | Pros                          | Cons                                                                                                                       | Why rejected                                                              |
| --------------------------------- | ----------------------------- | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| Python + ag2 + FastAPI            | ag2 patterns, ML ecosystem    | Two-language stack (TS frontend + Python server), slower cold start, would need a Python SDK that drifts from the Rust API | Net negative for this scope                                               |
| Bun instead of Deno               | Faster cold start, npm-native | Less mature, smaller stdlib                                                                                                | Deno is more conservative; either works, picked Deno for built-in tooling |
| Node.js                           | Largest ecosystem             | Multiple package managers, no built-in TS, complex tooling                                                                 | Deno wins on DX                                                           |
| Keep Python SDK + add Deno server | Both interfaces               | More to maintain                                                                                                           | No upside                                                                 |
| All-Rust server (axum)            | Single language with crates   | TypeScript devs less productive in Rust; iteration speed for research code matters                                         | Server velocity > Rust performance for orchestration                      |

## Consequences

### Positive

- Single TypeScript stack for frontend + server + CLI.
- Edge-deployable (Cloudflare Workers, Deno Deploy).
- Faster iteration on research orchestration (TypeScript over Rust for glue).
- No SDK drift — MCP is the only programmatic interface.
- Exactly one Rust binary to ship (`khive-mcp`).

### Negative

- ag2 patterns need to be reimplemented in TS. Mitigated: ~500 LOC reimplementation, agentic
  patterns are well-understood.
- Python users wanting programmatic access need to use an MCP client (slight learning curve over a
  Python SDK). Mitigated: `mcp` Python package is well-maintained and easy to use.
- Researchers who prefer Python need to either: use MCP from Python, or use the HTTP API of the Deno
  server. Both work fine.

### Neutral

- Specialized MCP servers (paper extractor etc.) can still be Python where ML/extraction tooling is
  mature. They live in `mcp/` and the language is per-tool.
- The Rust core crates are unaffected.

## Open Questions

1. **HTTP framework**: Hono is the leading choice — small, fast, runs everywhere. Picked initially;
   can swap if it doesn't fit.
2. **Streaming protocol for research sessions**: SSE (Server-Sent Events) is the obvious choice for
   frontend, matching how the Deno server will push session events. WebSockets if bidirectional
   becomes necessary.
3. **MCP transport for the server**: stdio (spawn child process) for v0.1. Could move to HTTP MCP
   transport if it standardizes.

## Implementation

### `deno/` package layout

```
deno/
├── deno.json           # Deno config: tasks, deps, fmt config
├── README.md
└── src/
    ├── server.ts       # HTTP gateway entry — Hono app, port 8000
    ├── cli.ts          # CLI entry — `khive` command-line tool
    ├── api/            # HTTP route handlers (used by server.ts)
    ├── commands/       # CLI subcommand handlers (used by cli.ts)
    ├── mcp/            # MCP client wrapper — spawns `khive-mcp`
    ├── research/       # Research session orchestration
    │                   # (v0.1: single-shot mode; v0.2: multi-step loop)
    └── types/          # Shared TypeScript types
```

Both entry points reuse the same `mcp/`, `research/`, and `types/` modules — one TS codebase, two
binaries when compiled.

### Crate plan

Rust:

- `khive-runtime` — library (embedded by `khive-mcp`).
- `khive-mcp` — the only Rust binary (stdio MCP server, embeds runtime).
- Storage crates (`khive-types`, `khive-score`, `khive-storage`, `khive-db`, `khive-query`).

Deno package:

- `deno/` — single TypeScript package with two entry points:
  - `src/server.ts` — HTTP gateway + research orchestration (run via `deno task server`)
  - `src/cli.ts` — `khive` command-line tool (compiled via `deno compile`)

### Deployment

Local development:

```bash
# Start the server (it spawns khive-mcp as a child process)
deno task --cwd deno server
# CLI dev
deno task --cwd deno cli --help
# Frontend
pnpm --filter frontend dev
```

Production:

```bash
deno compile --allow-net --allow-read --allow-env --allow-run \
  --output khive-server deno/src/server.ts
deno compile --allow-net --allow-read --allow-env --allow-run \
  --output khive deno/src/cli.ts
# Deploy the binaries + khive-mcp to your host of choice
```

## References

- ADR-003: Four-Layer Architecture (this ADR fills in the server/CLI language)
- ADR-005: Storage Capability Traits (unaffected — still used by `khive-runtime`)
- ADR-010: KG Versioning Direction (planned; the edge-deployable property here matches the "GitHub
  for KGs" vision)
- MCP spec: https://modelcontextprotocol.io
- Hono: https://hono.dev
- Deno: https://deno.com
