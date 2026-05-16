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

Everything in khive is one of three things ([ADR-004](docs/adr/ADR-004-substrate-observables.md)):

| Substrate  | What it is                             | Mutability            | Example                                              |
| ---------- | -------------------------------------- | --------------------- | ---------------------------------------------------- |
| **Entity** | A graph node with typed edges          | Mutable + soft-delete | `LoRA` (concept), `arxiv:2106.09685` (document)      |
| **Note**   | A temporal observation about the world | Mutable + soft-delete | "FlashAttention gains scale with seq len, not batch" |
| **Event**  | An audit log entry                     | Immutable             | `create(kind="entity", ...)` was called at T         |

Entities are _things_. Notes are _what you think about things_. Events are _what happened_.

---

## The MCP verb surface

11 tools in v0.1, verb-shaped ([ADR-023](docs/adr/ADR-023-verb-consolidated-mcp-surface.md)):

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
│  crates/khive-mcp    — Rust binary (stdio MCP server)        │
│  The only binary you need. Embeds khive-runtime.              │
└──────────────────────────────────────────────────────────────┘
                            ↕ in-process
┌──────────────────────────────────────────────────────────────┐
│  crates/             — Rust storage + query core              │
│  khive-types, khive-score, khive-storage, khive-db,           │
│  khive-query, khive-runtime                                    │
└──────────────────────────────────────────────────────────────┘
```

Native sqlite-vec for vector search, FTS5 trigram tokenization (CJK-safe), concurrent connection
pooling, memory-local graph traversal. One binary, one DB file, no services to run.

HTTP gateway, CLI, and visual frontend are planned for future releases.

---

## Crates

| Crate           | Purpose                                                |
| --------------- | ------------------------------------------------------ |
| `khive-types`   | Domain types: Entity, Note, Event, Id128, closed enums |
| `khive-score`   | Deterministic i64 fixed-point scoring                  |
| `khive-storage` | Trait-only capability surface (zero implementations)   |
| `khive-db`      | SQLite backend: sqlite-vec, FTS5, graph edges          |
| `khive-query`   | SPARQL / GQL → SQL compiler                            |
| `khive-runtime` | Composable service API, retrieval pipeline, graph ops  |
| `khive-mcp`     | Stdio MCP binary — the only Rust-facing user surface   |

Dependency direction: `types → score → storage → db → query → runtime → mcp`. Storage is
trait-only; backends (SQLite today, Postgres tomorrow) implement the traits without touching
consumers.

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

## Design decisions (ADRs)

khive's architecture is specified in 22 Architecture Decision Records. ADRs are the normative
contract — code implements what they specify. Schema or interface changes require an ADR first.

| ADR                                                        | Title                         | What it decides                            |
| ---------------------------------------------------------- | ----------------------------- | ------------------------------------------ |
| [001](docs/adr/ADR-001-entity-kind-taxonomy.md)            | Entity Kind Taxonomy          | 6 closed entity kinds                      |
| [002](docs/adr/ADR-002-edge-ontology.md)                   | Edge Ontology                 | 13 closed relations in 6 categories        |
| [004](docs/adr/ADR-004-substrate-observables.md)           | Substrate Observables         | Note, Entity, Event — the three primitives |
| [005](docs/adr/ADR-005-storage-capability-traits.md)       | Storage Capability Traits     | Trait-only crate, 6 capabilities           |
| [019](docs/adr/ADR-019-note-kind-taxonomy.md)              | Note Kind Taxonomy            | 5 closed note kinds                        |
| [021](docs/adr/ADR-021-edge-relation-enum.md)              | Edge Relation Enum            | Compiler-enforced relation set             |
| [023](docs/adr/ADR-023-verb-consolidated-mcp-surface.md)   | Verb-Consolidated MCP         | 11 tools, UUID-resolving verbs             |
| [024](docs/adr/ADR-024-note-search-and-cross-substrate.md) | Note Search + Cross-Substrate | Hybrid retrieval + `annotates` edges       |

Full index: [docs/adr/README.md](docs/adr/README.md).

---

## Contributing

- Feature branches + PRs. Never push directly to main.
- `make ci` must pass (fmt, clippy, test, no-default-features check, release build).
- Conventional commits: `feat(types): add NoteKind taxonomy`.
- Schema/interface changes require an ADR — propose in the PR or as an issue.
- See [CLAUDE.md](CLAUDE.md) for the developer guide, [AGENTS.md](AGENTS.md) for agent usage.

---

## Status

**v0.1.0 — published on [crates.io](https://crates.io/crates/khive-mcp).** The Rust core is
complete: 7 crates, 11 MCP tools, hybrid search with local embeddings, GQL/SPARQL queries. Ready
for use with Claude Code and any MCP-compatible agent.

## License

Apache 2.0 — see [LICENSE](LICENSE).
