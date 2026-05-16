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
# CLI (dev mode)
deno task cli entity add concept "FlashAttention"

# HTTP server (watch mode)
deno task server

# Compile standalone binaries → ../target/khive, ../target/khive-server
deno task compile:cli
deno task compile:server

# Type-check, format, lint
deno task check
deno task fmt
deno task lint

# Tests
deno task test
```

## Publishing

The package is published to npm as `khive` and JSR as `@khive/khive`. The CLI binary is also
published as a GitHub release artifact via `deno compile`.

## Local stack

Both entry points spawn `khive-mcp` as a child process via stdio. Two ways to configure:

```bash
# Default: spawn `khive-mcp` from PATH
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
- **CLI** → install via npm (`npm i -g khive`), JSR, or download a release binary.

Both deployments need `khive-mcp` (Rust binary) on `PATH`. The Rust binary is published as a GitHub
release artifact per platform (macOS/Linux/Windows).
