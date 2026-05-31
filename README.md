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

| Capability                  | How                                                                                                                      |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| **63 verbs, 7 packs**       | KG, GTD, memory, brain, comm, schedule, knowledge — all load by default                                                  |
| **Typed entities**          | 9 closed kinds: concept, document, dataset, project, person, org, artifact, service, resource                            |
| **Typed edges**             | 15 closed relations in 8 categories (structure, derivation, provenance, temporal, dependency, impl, lateral, annotation) |
| **Typed notes**             | 5 closed kinds: observation, insight, question, decision, reference                                                      |
| **Hybrid retrieval**        | FTS5 + vector RRF with embedding rerank; shipped BM25, HNSW, Vamana, and fusion crates for pack-specific retrieval paths |
| **Graph traversal**         | BFS with depth/direction/relation filters, bidirectional shortest path                                                   |
| **GQL + SPARQL queries**    | Parse to SQL, run against the same SQLite backend                                                                        |
| **Salience-weighted notes** | Notes carry salience scores; search ranks by semantic relevance × salience                                               |
| **Cross-substrate links**   | Notes annotate entities (and vice versa) via the same edge system                                                        |
| **Soft delete + supersede** | History-preserving: old records stay, newer ones supersede via graph edges                                               |
| **Namespace isolation**     | Tenant scoping on every operation — share one DB, isolate many agents                                                    |

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

One MCP tool: `request` (ADR-020 + ADR-027). Every verb is a parsed op inside it.

```
request(ops="verb(arg=value, arg=value)")              # single op
request(ops="[v1(...), v2(...), v3(...)]")             # parallel batch (max 100)
request(ops="[{\"tool\":\"v1\",\"args\":{...}}, ...]") # equivalent JSON form
```

All 7 packs load by default — **63 verbs** out of the box:

| Pack          | Prefix       | Verbs | What it does                                     |
| ------------- | ------------ | ----- | ------------------------------------------------ |
| **kg**        | _(bare)_     | 11    | Entities, edges, notes, graph queries            |
| **gtd**       | `gtd.`       | 5     | Task lifecycle (inbox → next → active → done)    |
| **memory**    | `memory.`    | 2     | Salience-weighted remember / decay-ranked recall |
| **brain**     | `brain.`     | 13    | Bayesian user profiles + feedback loop           |
| **comm**      | `comm.`      | 5     | Threaded messaging                               |
| **schedule**  | `schedule.`  | 4     | Reminders and scheduled verb execution           |
| **knowledge** | `knowledge.` | 14    | Atom-based KB with embedding rerank search       |

`create`, `list`, `search` take `kind=entity|note` (or `kind=edge` for `list`).
`get`, `update`, `delete`, `merge` are UUID-only — they auto-detect the record type.

Agents reach khive via MCP stdio — Python, TypeScript, Rust, or any MCP-compatible client.
No language SDK to learn.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  khive-mcp       — Rust binary (stdio MCP server)            │
│  khived           — persistent daemon (ADR-049): warm runtime │
│                     auto-spawned on first request             │
│  1 tool: `request` (ADR-020 + ADR-027) — parses DSL,         │
│  dispatches each op through the VerbRegistry                 │
└──────────────────────────────────────────────────────────────┘
                            ↕ VerbRegistry dispatch
┌──────────────────────────────────────────────────────────────┐
│  khive-pack-kg         — KG vocabulary + 11 verb handlers    │
│  khive-pack-gtd        — task lifecycle (5 verbs)            │
│  khive-pack-memory     — salience + decay recall (2 verbs)   │
│  khive-pack-brain      — Bayesian profiles (13 verbs)        │
│  khive-pack-comm       — threaded messaging (5 verbs)        │
│  khive-pack-schedule   — reminders + scheduled ops (4 verbs) │
│  khive-pack-knowledge  — atom KB + embedding rerank (14 verbs)│
└──────────────────────────────────────────────────────────────┘
                            ↕ in-process
