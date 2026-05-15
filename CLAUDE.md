# khive — Claude Code Working Guide

**What this is**: The research knowledge graph runtime. Build domain-specific KGs that grow with
your work.

**License**: Apache 2.0 (public).

---

## Architecture

The Rust core owns storage + query (where native code matters). Everything user-facing is TypeScript
(where iteration speed matters). Agents reach the KG via MCP, the universal interface.

```
┌──────────────────────────────────────────────────────────────┐
│  frontend/  — Next.js 15 + React 19 + Tailwind               │
│  Visual KG explorer, traverse UI, research session frontend  │
└──────────────────────────────────────────────────────────────┘
                            ↕ HTTP
┌──────────────────────────────────────────────────────────────┐
│  deno/      — Deno 2 + TypeScript (single package)           │
│  Two entry points:                                            │
│    • src/server.ts   HTTP gateway + research orchestration   │
│    • src/cli.ts      `khive` CLI (deno compile → binary)     │
│  Both talk to the runtime via MCP stdio                       │
└──────────────────────────────────────────────────────────────┘
                            ↕ MCP stdio
┌──────────────────────────────────────────────────────────────┐
│  crates/khive-mcp    — Rust binary (ONLY user-facing Rust)   │
│  Stdio MCP server, embeds khive-runtime                       │
└──────────────────────────────────────────────────────────────┘
                            ↕ in-process
┌──────────────────────────────────────────────────────────────┐
│  crates/             — Rust storage + query core              │
│  khive-types, khive-score, khive-storage, khive-db,           │
│  khive-query, khive-runtime                                    │
└──────────────────────────────────────────────────────────────┘
```

External agents (Claude Code, Python ag2, custom TS, etc.) call into the same `khive-mcp` over
stdio. There is **no language-specific SDK** — MCP is the universal interface. See
[ADR-011](docs/adr/ADR-011-deno-mcp-only-server.md).

---

## Directory map

| Path                   | Purpose                                                                           |
| ---------------------- | --------------------------------------------------------------------------------- |
| `frontend/`            | Next.js 15 App Router. KG visualization, traverse UI, research session interface. |
| `deno/`                | Deno + TypeScript. Server + CLI in one package.                                   |
| `deno/src/server.ts`   | HTTP entry point — `deno task server`                                             |
| `deno/src/cli.ts`      | CLI entry point — `deno task cli` (compiles to `khive` binary)                    |
| `deno/src/api/`        | HTTP routes (used by server entry)                                                |
| `deno/src/commands/`   | CLI command handlers (used by cli entry)                                          |
| `deno/src/mcp/`        | MCP client wrapper — talks to `khive-mcp`                                         |
| `deno/src/research/`   | Agent orchestration (research sessions)                                           |
| `mcp/`                 | Specialized MCP servers (paper extractors etc.) — language per tool               |
| `crates/khive-types`   | Domain types (Entity, Note, Event, Id128, ...)                                    |
| `crates/khive-score`   | Deterministic scoring (i64 fixed-point)                                           |
| `crates/khive-storage` | Trait-only capability surface (SqlAccess, GraphStore, ...)                        |
| `crates/khive-db`      | SQLite backend implementing the storage traits                                    |
| `crates/khive-query`   | SPARQL/GQL parsers + SQL compiler                                                 |
| `crates/khive-runtime` | Composable Service API (used by `khive-mcp`)                                      |
| `crates/khive-mcp`     | The stdio MCP binary — the only Rust user-facing binary                           |
| `docs/adr/`            | Architecture Decision Records                                                     |

---

## Working conventions

### Deno (deno/)

- Deno 2.x. **Never** `npm install` for the Deno package — use `deno.json` `imports` map.
- `deno task cli`, `deno task server` for dev.
- `deno task fmt`, `deno task lint`, `deno task check`, `deno task test`.
- Package name on npm/JSR: `khive`.
- Shared code lives in `deno/src/mcp/`, `deno/src/research/`, `deno/src/types/` — imported by both
  entry points.

### Next.js (frontend/)

- Next 15.x, React 19, TypeScript strict, App Router.
- Tailwind + shadcn/ui.
- TanStack Query for data fetching.
- react-flow for KG visualization.

### Rust (crates/)

- Workspace at `crates/Cargo.toml`.
- `cargo check --workspace`, `cargo test --workspace`.
- Apache-2.0 license on all crates.
- Storage crates are trait-driven (ADR-005). Backend implementations live in their own crates.
- `cargo clippy --workspace -- -D warnings` must pass before merge.
- `cargo fmt --all -- --check` must pass before merge.

