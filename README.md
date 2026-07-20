# khive

A research knowledge graph runtime for agents that need structure: typed substrates, closed
taxonomies, and a verb-consolidated MCP surface.

[![CI](https://github.com/ohdearquant/khive/actions/workflows/ci.yml/badge.svg)](https://github.com/ohdearquant/khive/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/khive-mcp.svg)](https://crates.io/crates/khive-mcp)
[![License: BUSL-1.1](https://img.shields.io/badge/License-BUSL--1.1-blue.svg)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-ohdearquant.github.io%2Fkhive-0969da)](https://ohdearquant.github.io/khive/)
[![Discord](https://img.shields.io/badge/Discord-join%20chat-5865F2?logo=discord&logoColor=white)](https://discord.gg/JDj9ENhUE8)

**[Documentation](https://ohdearquant.github.io/khive/)** &middot; **[Discord](https://discord.gg/JDj9ENhUE8)**

Vector search finds similar text. A knowledge graph finds _structure_: lineages, dependencies,
contradictions, gaps. khive gives your research agent a typed, queryable graph that grows as it
works. Read a paper and entities and edges fall out. Make a connection and it's traversable
immediately. Come back next session and the graph remembers what you built.

There's no Neo4j to run and no separate SPARQL endpoint to deploy. It's SQLite on disk, MCP over
stdio, and `cargo test` finishes in 4 seconds.

---

## What you get

| Capability                  | How                                                                                                                                                      |
| --------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **84 verbs, 11 packs**      | KG, GTD, memory, brain, comm, schedule, knowledge, session, git, workspace, blob: all load by default                                                    |
| **Typed entities**          | 9 closed kinds: concept, document, dataset, project, person, org, artifact, service, resource                                                            |
| **Typed edges**             | 17 closed relations in 9 categories (structure, derivation, provenance, temporal, dependency, impl, lateral, annotation, epistemic)                      |
| **Typed notes**             | 5 closed kinds: observation, insight, question, decision, reference                                                                                      |
| **Hybrid retrieval**        | FTS5 + vector RRF with embedding rerank; shipped BM25, HNSW, Vamana, and fusion crates for pack-specific retrieval paths                                 |
| **Graph traversal**         | BFS with depth/direction/relation filters, bidirectional shortest path                                                                                   |
| **GQL + SPARQL queries**    | Parse to SQL, run against the same SQLite backend                                                                                                        |
| **Salience-weighted notes** | Notes carry salience scores; search ranks by semantic relevance × salience                                                                               |
| **Cross-substrate links**   | Notes annotate entities (and vice versa) via the same edge system                                                                                        |
| **Soft delete + supersede** | History-preserving: old records stay, newer ones supersede via graph edges                                                                               |
| **Namespace attribution**   | Every record is stamped with a namespace and all records share one store by default. Operators can supply a custom Gate for namespace-scoped enforcement |

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

One MCP tool: `request` (ADR-016 + ADR-027). Every verb is a parsed op inside it.

```
request(ops="verb(arg=value, arg=value)")              # single op
request(ops="[v1(...), v2(...), v3(...)]")             # parallel batch (max 100)
request(ops="[{\"tool\":\"v1\",\"args\":{...}}, ...]") # equivalent JSON form
```

All 11 packs load by default, giving **84 verbs** out of the box (regenerate with
`request(ops="verbs()")` before editing this table):

| Pack          | Prefix       | Verbs | What it does                                                                                   |
| ------------- | ------------ | ----- | ---------------------------------------------------------------------------------------------- |
| **kg**        | _(bare)_     | 18    | Entities, edges, notes, graph queries, reference resolution                                    |
| **gtd**       | `gtd.`       | 5     | Task lifecycle (inbox → next → active → done)                                                  |
| **memory**    | `memory.`    | 5     | Salience-weighted remember / decay-ranked recall                                               |
| **brain**     | `brain.`     | 15    | Bayesian user profiles + feedback loop                                                         |
| **comm**      | `comm.`      | 7     | Threaded messaging                                                                             |
| **schedule**  | `schedule.`  | 4     | Reminders and scheduled verb execution                                                         |
| **knowledge** | `knowledge.` | 19    | Atom-based KB with embedding rerank search                                                     |
| **session**   | `session.`   | 4     | Session record persistence (store/list/resume/export)                                          |
| **git**       | `git.`       | 4     | `git.digest` provenance ingestion + `git.commit`/`git.branch`/`git.push` write verbs (ADR-108) |
| **workspace** | _(none)_     | 0     | Adds the `workspace` entity kind + `contains` endpoint rules to git/gtd/session notes (#873)   |
| **blob**      | `blob.`      | 3     | Content-addressed object put/get/stat over the `BlobStore` CAS trait (ADR-111)                 |

`create`, `list`, `search` take `kind=entity|note` (or `kind=edge` for `list`).
`get`, `update`, `delete`, `merge` are UUID-only: they auto-detect the record type.

Agents reach khive via MCP stdio, using Python, TypeScript, Rust, or any MCP-compatible client, so there is no language SDK to learn.

---

## What typed edges add beyond vector similarity

A vector index returns "these two texts are close." It has no idea that document B introduced
concept A, that concept C is a variant of concept A, or that concept D was superseded by concept
E last month. Cosine distance carries no direction and no type. It can't tell you what a
relationship _is_, only that something is nearby.

khive's graph carries both signals. Every edge is one of 17 closed relations across 9
categories: structure (`contains`, `part_of`, `instance_of`), derivation (`extends`,
`variant_of`, `introduced_by`, `supersedes`), provenance (`derived_from`), temporal
(`precedes`), dependency (`depends_on`, `enables`), implementation (`implements`), lateral
(`competes_with`, `composed_with`), annotation (`annotates`), and epistemic (`supports`,
`refutes`).

The following example, run against a scratch database, shows the difference in practice
(full transcript in [`demos/research-ingest.md`](demos/research-ingest.md)):

```
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention\")")
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention-2\", description=\"Improved parallelism and work partitioning over FlashAttention\")")
request(ops="link(source_id=\"<fa2_id>\", target_id=\"<fa_id>\", relation=\"extends\")")
request(ops="traverse(roots=[\"<fa_id>\"], max_depth=2)")
```

`traverse` returns the lineage directly: FlashAttention-2 reaches FlashAttention across one
`extends` edge, with the edge id and relation name attached to the path. A vector search over
the same two entities returns a similarity score between two chunks of text, with no way to tell
which one came first or which direction the relationship runs.

khive runs both signals together. `search` combines FTS5 and vector similarity through RRF
fusion to find candidates. `traverse`, `neighbors`, and GQL/SPARQL walk the typed edges to show
how those candidates actually relate. Similarity surfaces what's nearby in meaning. The graph
records what's connected, in which direction, and why.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  kkernel mcp:     stdio MCP server (the `kkernel` binary)     │
│  khived:          persistent daemon (ADR-049): warm runtime   │
│                     auto-spawned on first request             │
│  1 tool: `request` (ADR-016 + ADR-027): parses DSL,           │
│  dispatches each op through the VerbRegistry                  │
└──────────────────────────────────────────────────────────────┘
                            ↕ VerbRegistry dispatch
┌──────────────────────────────────────────────────────────────┐
│  khive-pack-kg:        KG vocabulary + 18 verb handlers       │
│  khive-pack-gtd:       task lifecycle (5 verbs)               │
│  khive-pack-memory:    salience + decay recall (5 verbs)      │
│  khive-pack-brain:     Bayesian profiles (15 verbs)           │
│  khive-pack-comm:      threaded messaging (7 verbs)           │
│  khive-pack-schedule:  reminders + scheduled ops (4 verbs)    │
│  khive-pack-knowledge: atom KB + embedding rerank (19 verbs)  │
│  khive-pack-session:   session record persistence (4 verbs)   │
│  khive-pack-git:       provenance ingest + writes (4 verbs)   │
└──────────────────────────────────────────────────────────────┘
                            ↕ in-process
┌──────────────────────────────────────────────────────────────┐
│  khive-runtime, khive-request, khive-query, khive-db,         │
│  khive-storage, khive-score, khive-types                      │
└──────────────────────────────────────────────────────────────┘
```

Embedded SQLite storage with FTS5 trigram text search and sqlite-vec vector search.
Retrieval ships in-process BM25, HNSW, Vamana, and fusion crates for hybrid and pack-specific
search paths. One binary, one DB file, no external services to run.

The **khived daemon** (ADR-049) keeps the runtime warm between MCP sessions: the embedding model
stays loaded, SQLite connections stay open, and pack registries stay initialized. It auto-spawns on
first request and persists in the background, eliminating cold-start overhead on reconnect.

HTTP gateway and visual frontend are planned for future releases. The `kkernel` admin CLI
(migrations, reindexing, data import/export, diagnostics) ships today, documented in
[docs/operations.md](docs/operations.md).

---

## Performance

Knowledge search runs on an in-process Vamana ANN index (`khive-vamana`). On the standard
SIFT-1M benchmark, the index returns **recall@10 of 0.95 at a p50 query latency of 171µs over
1,000,000 vectors**, measured on a single laptop (macos-arm64, commit `eb6696c`). Tail latency
stays under 250µs and the index scales sublinearly as the corpus grows from 100K to 1M vectors:

| Vectors | Recall@10 | p50   | p95   | p99   | Build  |
| ------- | --------- | ----- | ----- | ----- | ------ |
| 100K    | 0.9504    | 71µs  | 93µs  | 102µs | 11.1s  |
| 316K    | 0.9523    | 130µs | 177µs | 200µs | 54.6s  |
| 1M      | 0.9521    | 171µs | 216µs | 234µs | 320.8s |

Query latency grows about 2.4x while the corpus grows 10x, so the search path is sublinear in
corpus size over this range. Index build is a one-time cost paid at construction; it grows
super-linearly here (about 29x build time for the 10x corpus).

The speedups over exhaustive search implied by these latencies (89x at 100K, 153x at 316K, 341x
at 1M) are computed against a back-derived brute-force baseline rather than a directly measured
one (see [#167](https://github.com/ohdearquant/khive/issues/167)); treat them as indicative, not
as a measured headline figure. The benchmark harness lives in `perf/`; raw data is in
[`perf/ledger.csv`](perf/ledger.csv).

---

## Crates

| Crate                  | Purpose                                                                                                  |
| ---------------------- | -------------------------------------------------------------------------------------------------------- |
| `khive-types`          | Domain types, Pack trait, closed enums                                                                   |
| `khive-score`          | Deterministic i64 fixed-point scoring                                                                    |
| `khive-storage`        | Trait-only capability surface (zero implementations)                                                     |
| `khive-db`             | SQLite backend: entity/note/edge tables, FTS5 TextSearch, current sqlite-vec VectorStore compatibility   |
| `khive-retrieval`      | Hybrid retrieval primitives                                                                              |
| `khive-fusion`         | RRF, weighted, union, vector-only, and keyword-only fusion strategies                                    |
| `khive-bm25`           | BM25 keyword index                                                                                       |
| `khive-hnsw`           | HNSW vector index                                                                                        |
| `khive-vamana`         | Vamana ANN index used by knowledge search                                                                |
| `khive-query`          | SPARQL / GQL → SQL compiler                                                                              |
| `khive-runtime`        | Service API + VerbRegistry + PackRuntime trait                                                           |
| `khive-request`        | Request DSL parser (function-call, JSON; pipe / LNDL planned). Transport-agnostic AST.                   |
| `khive-pack-kg`        | KG pack: vocabulary, verb handlers, kind validation                                                      |
| `khive-pack-gtd`       | GTD pack: task lifecycle over the notes substrate                                                        |
| `khive-pack-memory`    | Memory pack: salience-weighted remember/recall with decay                                                |
| `khive-pack-brain`     | Brain pack: Bayesian user profiles, feedback, resolution                                                 |
| `khive-pack-comm`      | Comm pack: threaded messaging with inbox                                                                 |
| `khive-pack-schedule`  | Schedule pack: reminders and scheduled verb execution                                                    |
| `khive-pack-knowledge` | Knowledge pack: atom-based KB with embedding rerank search                                               |
| `khive-mcp`            | MCP server library: single `request` tool dispatching through the VerbRegistry (served by `kkernel mcp`) |
| `kkernel`              | The single shipped binary: `kkernel mcp` serves MCP; admin subcommands (exec, reindex, db, …)            |

Dependency direction (storage stack): `types → score → storage → db → query → runtime → packs → mcp`.
Side input: `request → mcp` (the DSL parser is consumed only at the MCP dispatch boundary;
packs do not depend on it).
Storage is trait-only; backends (SQLite today, Postgres tomorrow) implement the traits without
touching consumers.

---

## Quick start

**1. Install** (from [crates.io](https://crates.io/crates/khive-mcp), currently at `0.4.0`; `0.5.0` publishes with this release):

```bash
cargo install kkernel
```

`kkernel` is the single shipped binary; `kkernel mcp` serves the MCP `request` surface. If you
don't have Rust, install it first via [rustup](https://rustup.rs).

**2. Add to your MCP config** (`.mcp.json` in your project, or `~/.claude/mcp.json` for
global):

```json
{ "mcpServers": { "khive": { "command": "kkernel", "args": ["mcp"] } } }
```

**3. Verify:**

```bash
kkernel --version   # confirms the binary and version you just installed
```

All 11 packs load by default, a background daemon auto-spawns to keep the runtime warm, and any
MCP client discovers the `request` tool with the full 84-verb catalog.

### Alternative: npm

An npm-distributed `khive` package also exists, but the published version there can lag behind
the latest crates.io release. Check `khive --version` (or `khive-mcp --version`) after install
and compare it to the [crates.io version](https://crates.io/crates/khive-mcp) before relying on
features documented here.

```bash
npm install -g khive
```

```json
{ "mcpServers": { "khive": { "command": "khive", "args": ["mcp"] } } }
```

### Build from source

```bash
git clone https://github.com/ohdearquant/khive.git && cd khive
cd crates && cargo build --release -p kkernel
# binary at crates/target/release/kkernel
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

### Claude Desktop

Add the same server entry to `claude_desktop_config.json`:

```json
{ "mcpServers": { "khive": { "command": "kkernel", "args": ["mcp"] } } }
```

See [docs/guide/getting-started.md](docs/guide/getting-started.md) for the config file location
on each platform.

### Claude Code plugin (skills + agent)

For guided research workflows, install the marketplace plugin:

```
/plugin marketplace add ohdearquant/khive
/plugin install kg
```

This adds 4 workflow skills and a researcher agent:

| Skill         | What it does                                                   |
| ------------- | -------------------------------------------------------------- |
| `/kg:digest`  | Ingest material into the graph: extract entities, link, verify |
| `/kg:explore` | Discover what the graph knows: traverse, narrate, surface gaps |
| `/kg:connect` | Wire a new concept into existing knowledge: find relations     |
| `/kg:polish`  | Audit and fix: orphans, low-degree nodes, duplicates           |

### Configuration

```bash
khive mcp                                     # Default: ~/.khive/khive.db
khive mcp --db /path/to/my.db                # Custom DB path
khive mcp --db :memory:                       # Ephemeral (testing)
khive mcp --namespace my-project              # Default namespace (default: "local")
khive mcp --no-embed                          # Disable local embedding model
khive mcp --log debug                         # Log level (default: warn)
```

Environment variables: `KHIVE_DB`, `KHIVE_NAMESPACE`, `KHIVE_NO_EMBED`, `KHIVE_LOG`.

For config file discovery order, the `[[backends]]` model, and how `--db` interacts
with a declared backend topology, see [docs/configuration.md](docs/configuration.md).

For the `kkernel` admin CLI (migrations, reindexing, data import/export, diagnostics), see
[docs/operations.md](docs/operations.md).

### Development

```bash
cd crates && cargo test --workspace
make ci  # Full CI: fmt, clippy, test, build
```

Prerequisites: Rust 1.93+ (workspace MSRV) (via [rustup](https://rustup.rs)),
Deno 2.x (for the TypeScript CLI layer, optional)

- Node.js 20+ and pnpm (for frontend, optional)

---

## Demos

Runnable, copy-pasteable transcripts against a scratch database. See [`demos/`](demos/):

- [`demos/research-ingest.md`](demos/research-ingest.md): create entities, link them, search,
  and traverse the graph
- [`demos/gtd-memory.md`](demos/gtd-memory.md): task lifecycle and salience-weighted memory
  recall

Docs: [ohdearquant.github.io/khive](https://ohdearquant.github.io/khive/) (agents: fetch
[`/llms.txt`](https://ohdearquant.github.io/khive/llms.txt)). Questions and discussion:
[Discord](https://discord.gg/JDj9ENhUE8).

---

## Contributing

- Feature branches + PRs. Never push directly to main.
- `make ci` must pass (fmt, clippy, test, no-default-features check, release build).
- Conventional commits: `feat(types): add NoteKind taxonomy`.
- Schema/interface changes need a design doc. Propose it in the PR or as an issue.
- See [CLAUDE.md](CLAUDE.md) for the developer guide, [AGENTS.md](AGENTS.md) for agent usage.

---

## Status

**v0.5.0 (publication pending; crates.io currently serves 0.4.0).** 84 verbs across 11
packs, 9 entity kinds, 17 edge relations, daemon warm startup (ADR-049), knowledge search with
embedding rerank, Bayesian brain profiles, threaded messaging, scheduled verb execution.
Ready for use with Claude Code and any MCP-compatible agent.

## License

Business Source License 1.1. See [LICENSE](LICENSE).

You may use, modify, and redistribute khive freely, including in production,
with one restriction: offering khive itself to third parties as a competing
hosted or embedded service requires a commercial license. On the Change Date
(2030-07-20) each released version converts automatically to Apache 2.0.

Versions published to crates.io and npm before 2026-07-20 remain under
Apache 2.0; every release from this date forward ships under the Business
Source License 1.1.
