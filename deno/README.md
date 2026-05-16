# khive (Deno package)

Deno + TypeScript user surfaces for khive. Single package, two entry points:

- **`src/cli.ts`** — `khive` command-line tool (compiles to a standalone binary).
- **`src/server.ts`** — HTTP gateway + research orchestration (long-running process).

Both consume the same Rust runtime via stdio MCP (`khive-mcp` binary). Why this architecture? See
[ADR-011](../docs/adr/ADR-011-deno-mcp-only-server.md).

## Stack

- **Deno 2.x** — TypeScript runtime, edge-deployable, compiles to standalone binaries.
- **Hono** — HTTP framework (small, fast, runs on Deno/Bun/Node/Workers).
- **@modelcontextprotocol/sdk** — talks to `khive-mcp` (Rust) for KG operations.
- **LLM SDKs** — `@anthropic-ai/sdk` and `openai` for research-agent orchestration.

## Layout

```
deno/
├── deno.json          # config + tasks + deps (one for both entry points)
├── README.md          # this file
└── src/
    ├── cli.ts         # CLI entry point  → `deno task cli` → compiles to `khive`
    ├── server.ts      # Server entry point → `deno task server` → HTTP on :8000
    ├── api/           # HTTP routes (used by server.ts)
    ├── commands/      # CLI command implementations (used by cli.ts)
    ├── mcp/           # MCP client wrapper (shared)
    ├── research/      # Agent orchestration (shared — used by both)
    └── types/         # Shared TypeScript types
```

## Development

```bash
# CLI (dev mode) — requires khive-mcp binary on PATH (or set KHIVE_MCP_COMMAND)
deno task cli entity create --kind concept --name "FlashAttention"

# HTTP server (watch mode)
deno task server

# Compile standalone binaries → ../target/khive, ../target/khive-server
deno task compile:cli
deno task compile:server

# Type-check, format, lint
deno task check
deno task fmt
deno task lint

# Tests (unit + integration — integration requires khive-mcp binary)
deno task test
```

## Publishing

The package is not yet published to npm or JSR. Use `deno task` for local development. Standalone
binaries are not yet published as GitHub release artifacts.

## Local stack

Both entry points spawn `khive-mcp` as a child process via stdio. Two ways to configure:

```bash
# Default: spawn `khive-mcp` from PATH (build first: cd crates && cargo build --release -p khive-mcp)
deno task server

# Custom: explicit command (e.g., specific binary, DB path, log level)
KHIVE_MCP_COMMAND="khive-mcp --db ~/.khive/khive-graph.db --log debug" deno task server
```

## Environment

| Variable            | Default     | Purpose                                     |
| ------------------- | ----------- | ------------------------------------------- |
| `PORT`              | `8000`      | HTTP server port (server.ts only)           |
| `KHIVE_MCP_COMMAND` | `khive-mcp` | How to spawn the Rust MCP server            |
| `KHIVE_NAMESPACE`   | `local`     | Default namespace for CLI/server operations |
| `ANTHROPIC_API_KEY` | (none)      | Claude API for research-agent orchestration |
| `OPENAI_API_KEY`    | (none)      | OpenAI API for research-agent orchestration |

## Deployment

- **Server** → Deno Deploy / Cloudflare Workers / any Docker host. Single binary via
  `deno task compile:server`.
- **CLI** → compile locally via `deno task compile:cli`, then place the resulting binary on PATH.

Both deployments need `khive-mcp` (Rust binary) built and on `PATH`.
