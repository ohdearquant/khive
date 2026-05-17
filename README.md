# khive

A research knowledge graph runtime. Typed substrates, closed taxonomies, and a verb-consolidated
MCP surface — built for agents that need structure, not just vectors.

[![CI](https://github.com/ohdearquant/khive/actions/workflows/ci.yml/badge.svg)](https://github.com/ohdearquant/khive/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/khive-mcp.svg)](https://crates.io/crates/khive-mcp)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Vector search finds similar text. A knowledge graph finds _structure_ — lineages, dependencies,
contradictions, gaps. khive gives your research agent a typed, queryable graph that grows as it
works: read a paper and entities + edges fall out; make a connection and it's traversable
immediately; come back next session and the graph remembers what you built.

No Neo4j. No SPARQL endpoint to deploy. SQLite on disk, MCP over stdio, `cargo test` in 4 seconds.

---

## What you get

| Capability                  | How                                                                                                |
| --------------------------- | -------------------------------------------------------------------------------------------------- |
| **Typed entities**          | 6 closed kinds: concept, document, dataset, project, person, org                                   |
| **Typed edges**             | 13 closed relations in 6 categories (structure, derivation, dependency, impl, lateral, annotation) |
| **Typed notes**             | 5 closed kinds: observation, insight, question, decision, reference                                |
| **Hybrid search**           | FTS5 trigram (CJK-safe) + sqlite-vec embeddings + reciprocal rank fusion                           |
| **Graph traversal**         | BFS with depth/direction/relation filters, bidirectional shortest path                             |
| **GQL + SPARQL queries**    | Parse to SQL, run against the same SQLite backend                                                  |
| **Salience-weighted notes** | Notes carry importance scores; search ranks by semantic relevance × salience                       |
| **Cross-substrate links**   | Notes annotate entities (and vice versa) via the same edge system                                  |
| **Soft delete + supersede** | History-preserving: old records stay, newer ones supersede via graph edges                         |
| **Namespace isolation**     | Tenant scoping on every operation — share one DB, isolate many agents                              |

---

## The three substrates

Everything in khive is one of three things:

| Substrate  | What it is                             | Mutability            | Example                                              |
| ---------- | -------------------------------------- | --------------------- | ---------------------------------------------------- |
| **Entity** | A graph node with typed edges          | Mutable + soft-delete | `LoRA` (concept), `arxiv:2106.09685` (document)      |
| **Note**   | A temporal observation about the world | Mutable + soft-delete | "FlashAttention gains scale with seq len, not batch" |
| **Event**  | An audit log entry                     | Immutable             | `create(kind="entity", ...)` was called at T         |

Entities are _things_. Notes are _what you think about things_. Events are _what happened_.

---

## The MCP verb surface

11 tools in v0.1, verb-shaped:

```
CRUD:     create  get  list  update  delete  merge
Graph:    link  traverse  neighbors  query
Search:   search
```

`create`, `list`, `search` take `kind=entity|note` (or `kind=edge` for `list`).
`get`, `update`, `delete`, `merge` are UUID-only — they auto-detect the record type.

Agents reach khive via MCP stdio — Python, TypeScript, Rust, or any MCP-compatible client.
No language SDK to learn.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  khive-mcp       — Rust binary (stdio MCP server)            │
│  Thin dispatch shell — routes verbs to packs via registry.   │
└──────────────────────────────────────────────────────────────┘
                            ↕ VerbRegistry dispatch
┌──────────────────────────────────────────────────────────────┐
│  khive-pack-kg   — KG vocabulary + 11 verb handlers          │
└──────────────────────────────────────────────────────────────┘
                            ↕ in-process
┌──────────────────────────────────────────────────────────────┐
│  khive-runtime, khive-query, khive-db, khive-storage,        │
│  khive-score, khive-types                                    │
└──────────────────────────────────────────────────────────────┘
```

Native sqlite-vec for vector search, FTS5 trigram tokenization (CJK-safe), concurrent connection
pooling, memory-local graph traversal. One binary, one DB file, no services to run.

HTTP gateway, CLI, and visual frontend are planned for future releases.

---

## Crates

| Crate           | Purpose                                              |
| --------------- | ---------------------------------------------------- |
| `khive-types`   | Domain types, Pack trait, closed enums               |
| `khive-score`   | Deterministic i64 fixed-point scoring                |
| `khive-storage` | Trait-only capability surface (zero implementations) |
| `khive-db`      | SQLite backend: sqlite-vec, FTS5, graph edges        |
| `khive-query`   | SPARQL / GQL → SQL compiler                          |
| `khive-runtime` | Service API + VerbRegistry + PackRuntime trait       |
| `khive-pack-kg` | KG pack: vocabulary, verb handlers, kind validation  |
| `khive-mcp`     | Stdio MCP binary — thin dispatch over VerbRegistry   |

Dependency direction: `types → score → storage → db → query → runtime → pack-kg → mcp`.
Storage is trait-only; backends (SQLite today, Postgres tomorrow) implement the traits without
touching consumers.

---

## Quick start

### Install from crates.io

```bash
cargo install khive-mcp
```

### Or build from source

```bash
git clone https://github.com/ohdearquant/khive.git && cd khive
cd crates && cargo build --release -p khive-mcp
# Binary at: crates/target/release/khive-mcp
```

### Configure for Claude Code

Add to your project's `.mcp.json` (or `~/.claude/mcp.json` for global):

```json
{
  "mcpServers": {
    "khive": {
      "command": "khive-mcp",
      "args": []
    }
  }
}
```

That's it. Claude Code will auto-discover the 11 tools. Your agent can immediately:

```
create(kind="entity", entity_kind="concept", name="LoRA", description="Low-Rank Adaptation")
search(kind="entity", query="parameter efficient fine-tuning")
link(source_id="<lora-uuid>", target_id="<qlora-uuid>", relation="variant_of")
```

### Configuration options

```bash
khive-mcp                                    # Default: ~/.khive/khive-graph.db
khive-mcp --db /path/to/my.db               # Custom DB path
khive-mcp --db :memory:                      # Ephemeral (testing)
khive-mcp --namespace my-project             # Default namespace (default: "local")
khive-mcp --no-embed                         # Disable local embedding model
khive-mcp --log debug                        # Log level (default: warn)
```

Environment variables: `KHIVE_DB`, `KHIVE_NAMESPACE`, `KHIVE_NO_EMBED`, `KHIVE_LOG`.

### Run tests

```bash
cd crates && cargo test --workspace
make ci  # Full CI: fmt, clippy, test, build
```

### Prerequisites

- Rust 1.94+ (via [rustup](https://rustup.rs))
- Deno 2.x (for TypeScript layers — optional, not needed for MCP server)
- Node.js 20+ and pnpm (for frontend — optional)

---

## Contributing

- Feature branches + PRs. Never push directly to main.
- `make ci` must pass (fmt, clippy, test, no-default-features check, release build).
- Conventional commits: `feat(types): add NoteKind taxonomy`.
- Schema/interface changes need a design doc — propose in the PR or as an issue.
- See [CLAUDE.md](CLAUDE.md) for the developer guide, [AGENTS.md](AGENTS.md) for agent usage.

---

## Status

**v0.1.2 — published on [crates.io](https://crates.io/crates/khive-mcp).** 8 crates, 11 MCP tools,
pack-based verb dispatch, hybrid search with local embeddings, GQL/SPARQL queries. Ready for use
with Claude Code and any MCP-compatible agent.

## License

Apache 2.0 — see [LICENSE](LICENSE).
