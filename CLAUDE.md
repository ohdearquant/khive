# khive — Developer Guide

**What this is**: A research knowledge graph runtime. Typed entities, closed edge ontology,
hybrid search, GQL/SPARQL queries — all in a single 7.7MB Rust binary over MCP stdio.

**v0.1.0** — [crates.io](https://crates.io/crates/khive-mcp) | Apache 2.0

---

## Scope and intent

khive is a **structured persistence layer for AI research agents**. An agent reads papers, forms
concepts, links ideas, records decisions — khive gives that work a typed, queryable graph that
persists across sessions.

It is NOT a general-purpose database, a vector DB, or a chat memory system. It has opinions:
6 entity kinds, 13 edge relations, 5 note kinds — all closed sets. If your data doesn't fit the
schema, change how you model it, not the schema. Schema changes require an ADR.

---

## Data vs. view — the principle most violated here

Data is **history-preserving**: it records what happened and marks state. The query/view layer
decides **what is shown**. Different layers — never conflate them.

"Don't show stale / superseded / non-current info" is **always a view problem — never a reason
to delete, mutate, copy, or transfer data to "fix" what a query returns.** That's the currency
rule.

(Distinct from correctness. If a stored record is actually _wrong_, use the curation verbs —
`update` / `delete` / `merge` per [ADR-014](docs/adr/ADR-014-curation-operations.md). Curation
modifies data deliberately; the view-layer rule doesn't apply to deliberate correction.)

`supersedes` means _precisely_: keep the old record, mark it superseded; what a query returns is
a separate view-layer decision (filter superseded — do **not** rewrite or transfer its edges).

**Tell:** if you are mutating / copying / transferring stored relationships to make a _query
result_ look right, stop — wrong layer.

Corollary: the rule for any typed relation lives in its ADR (and consumer ADRs). If your intended
behavior isn't written there, it is an unspecified design decision → escalate, do not invent.

---

## Architecture (what ships today)

```
┌──────────────────────────────────────────────────────────────┐
│  khive-mcp     — stdio MCP server (the only binary)          │
│  11 verb tools: create get list update delete merge          │
│                 search link neighbors traverse query         │
└──────────────────────────────────────────────────────────────┘
                            ↕ VerbRegistry dispatch
┌──────────────────────────────────────────────────────────────┐
│  khive-pack-kg — KG vocabulary + 11 verb handlers (ADR-025)  │
└──────────────────────────────────────────────────────────────┘
                            ↕ in-process
┌──────────────────────────────────────────────────────────────┐
│  khive-runtime — composable Service API + VerbRegistry       │
│  khive-query   — GQL/SPARQL → SQL compiler                   │
│  khive-db      — SQLite + sqlite-vec + FTS5                  │
│  khive-storage — trait-only capability surface               │
│  khive-score   — deterministic i64 scoring                   │
│  khive-types   — domain types + Pack trait                   │
└──────────────────────────────────────────────────────────────┘
```

Dependency chain: `types → score → storage → db → query → runtime → pack-kg → mcp`.

Future layers (HTTP gateway, CLI, frontend) are planned but not shipped.

---

## Directory map

| Path                   | Purpose                                                     |
| ---------------------- | ----------------------------------------------------------- |
| `crates/khive-types`   | Domain types: Entity, Note, Event, EntityKind, EdgeRelation |
| `crates/khive-score`   | Deterministic i64 fixed-point scoring + RRF                 |
| `crates/khive-storage` | Trait-only: SqlAccess, GraphStore, VectorStore, TextSearch  |
| `crates/khive-db`      | SQLite backend + sqlite-vec + FTS5 trigram                  |
| `crates/khive-query`   | GQL + SPARQL parsers, AST validation, SQL compiler          |
| `crates/khive-runtime` | Service API + VerbRegistry + PackRuntime trait              |
| `crates/khive-pack-kg` | KG pack: vocabulary, verb handlers, kind validation         |
| `crates/khive-mcp`     | Stdio MCP binary — thin dispatch shell over VerbRegistry    |
| `docs/adr/`            | Architecture Decision Records (the design contract)         |
| `tests/smoke_test.py`  | End-to-end binary smoke test (all 11 tools)                 |
| `scripts/publish.sh`   | Publish all crates to crates.io in dependency order         |

---

## Closed taxonomies (DO NOT extend without an ADR)

### 6 entity kinds ([ADR-001](docs/adr/ADR-001-entity-kind-taxonomy.md))

`concept` | `document` | `dataset` | `project` | `person` | `org`

### 13 edge relations ([ADR-002](docs/adr/ADR-002-edge-ontology.md))

Structure: `contains` | `part_of` | `instance_of`
Derivation: `extends` | `variant_of` | `introduced_by` | `supersedes`
Dependency: `depends_on` | `enables`
Implementation: `implements`
Lateral: `competes_with` | `composed_with`
Annotation: `annotates`

### 5 note kinds ([ADR-019](docs/adr/ADR-019-note-kind-taxonomy.md))

`observation` | `insight` | `question` | `decision` | `reference`

Entity and note kinds are **pack-owned** ([ADR-025](docs/adr/ADR-025-pack-standard.md)) — the
`kg` pack declares them as static vocabulary; the runtime validates against all loaded packs.
Edge relations remain a **closed enum** (compile-time). Ad-hoc kinds/relations are rejected,
not silently accepted.

---

## MCP tool surface (11 tools, v0.1)

| Tool        | Params                                | What it does                                                  |
| ----------- | ------------------------------------- | ------------------------------------------------------------- |
| `create`    | `kind=entity\|note` + fields          | Create an entity or note                                      |
| `get`       | `id` (UUID)                           | Fetch any record — auto-detects entity/note/edge              |
| `list`      | `kind=entity\|edge\|note` + filters   | Structured browse with pagination                             |
| `update`    | `id` + patch fields                   | Patch entity (name/desc/props/tags) or edge (relation/weight) |
| `delete`    | `id`, `hard?`                         | Soft-delete (default) or hard-delete with edge cascade        |
| `merge`     | `into_id`, `from_id`                  | Deduplicate two entities (v0.1: entity-only)                  |
| `search`    | `kind=entity\|note`, `query`          | Hybrid FTS5 + vector search with RRF fusion                   |
| `link`      | `source_id`, `target_id`, `relation`  | Create a typed directed edge                                  |
| `neighbors` | `node_id`, `direction?`, `relations?` | Immediate graph neighbors                                     |
| `traverse`  | `roots`, `max_depth?`, `relations?`   | Multi-hop BFS with filters                                    |
| `query`     | GQL or SPARQL string                  | Pattern matching compiled to SQL                              |

`get`/`update`/`delete`/`merge` are UUID-only — no `kind` needed, the handler resolves
the substrate from the UUID. `create`/`list`/`search` require `kind`.

---

## Coding standards

### Rust

- **Workspace**: `crates/Cargo.toml`. Always `cargo check --workspace` after changes.
- **Clippy**: `cargo clippy --workspace --all-targets -- -D warnings` must pass. No exceptions.
- **Fmt**: `cargo fmt --all -- --check`. No exceptions.
- **Tests**: `cargo test --workspace`. Run before every commit.
- **No comments by default.** Only when the WHY is non-obvious. Never explain WHAT.
- **No stubs.** If you don't know how to implement, read the source until you do.
- **No premature abstraction.** Three similar lines > a helper used once.
- **No backwards-compat shims.** If something is unused, delete it.
- **Match existing patterns.** Read the file before editing it.

### Schema changes

- **Migrations only.** Schema changes go through `crates/khive-db/src/migrations.rs`.
  Add a new `VersionedMigration` with `version = <last + 1>`. Never edit V1.
- **Store DDL** (`NOTES_DDL`, etc.) must include new columns for test convenience,
  and `run_migrations` must handle the idempotency.

### MCP tool changes

- Tool params live in `crates/khive-mcp/src/tools/<verb>.rs` (schema only — `Serialize + Deserialize + JsonSchema`).
- MCP server (`crates/khive-mcp/src/server.rs`) is a thin dispatch shell — no business logic.
- **Verb handler logic lives in the pack** (`crates/khive-pack-kg/src/handlers.rs`).
- Runtime methods live in `crates/khive-runtime/src/operations.rs` (or `curation.rs`,
  `retrieval.rs`, `graph_traversal.rs`).
- **Invalid inputs return `McpError::invalid_params`** with the full list of valid values.
  Never silently coerce.

### Namespace isolation

- **Enforced at the runtime layer.** Every ID-based operation (get, delete, update, merge)
  must verify `record.namespace == caller_namespace` after fetching by UUID.
- Storage stores are ID-only. The runtime is the trust boundary.

### Edge cascade

- **Hard entity delete cascades incident edges.** No dangling references.
- Soft delete leaves edges in place — queries filter by `deleted_at IS NULL`.

---

## Development workflow

```bash
# Full CI (same as GitHub Actions)
make ci

# Individual checks
make check      # cargo check --workspace
make clippy     # clippy with -D warnings
make test       # cargo test --workspace
make fmt        # cargo fmt + deno fmt docs/
make fmt-check  # verify without modifying

# Build release binary
make build      # cargo build --workspace --release

# Publish to crates.io
make publish-dry  # dry run — validates all 7 crates
make publish      # live publish in dependency order
```

### Commits

Feature branches + PRs. Never push directly to main.

Conventional commits with crate scope: `feat(query): add SPARQL property filter`.

### Testing

- **Verify before claiming complete.** Run the test, check the output.
- **Report outcomes faithfully.** If tests fail, say so.
- **Integration > unit** for the MCP surface — the value is in the composition.
- **Smoke test**: `python3 tests/smoke_test.py` exercises all 11 tools end-to-end.

---

## ADR-driven development

Architecture Decision Records (`docs/adr/`) are the normative contract. Code implements what
ADRs specify. Changing the schema or interface requires an ADR **before** code lands.

Key ADRs for contributors:

| ADR                                                      | What it governs                                    |
| -------------------------------------------------------- | -------------------------------------------------- |
| [001](docs/adr/ADR-001-entity-kind-taxonomy.md)          | 6 entity kinds — don't add without this            |
| [002](docs/adr/ADR-002-edge-ontology.md)                 | 13 edge relations — closed set                     |
| [005](docs/adr/ADR-005-storage-capability-traits.md)     | Storage traits — the abstraction boundary          |
| [008](docs/adr/ADR-008-query-layer-separation.md)        | Query crate — parser/validator/compiler separation |
| [019](docs/adr/ADR-019-note-kind-taxonomy.md)            | 5 note kinds                                       |
| [022](docs/adr/ADR-022-schema-migrations.md)             | Migration system — how to change the DB schema     |
| [023](docs/adr/ADR-023-verb-consolidated-mcp-surface.md) | 11 MCP tools — the public contract                 |
| [025](docs/adr/ADR-025-pack-standard.md)                 | Pack trait — composable vocabulary extension       |

Full index: [docs/adr/README.md](docs/adr/README.md).

---

## What lives where

| Want to do...           | Edit this                                                                                    |
| ----------------------- | -------------------------------------------------------------------------------------------- |
| Add a new MCP tool      | `crates/khive-mcp/src/tools/` (params) + pack handler                                        |
| Add a verb handler      | `crates/khive-pack-kg/src/handlers.rs`                                                       |
| Add a runtime operation | `crates/khive-runtime/src/operations.rs`                                                     |
| Change DB schema        | `crates/khive-db/src/migrations.rs` (new version) + store DDL                                |
| Add a new entity kind   | `crates/khive-pack-kg/src/vocab.rs` + ADR-001 amendment                                      |
| Add a new edge relation | **STOP** — ADR change ([ADR-002](docs/adr/ADR-002-edge-ontology.md))                         |
| Add a new note kind     | `crates/khive-pack-kg/src/vocab.rs` + ADR-019 amendment                                      |
| Add a new pack          | New crate implementing `Pack` + `PackRuntime` ([ADR-025](docs/adr/ADR-025-pack-standard.md)) |
| Fix a query parser bug  | `crates/khive-query/src/parsers/` + add regression test                                      |
| Fix a storage bug       | `crates/khive-db/src/stores/` + test                                                         |

---

## Anti-patterns

- **Don't add a language SDK.** MCP is the universal interface. No Python/TS/Go client library.
- **Don't reimplement KG primitives outside `crates/`.** If you need entity CRUD in another
  language, call the MCP server — don't rewrite the storage logic.
- **Don't silently coerce invalid input.** Invalid entity kinds, note kinds, and edge relations
  must return errors with the valid values listed. Never `unwrap_or_default()`.
- **Don't bypass namespace isolation.** Every ID-based runtime method checks namespace.
  Storage is ID-only by design — the runtime enforces access control.
- **Don't edit V1 migrations.** Append a new version. V1 is immutable on existing databases.
- **Don't store research findings only as notes.** Notes are for context; entities + edges
  are for structure. If a concept is worth naming, it's an entity.
- **Don't optimize before measuring.** The bottleneck is rarely where you think.

---

## Cross-references

- **[lattice-embed](https://crates.io/crates/lattice-embed)**: Local embedding model inference.
  Consumed as a Rust dependency by `khive-runtime` (ADR-012).
- **[AGENTS.md](AGENTS.md)**: Guide for AI agents _using_ khive (not developing it).