### MCP (mcp/)

- Specialized MCP servers — paper extractors, web scrapers, etc.
- Language per tool — Python for ML-heavy, Deno for orchestration glue.
- Each MCP server has its own config (`pyproject.toml`, `deno.json`, etc.).

---

## CI / CD

GitHub Actions runs on every push to `main` and every PR:

- **CI job** (ubuntu + macOS matrix): `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test --workspace`, `cargo check --no-default-features`, release build.
- **Docs lint job**: `deno fmt --check docs/`.

Locally, `make ci` runs the same checks. Individual targets: `make check`, `make clippy`,
`make test`, `make fmt`, `make docs-check`.

Feature branches + PRs for all changes. Never push directly to main.

Conventional commits with crate scope: `feat(types): add NoteKind taxonomy`.

---

## ADR-driven development

Architecture Decision Records (`docs/adr/`) are the design contract. Code implements what ADRs
specify. Significant design changes (new entity kinds, new edge relations, new MCP verbs, new
storage traits) require an ADR **before** code lands. See `docs/adr/README.md` for the full index.

ADR status values: `accepted` (approved design), `planned` (approved but not yet implemented),
`deprecated` (replaced by a newer ADR).

---

## Development workflow

```bash
# Full CI (same as GitHub Actions)
make ci

# Rust core
cd crates && cargo test --workspace

# Deno (CLI dev mode)
cd deno && deno task cli --help

# Deno (HTTP server, watch)
cd deno && deno task server

# Frontend
cd frontend && pnpm install && pnpm dev
```

### Local stack

Both Deno entry points spawn `khive-mcp` as a child process. The `khive-mcp` binary must be on
`PATH` or specified via `KHIVE_MCP_COMMAND`:

```bash
# Default: spawn `khive-mcp` from PATH
deno task --cwd deno cli entity list

# Custom binary location or arguments
KHIVE_MCP_COMMAND="$(pwd)/target/release/khive-mcp --db /tmp/khive-graph.db" \
  deno task --cwd deno cli entity list
```

---

## Code style

- **Default to writing no comments.** Only when the WHY is non-obvious.
- **Don't introduce abstractions for future hypothetical use.** Three similar lines beats premature
  abstraction.
- **No stubs.** If you don't know how to implement, research until you do.
- **No backwards-compat shims** unless explicitly necessary.
- **Match existing patterns in the file.** Don't reinvent style locally.

---

## Testing discipline

- **Verify before claiming complete.** Run the test, check the output. "I believe it works" is not
  "I confirmed it works."
- **Report outcomes faithfully.** If tests fail, say so. Never claim "all green" when output shows
  failures.
- **Integration > unit** for research pipelines — the value is in the composition.

---

## What lives where

| Want to do...                          | Edit this                                                                             |
| -------------------------------------- | ------------------------------------------------------------------------------------- |
| Add a new HTTP route                   | `deno/src/api/`                                                                       |
| Add a new CLI subcommand               | `deno/src/commands/`                                                                  |
| Add a new MCP tool                     | `crates/khive-mcp/src/tools/`                                                         |
| Add a research agent role              | `deno/src/research/`                                                                  |
| Change KG schema                       | `crates/khive-db/src/stores/` + add migration                                         |
| Add a new entity kind                  | **STOP** — that's an ADR change ([ADR-001](docs/adr/ADR-001-entity-kind-taxonomy.md)) |
| Add a new edge relation                | **STOP** — that's an ADR change ([ADR-002](docs/adr/ADR-002-edge-ontology.md))        |
| Add KG visualization                   | `frontend/app/`                                                                       |
| Add specialized MCP server (extractor) | `mcp/<name>/`                                                                         |

---

## Cross-references

- **lattice** (separate repo, public on crates.io): inference engine for embeddings + reranking +
  LLM serving. `lattice-embed` is consumed directly by `khive-runtime` as a Rust dependency (see
  ADR-012).
- **lionag2** (open source, separate repo): ag2 research patterns we drew inspiration from.

---

## Anti-patterns

- **Don't add a language SDK.** Agents speak MCP. That's the contract.
- **Don't write the CLI in Rust.** It's Deno (one TS codebase covers server + CLI).
- **Don't reimplement KG primitives outside `crates/`.** If you're writing entity CRUD in Deno,
  you're in the wrong place — add an MCP tool instead.
- **Don't store research findings only in memory.** They should become entities + edges in the KG.
- **Don't optimize before measuring.** The bottleneck is rarely where you think.