┌──────────────────────────────────────────────────────────────┐
│  khive-runtime, khive-request, khive-query, khive-db,        │
│  khive-storage, khive-score, khive-types                     │
└──────────────────────────────────────────────────────────────┘
```

Embedded SQLite storage with FTS5 trigram text search and sqlite-vec vector search.
Retrieval ships in-process BM25, HNSW, Vamana, and fusion crates for hybrid and pack-specific
search paths. One binary, one DB file, no external services to run.

The **khived daemon** (ADR-049) keeps the runtime warm between MCP sessions — embedding model
stays loaded, SQLite connections stay open, pack registries stay initialized. It auto-spawns on
first request and persists in the background, eliminating cold-start overhead on reconnect.

HTTP gateway, CLI, and visual frontend are planned for future releases.

---

## Crates

| Crate                  | Purpose                                                                                                |
| ---------------------- | ------------------------------------------------------------------------------------------------------ |
| `khive-types`          | Domain types, Pack trait, closed enums                                                                 |
| `khive-score`          | Deterministic i64 fixed-point scoring                                                                  |
| `khive-storage`        | Trait-only capability surface (zero implementations)                                                   |
| `khive-db`             | SQLite backend: entity/note/edge tables, FTS5 TextSearch, current sqlite-vec VectorStore compatibility |
| `khive-retrieval`      | Hybrid retrieval primitives                                                                            |
| `khive-fusion`         | RRF, weighted, union, vector-only, and keyword-only fusion strategies                                  |
| `khive-bm25`           | BM25 keyword index                                                                                     |
| `khive-hnsw`           | HNSW vector index                                                                                      |
| `khive-vamana`         | Vamana ANN index used by knowledge search                                                              |
| `khive-query`          | SPARQL / GQL → SQL compiler                                                                            |
| `khive-runtime`        | Service API + VerbRegistry + PackRuntime trait                                                         |
| `khive-request`        | Request DSL parser (function-call, JSON; pipe / LNDL planned). Transport-agnostic AST.                 |
| `khive-pack-kg`        | KG pack: vocabulary, verb handlers, kind validation                                                    |
| `khive-pack-gtd`       | GTD pack: task lifecycle over the notes substrate                                                      |
| `khive-pack-memory`    | Memory pack: salience-weighted remember/recall with decay                                              |
| `khive-pack-brain`     | Brain pack: Bayesian user profiles, feedback, resolution                                               |
| `khive-pack-comm`      | Comm pack: threaded messaging with inbox                                                               |
| `khive-pack-schedule`  | Schedule pack: reminders and scheduled verb execution                                                  |
| `khive-pack-knowledge` | Knowledge pack: atom-based KB with embedding rerank search                                             |
| `khive-mcp`            | Stdio MCP binary — single `request` tool dispatching through the VerbRegistry                          |

Dependency direction (storage stack): `types → score → storage → db → query → runtime → packs → mcp`.
Side input: `request → mcp` (the DSL parser is consumed only at the MCP dispatch boundary;
packs do not depend on it).
Storage is trait-only; backends (SQLite today, Postgres tomorrow) implement the traits without
touching consumers.

---

## Quick start

**1. Install:**

```bash
npm install -g khive
```

**2. Add to your MCP config** (`.mcp.json` in your project, or `~/.claude/mcp.json` for
global):

```json
{ "mcpServers": { "khive": { "command": "khive", "args": ["mcp"] } } }
```

**That's it.** All 7 packs load by default, a background daemon auto-spawns to keep the runtime
warm, and Claude Code discovers the `request` tool with the full 63-verb catalog.

### Alternative: install via Cargo

If you prefer Rust tooling or need to build from source:

```bash
cargo install khive-mcp                        # from crates.io
# or:
git clone https://github.com/ohdearquant/khive.git && cd khive
cd crates && cargo build --release -p khive-mcp
```

Then point your MCP config at the binary directly:

```json
{ "mcpServers": { "khive": { "command": "khive-mcp" } } }
```

### Usage

The agent expresses verbs as DSL ops inside the single `request` tool:

```text
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\")")
request(ops="search(kind=\"entity\", query=\"parameter efficient fine-tuning\")")
request(ops="link(source_id=\"<uuid>\", target_id=\"<uuid>\", relation=\"variant_of\")")

# Batch multiple ops in one call:
request(ops="[create(...), create(...), link(...)]")
```

### Claude Code plugin (skills + agent)

For guided research workflows, install the marketplace plugin:

```
/plugin marketplace add ohdearquant/khive
/plugin install kg
```

This adds 4 workflow skills and a researcher agent:

| Skill         | What it does                                                    |
| ------------- | --------------------------------------------------------------- |
| `/kg:digest`  | Ingest material into the graph — extract entities, link, verify |
| `/kg:explore` | Discover what the graph knows — traverse, narrate, surface gaps |
| `/kg:connect` | Wire a new concept into existing knowledge — find relations     |
| `/kg:polish`  | Audit and fix — orphans, low-degree nodes, duplicates           |

### Configuration

```bash
khive mcp                                     # Default: ~/.khive/khive-graph.db
khive mcp --db /path/to/my.db                # Custom DB path
khive mcp --db :memory:                       # Ephemeral (testing)
khive mcp --namespace my-project              # Default namespace (default: "local")
khive mcp --no-embed                          # Disable local embedding model
khive mcp --log debug                         # Log level (default: warn)
```

Environment variables: `KHIVE_DB`, `KHIVE_NAMESPACE`, `KHIVE_NO_EMBED`, `KHIVE_LOG`.

### Development

```bash
cd crates && cargo test --workspace
make ci  # Full CI: fmt, clippy, test, build
```

Prerequisites: Rust 1.94+ (via [rustup](https://rustup.rs)),
Deno 2.x (for the TypeScript CLI layer — optional)

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

**v0.2.3 — published on [crates.io](https://crates.io/crates/khive-mcp).** 63 verbs across 7
packs, 9 entity kinds, 15 edge relations, daemon warm startup (ADR-049), knowledge search with
embedding rerank, Bayesian brain profiles, threaded messaging, scheduled verb execution.
Ready for use with Claude Code and any MCP-compatible agent.

## License

Apache 2.0 — see [LICENSE](LICENSE).
