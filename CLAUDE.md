# khive — Developer Guide

**What this is**: A research knowledge graph runtime. Typed entities, closed edge ontology,
hybrid search, GQL/SPARQL queries — all in a single ~21MB Rust binary over MCP stdio (see #1055;
measure a fresh `main` build before citing an exact figure — it drifts).

**v0.3.0** — [crates.io](https://crates.io/crates/khive-mcp) | Apache 2.0

---

## Scope and intent

khive is a **structured persistence layer for AI research agents**. An agent reads papers, forms
concepts, links ideas, records decisions — khive gives that work a typed, queryable graph that
persists across sessions.

It is NOT a general-purpose database, a vector DB, or a chat memory system. It has opinions:
9 entity kinds, 17 edge relations, 5 note kinds — all closed sets. If your data doesn't fit the
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
│  kkernel        — binary frontend (`crates/kkernel/src/main.rs`) │
│  Ships the CLI (`kkernel::cli`) and hosts the MCP server via │
│  `kkernel mcp`. Auto-spawns `kkernel mcp --daemon` for warm  │
│  ANN/embedder state (ADR-049); daemon keeps indexes hot      │
│  across sessions.                                             │
└──────────────────────────────────────────────────────────────┘
                            ↕ links
┌──────────────────────────────────────────────────────────────┐
│  khive-mcp      — MCP server library, no binary of its own    │
│  1 tool: `request` (ADR-016) — parses DSL,                   │
│  dispatches verb ops through the VerbRegistry                │
└──────────────────────────────────────────────────────────────┘
                            ↕ VerbRegistry dispatch
┌──────────────────────────────────────────────────────────────┐
│  12 default packs (`RuntimeConfig::built_in_packs()`):        │
│  kg, gtd, memory, brain, comm, schedule, knowledge, session, │
│  git, code, workspace, blob — together exposing               │
│  91 public verbs (see the verb-catalog paragraph below        │
│  for the per-pack breakdown)                                   │
│  khive-vcs         — KG versioning: snapshots/branches (ADR-010)    │
│  khive-merge       — KG merge algorithm (ADR-039, forward-deployed,  │
│                       excluded from workspace members)               │
└──────────────────────────────────────────────────────────────┘
                            ↕ in-process
┌──────────────────────────────────────────────────────────────┐
│  khive-runtime — composable Service API + VerbRegistry       │
│  khive-request — DSL parser (function-call + JSON forms)     │
│  khive-query   — GQL/SPARQL → SQL compiler                   │
│  khive-db      — SQLite storage + FTS5 TextSearch + sqlite-vec VectorStore compatibility │
│  retrieval     — khive-retrieval/fusion/bm25/hnsw/vamana engines and fusion primitives   │
│  khive-storage — contracts + neutral request context          │
│  khive-score   — deterministic i64 scoring                   │
│  khive-types   — domain types + Pack trait                   │
└──────────────────────────────────────────────────────────────┘
```

Dependency chain (storage stack): `types → score → storage → db → query → runtime → packs → khive-mcp → kkernel`.
Side input: `request → mcp` (the DSL parser is consumed only at the MCP dispatch boundary;
packs do not depend on it).

Future layers (HTTP gateway, CLI, frontend, LNDL frontend in `khive-request`) are planned but
not shipped.

---

## Directory map

| Path                            | Purpose                                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/khive-types`            | Domain types: Entity, Note, Event, EntityKind, EdgeRelation, Pack trait                                                                                                                                                                                                                                                                                                           |
| `crates/khive-score`            | Deterministic i64 fixed-point scoring + RRF                                                                                                                                                                                                                                                                                                                                       |
| `crates/khive-storage`          | Capability contracts plus backend-neutral request cancellation/deadline context                                                                                                                                                                                                                                                                                                   |
| `crates/khive-db`               | SQLite backend; FTS5 trigram TextSearch; current sqlite-vec VectorStore compatibility                                                                                                                                                                                                                                                                                             |
| `crates/khive-retrieval`        | Hybrid retrieval primitives over dense, lexical, graph, and fusion signals                                                                                                                                                                                                                                                                                                        |
| `crates/khive-fusion`           | RRF, weighted, union, vector-only, and keyword-only fusion strategies                                                                                                                                                                                                                                                                                                             |
| `crates/khive-bm25`             | BM25 keyword index                                                                                                                                                                                                                                                                                                                                                                |
| `crates/khive-hnsw`             | HNSW vector index                                                                                                                                                                                                                                                                                                                                                                 |
| `crates/khive-vamana`           | Vamana ANN index used by knowledge search                                                                                                                                                                                                                                                                                                                                         |
| `crates/khive-query`            | GQL + SPARQL parsers, AST validation, SQL compiler                                                                                                                                                                                                                                                                                                                                |
| `crates/khive-runtime`          | Service API + VerbRegistry + PackRuntime trait                                                                                                                                                                                                                                                                                                                                    |
| `crates/khive-request`          | Request DSL parser (function-call + JSON; pipe/LNDL planned)                                                                                                                                                                                                                                                                                                                      |
| `crates/khive-pack-kg`          | KG pack: vocabulary, 20 verb handlers, kind validation                                                                                                                                                                                                                                                                                                                            |
| `crates/khive-pack-gtd`         | GTD pack: 5 verbs over notes (assign / next / complete / tasks / transition)                                                                                                                                                                                                                                                                                                      |
| `crates/khive-pack-memory`      | Memory pack: `remember`/`recall`/`feedback` verbs, decay-weighted recall ([ADR-021](docs/adr/ADR-021-memory-pack.md))                                                                                                                                                                                                                                                             |
| `crates/khive-pack-brain`       | Brain pack: profile management registry, Bayesian routing/feedback verbs                                                                                                                                                                                                                                                                                                          |
| `crates/khive-pack-comm`        | Comm pack: threaded messaging, inbox/delivery/cursor verbs (ten public `comm.*` verbs)                                                                                                                                                                                                                                                                                            |
| `crates/khive-pack-schedule`    | Schedule pack: `schedule.remind`/`schedule`/`agenda`/`cancel` over `scheduled_event` notes                                                                                                                                                                                                                                                                                        |
| `crates/khive-pack-knowledge`   | Knowledge pack: domain/atom corpus verbs, search, compose, suggest                                                                                                                                                                                                                                                                                                                |
| `crates/khive-pack-session`     | Session pack: session storage/list/resume/export verbs                                                                                                                                                                                                                                                                                                                            |
| `crates/khive-pack-git`         | Git pack: commit/issue/pull_request note kinds, `git.digest`, and write verbs `git.commit`/`git.branch`/`git.push` ([ADR-088](docs/adr/ADR-088-git-lifecycle-pack.md), [ADR-108](docs/adr/ADR-108-git-write-surface.md))                                                                                                                                                          |
| `crates/khive-pack-code`        | Code pack: code concept vocabulary, finding-note lifecycle, `code.ingest` ([ADR-085](docs/adr/ADR-085-code-pack.md))                                                                                                                                                                                                                                                              |
| `crates/khive-pack-workspace`   | Workspace pack: `workspace` entity vocabulary and membership rules; zero verbs                                                                                                                                                                                                                                                                                                    |
| `crates/khive-pack-blob`        | Blob pack: `blob.put`/`blob.get`/`blob.stat` over the `BlobStore` CAS trait ([ADR-111](docs/adr/ADR-111-blob-store.md))                                                                                                                                                                                                                                                           |
| `crates/khive-pack-moodboard`   | Opt-in experimental visual-asset ingest, exact descriptor-space retrieval, and actor-scoped calibrated pairwise preference learning using BlobStore, Lattice embeddings, named Khive vectors, and a FANN serving head ([ADR-148](docs/adr/ADR-148-moodboard-visual-retrieval-pack.md), [ADR-149](docs/adr/ADR-149-moodboard-preference-learning.md)); not in the default pack set |
| `crates/khive-pack-agent`       | Agent pack: spawn/resume/suspend/observe wire surface; not self-registered — an embedder constructs it and registers it manually via `RegistryBuilder::register`, and it is outside the default pack set ([ADR-142](docs/adr/ADR-142-agentic-process-runtime.md))                                                                                                                 |
| `crates/khive-pack-template`    | Reference scaffold for dynamically registered packs; developer-only, not in the default pack set                                                                                                                                                                                                                                                                                  |
| `crates/khive-pack-formal`      | Formal-methods pack: typed edge endpoint rules for six formal-math concept subtypes (theorem, definition, structure, instance, axiom, goal); pure ontology, no verbs ([ADR-069](docs/adr/ADR-069-subject-model.md)); not in the default pack set                                                                                                                                  |
| `crates/khive-brain-core`       | Brain primitives: Beta posteriors, section types, profile state, weight derivation (used by `khive-pack-brain`)                                                                                                                                                                                                                                                                   |
| `crates/khive-changeset`        | KG change-set op-list model and NDJSON-delta codec                                                                                                                                                                                                                                                                                                                                |
| `crates/khive-fold`             | Fold, Anchor, Objective, and Selector primitives with deterministic ordering                                                                                                                                                                                                                                                                                                      |
| `crates/khive-quant`            | SQ8 scalar quantization codecs for approximate distance computation in ANN indexes                                                                                                                                                                                                                                                                                                |
| `crates/khive-text`             | Text analysis primitives: tokenization, normalization, filtering                                                                                                                                                                                                                                                                                                                  |
| `crates/khive-gate`             | Authorization request/decision/obligation/audit/gate interfaces ([ADR-018](docs/adr/ADR-018-authorization-gate.md))                                                                                                                                                                                                                                                               |
| `crates/khive-gate-rego`        | Rego policy backend for `khive-gate::Gate`, powered by `regorus`                                                                                                                                                                                                                                                                                                                  |
| `crates/khive-vcs`              | KG versioning: content-addressed snapshots, branch pointers, push/pull ([ADR-010](docs/adr/ADR-010-kg-versioning.md))                                                                                                                                                                                                                                                             |
| `crates/khive-vcs-adapters`     | Pure KG import format adapters and intermediate wire records (consumed by `khive-vcs`)                                                                                                                                                                                                                                                                                            |
| `crates/khive-merge`            | KG merge: three-way merge with LCA walk, conflict enum, strategy shortcuts ([ADR-039](docs/adr/ADR-039-note-merge.md)); forward-deployed v2, excluded from workspace members                                                                                                                                                                                                      |
| `crates/khive-channel`          | Channel transport abstraction: `Channel` trait, `ChannelEnvelope`, `ChannelRegistry` ([ADR-056](docs/adr/ADR-056-channel-transport-layer.md))                                                                                                                                                                                                                                     |
| `crates/khive-channel-email`    | Email (SMTP/IMAP) channel adapter implementing the `Channel` trait                                                                                                                                                                                                                                                                                                                |
| `crates/khive-channel-telegram` | Telegram Bot API channel adapter implementing the `Channel` trait                                                                                                                                                                                                                                                                                                                 |
| `crates/khive-mcp`              | MCP server library — single `request` tool over VerbRegistry; ships no binary of its own, hosted by `kkernel mcp`                                                                                                                                                                                                                                                                 |
| `crates/kkernel`                | Binary frontend — CLI (`kkernel::cli`) and admin/management library; hosts the MCP server and auto-spawns the warm-state daemon                                                                                                                                                                                                                                                   |
| `docs/adr/`                     | Architecture Decision Records (the design contract)                                                                                                                                                                                                                                                                                                                               |
| `marketplace/`                  | The `khive` umbrella Claude Code plugin (one skill per pack + kg agents) — install via `/plugin install`                                                                                                                                                                                                                                                                          |
| `tests/smoke_test.py`           | End-to-end binary smoke test (drives every verb via the `request` DSL)                                                                                                                                                                                                                                                                                                            |
| `scripts/publish.sh`            | Publish all crates to crates.io in dependency order                                                                                                                                                                                                                                                                                                                               |

---

## Closed taxonomies (DO NOT extend without an ADR)

### 9 entity kinds ([ADR-001](docs/adr/ADR-001-entity-kind-taxonomy.md), [ADR-048](docs/adr/ADR-048-knowledge-section-profiles.md))

`concept` | `document` | `dataset` | `project` | `person` | `org` | `artifact` | `service` | `resource`

### 17 edge relations ([ADR-002](docs/adr/ADR-002-edge-ontology.md) base 15; [ADR-055](docs/adr/ADR-055-epistemic-edge-relations.md) +2 epistemic)

Structure: `contains` | `part_of` | `instance_of`
Derivation: `extends` | `variant_of` | `introduced_by` | `supersedes`
Provenance: `derived_from`
Temporal: `precedes`
Dependency: `depends_on` | `enables`
Implementation: `implements`
Lateral: `competes_with` | `composed_with`
Annotation: `annotates`
Epistemic: `supports` | `refutes`

### 5 note kinds ([ADR-013](docs/adr/ADR-013-note-kind-taxonomy.md))

`observation` | `insight` | `question` | `decision` | `reference`

Entity and note kinds are **pack-owned** ([ADR-017](docs/adr/ADR-017-pack-standard.md)) — the
`kg` pack declares them as static vocabulary; the runtime validates against all loaded packs.
Edge relations remain a **closed enum** (compile-time). Ad-hoc kinds/relations are rejected,
not silently accepted.

The per-relation **endpoint contract** (which `(source, relation, target)` triples are legal)
is the ADR-002 base contract _plus_ any pack-declared additions
([ADR-017](docs/adr/ADR-017-pack-standard.md) §"Pack-extensible edge endpoints"). The GTD pack
uses this to allow `depends_on` between two `task` notes — base contract alone would reject a
note source for non-`annotates` relations. Rules are additive only; packs cannot tighten the
base contract.

---

## MCP tool surface (one tool: `request` — ADR-016)

The MCP server exposes a single tool named `request` that accepts a verb-dispatch DSL string
and routes each parsed op through the loaded packs. Verb taxonomy and semantics are unchanged
from ADR-023 — only the wire shape moved.

```
# Single op
request(ops="verb(arg=value, arg=value)")

# Parallel batch (max 100, no inter-op ordering)
request(ops="[v1(...), v2(...), v3(...)]")

# JSON form (equivalent)
request(ops="[{\"tool\":\"v1\",\"args\":{...}}, ...]")
```

Verbs come from whichever packs are loaded via `KHIVE_PACKS` (env) or `--pack` (CLI). Default
loads all 12 production packs: kg, gtd, memory, brain, comm, schedule, knowledge, session, git,
code, workspace, blob (the aggregate is registry-discovered with `verbs()`; the `code` pack contributes one verb, `code.ingest`
(ADR-085 Amendment 2, PR #1039 — L1 manifest + L1.5 import-scan source ingest into a
dedicated map database); its `finding` note kind and `findings.json` batch ingest are
still reached only through the `kkernel code-ingest` admin CLI path (ADR-085 Amendment
3), never the MCP verb surface; git contributes
commit/issue/pull_request note kinds, a batch ingester, the git.digest verb (ADR-088
Amendment 1), and three write verbs — git.commit / git.branch / git.push — that shell to
system git with hardened, allowlisted argv construction and unconditional force-push denial
(ADR-108); comm.probe (#644) added 2026-07-07; brain.event_counts (ADR-103 Stage 1, #724
Ask A) added 2026-07-08; kg.resolve added 2026-07-09; workspace (#873) contributes zero verbs,
adding only the `workspace` entity kind and `contains` endpoint rules to git/gtd/session notes;
blob contributes three verbs, blob.put/blob.get/blob.stat (ADR-111), over the `BlobStore`
content-addressed storage trait; a normal file-backed boot installs a default `FsBlobStore`
beside the database file with no config needed, and the verbs stay unconfigured only
against an in-memory backend;
regenerate via `request(ops="verbs()")` before editing this line).

### KG pack verbs (20 — ADR-017, ADR-046, ADR-089)

`create`, `list`, and `search` take a `kind` discriminant. It accepts either the substrate-level
name (`entity`, `note`, `edge`) **or** a pack-registered granular kind (`concept`, `document`,
`task`, `observation`, …). The registry resolves which substrate the granular form lives in.
Mixing a granular `kind` with a contradicting `entity_kind`/`note_kind` sub-filter is rejected.

| Verb             | Args                                                                                                                | What it does                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| ---------------- | ------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create`         | `kind=<substrate\|granular>` + fields                                                                               | Create an entity or note                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `get`            | `id` (UUID)                                                                                                         | Fetch any record — auto-detects entity/note/edge                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `list`           | `kind=<substrate\|granular>\|edge` + filters                                                                        | Structured browse with pagination                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `stats`          | —                                                                                                                   | Return aggregate KG substrate counts (entities, edges, notes)                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `update`         | `id` + patch fields                                                                                                 | Patch entity (name/desc/props/tags) or edge (relation/weight)                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `delete`         | `id`, `hard?`                                                                                                       | Soft-delete (default) or hard-delete with edge cascade                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `merge`          | `into_id`, `from_id`                                                                                                | Deduplicate two entities (v0.1: entity-only)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `search`         | `kind=<substrate\|granular>`, `query`                                                                               | Hybrid FTS5 + vector search with RRF fusion                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `link`           | `source_id`, `target_id`, `relation`                                                                                | Create a typed directed edge                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `neighbors`      | `node_id`, `direction?`, `relations?`, `include_entity_type?`                                                       | Immediate graph neighbors; `include_entity_type=true` adds entity subtype to each hit                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `traverse`       | `roots`, `max_depth?`, `direction?`, `relations?`, `min_weight?`, `limit?`, `include_roots?`, `include_properties?` | Bounded BFS: ≤100 roots, depth ≤10, default/max 100/1,000 results per root, shared 100k-row/5s storage-expansion budget                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `query`          | GQL or SPARQL string, `page_size?` (`limit?` deprecated alias)                                                      | Deterministically paged pattern matching compiled to SQL; GQL continues with `SKIP next_offset`                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `propose`        | `title`, `description`, `changeset`                                                                                 | Create an event-sourced change proposal (ADR-046)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `review`         | `id`, `decision`, `comment?`                                                                                        | Approve, reject, or comment on a proposal                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `withdraw`       | `id`, `rationale?`                                                                                                  | Rescind an open proposal (proposer-only)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `resolve`        | `refs`, `kind?`, `limit?`                                                                                           | Resolve natural-language references to ids (id passthrough, recent-ring, hybrid-search fallback)                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `verbs`          | `category?`, `pack?`                                                                                                | List all MCP-callable verbs registered on this server                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `context`        | `query?`, `entity_ids?`, `hops?`, `budget?`, `relations?`, `direction?`, `limit?`, `fanout?`                        | Entity-anchored graph context in one call (ADR-089)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `whoami`         | —                                                                                                                   | Report the caller's already-resolved identity and namespace scope; never returns credentials                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `db_diagnostics` | —                                                                                                                   | Writer-contention, graph-edge integrity, FTS5 segment/maintenance, and WAL/checkpoint diagnostics: aggregate plus pooled/standalone/writer-task acquisition counters, pooled checkout timeouts, audit failures, build identity, duplicate edge-ID and list-ledger counts, bounded one-row FTS structure readings, checkpoint/FTS maintenance counters, PASSIVE checkpoint probe, WAL file size, and explicitly qualified WAL-pin census. The probe may backfill WAL frames (normal checkpoint I/O); it never changes logical state, increments write-traffic acquisitions, escalates to TRUNCATE, or deletes sidecar evidence |

### GTD pack verbs (5 — ADR-019, optional)

Load with `KHIVE_PACKS=kg,gtd` or `--pack gtd`. Adds the `task` note kind.

| Verb             | Args                                                                         | What it does                                                           |
| ---------------- | ---------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `gtd.assign`     | `title`, `priority?`, `status?`, `assignee?`, `due?`, `depends_on?`, `tags?` | Create a task (defaults: status=inbox, priority=p2)                    |
| `gtd.next`       | `limit?`, `assignee?`                                                        | List actionable tasks (status ∈ next/active), priority-sort            |
| `gtd.complete`   | `id`, `status?` (`done` or `cancelled`), `result?`                           | Validate a terminal transition (default `done`), record `completed_at` |
| `gtd.tasks`      | `status?`, `assignee?`, `priority?`, `limit?`, `offset?`                     | Filtered task listing                                                  |
| `gtd.transition` | `id`, `status`, `note?`                                                      | Explicit lifecycle change with `can_transition` validation             |

### Memory pack verbs (5 — ADR-021, optional)

Load with `KHIVE_PACKS=kg,memory` or `--pack memory`. Adds the `memory` note kind.

| Verb              | Args                                                                                   | What it does                                                                                   |
| ----------------- | -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `memory.remember` | `content`, `salience?`, `decay_factor?`, `memory_type?`, `source_id?`                  | Create a memory note with salience + decay; optionally annotates a source                      |
| `memory.recall`   | `query`, `limit?`, `min_score?`, `min_salience?`, `memory_type?`, `tags?`, `tag_mode?` | Hybrid FTS + vector recall with RRF fusion, decay-weighted ranking                             |
| `memory.feedback` | `target_id`, `signal`                                                                  | Emit explicit feedback on a recalled entity; updates recall posteriors                         |
| `memory.prune`    | `min_salience?`, `before?`, `namespace?`, `dry_run?`                                   | Soft-delete memories below a salience floor and/or past `expires_at` (curation-layer, ADR-014) |
| `memory.vacuum`   | —                                                                                      | Run SQLite `VACUUM` to reclaim space freed by soft-deleted rows                                |

`get`/`update`/`delete`/`merge` are UUID-only — no `kind` needed, the handler resolves
the substrate from the UUID. `create`/`list`/`search` require `kind`.

Each op returns `{ok: true, tool, result}` or `{ok: false, tool, error}`. A failed op does
NOT abort the batch — each entry has its own ok/error. The aggregate response also carries
`summary: {total, succeeded, failed}`.

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

- **DDL lives in `.sql` files.** Schema DDL is authored in `crates/khive-db/sql/`
  and pulled into `migrations.rs` via `include_str!` — never hand-written as inline
  Rust string literals. `V1`'s body is `sql/schema.sql`.
- **Migrations only.** Schema changes go through `crates/khive-db/src/migrations.rs`.
  Add a new `VersionedMigration` (`version = <last + 1>`) pointing at a new
  `sql/NNN-<name>.sql` file. Never edit V1. (V1 is the consolidated fresh-start
  baseline — see [ADR-015](docs/adr/ADR-015-schema-migrations.md).)
- **Lint SQL.** `scripts/lint-sql.sh` loads every `crates/**/*.sql` into an
  in-memory SQLite db and checks hygiene; it runs in `make ci` and pre-commit. A
  malformed `.sql` fails before it ships.
- **Reusable query SQL** (hot/tuned queries) should likewise move to `.sql` files
  where it makes sense — lintable, `EXPLAIN`-able, and tunable without recompiling.

### MCP tool changes

- The MCP server exposes exactly one tool: `request` (ADR-016). There are no per-verb tool
  files — the only schema in `crates/khive-mcp/src/tools/` is `request.rs` (the `RequestParams`
  struct).
- DSL parsing lives in the `khive-request` crate (ADR-028). Edits to the parser go there,
  not in `khive-mcp`.
- MCP server (`crates/khive-mcp/src/server.rs`) is a thin dispatch shell — calls
  `khive_request::parse_request`, then routes each parsed op through the registry. No
  business logic.
- **Verb handler logic lives in the pack** (`crates/khive-pack-kg/src/handlers/`).
- Runtime methods live in `crates/khive-runtime/src/operations.rs` (or `curation.rs`,
  `retrieval.rs`, `graph_traversal.rs`).
- **Invalid DSL** (parse/lex failure) returns RPC-level `McpError::invalid_params` from
  `request`. **Per-verb validation failure** (unknown kind, bad UUID, etc.) returns a per-op
  `{ok: false, error: "..."}` entry — the batch does not abort.

### Namespace (attribution-only — ADR-007 Rev 6, 2026-06-19)

- **Namespace is attribution, not isolation.** It is a write-stamp on records, queryable and
  filterable, available to the Gate as policy input. It is never a storage boundary.
- **By-ID ops are namespace-agnostic** (get, update, delete, merge). No `record.namespace ==
  caller_namespace` check exists at the store, runtime, or handler layer. This was the
  prior v1 behavior; it was removed by PR-A1 (commit 2607e263) and is no longer correct.
  Do not add such checks.
- **Authorization lives at one seam: the Gate** (ADR-018). Storage stores are ID-only. The
  Gate is the trust boundary.
- **Multi-record ops default to `WHERE namespace='local'`**; the only escape is an explicit
  `namespace=` parameter from the caller.

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

# Build + verify Cargo's exact reported local artifact without installing or stopping the daemon
make fleet-build  # alias for the receipt-bound build + daemonless verbs() probe

# Re-run only the verification step (current build receipt, or a selected executable)
make fleet-check
make fleet-check FLEET_ARTIFACT="$HOME/.cargo/bin/kkernel"

# Build + install locally (ALWAYS use this after code changes)
make local      # depends on verify-local-artifact, then codesigns + installs kkernel → kills stale daemon
                # then /mcp in Claude Code to reconnect

# Publish to crates.io
make publish-dry  # dry run — validates all workspace crates
make publish      # live publish in dependency order
```

### Commits

Feature branches + PRs. Never push directly to main.

Conventional commits with crate scope: `feat(query): add SPARQL property filter`.

### Testing

- **Verify before claiming complete.** Run the test, check the output.
- **Report outcomes faithfully.** If tests fail, say so.
- **Integration > unit** for the MCP surface — the value is in the composition.
- **Smoke test**: `python3 tests/smoke_test.py` drives every verb through the `request` tool
  end-to-end (KG verbs in the default run; GTD verbs in the post-pass when both packs are
  loaded). Asserts `tools/list` returns only `request` and that its description carries the
  full verb catalog.

---

## ADR-driven development

Architecture Decision Records (`docs/adr/`) are the normative contract. Code implements what
ADRs specify. Changing the schema or interface requires an ADR **before** code lands.

Key ADRs for contributors:

| ADR                                                  | What it governs                                                     |
| ---------------------------------------------------- | ------------------------------------------------------------------- |
| [001](docs/adr/ADR-001-entity-kind-taxonomy.md)      | 9 entity kinds (8 base + resource ADR-048) — don't add without this |
| [002](docs/adr/ADR-002-edge-ontology.md)             | 15 base edge relations; +2 epistemic via ADR-055 = 17 total         |
| [005](docs/adr/ADR-005-storage-capability-traits.md) | Storage traits — the abstraction boundary                           |
| [008](docs/adr/ADR-008-query-layer-separation.md)    | Query crate — parser/validator/compiler separation                  |
| [013](docs/adr/ADR-013-note-kind-taxonomy.md)        | 5 base note kinds                                                   |
| [015](docs/adr/ADR-015-schema-migrations.md)         | Migration system — how to change the DB schema                      |
| [016](docs/adr/ADR-016-request-dsl.md)               | Request DSL — verb-dispatch syntax for `request`                    |
| [017](docs/adr/ADR-017-pack-standard.md)             | Pack trait, `EDGE_RULES`, pack-extensible endpoints                 |
| [023](docs/adr/ADR-023-declarative-pack-format.md)   | Pack verb surface, visibility, and composition                      |
| [027](docs/adr/ADR-027-dynamic-pack-loading.md)      | Dynamic pack loading via self-registration                          |
| [028](docs/adr/ADR-028-pack-scoped-backends.md)      | Pack-scoped backends and per-pack schema declaration                |

Full index: [docs/adr/README.md](docs/adr/README.md).

---

## What lives where

| Want to do...                                               | Edit this                                                                                                                           |
| ----------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Add a new verb                                              | Pack handler in `crates/khive-pack-kg/src/handlers/` (or your pack); the MCP surface is `request` — no per-verb tool file to author |
| Change DSL syntax                                           | `crates/khive-request/src/lib.rs` + unit tests (ADR-016)                                                                            |
| Change MCP surface shape                                    | `crates/khive-mcp/src/server.rs` (ADR-016 — `request` is the only tool)                                                             |
| Add a runtime operation                                     | `crates/khive-runtime/src/operations.rs`                                                                                            |
| Change DB schema                                            | New `crates/khive-db/sql/NNN-<name>.sql` file + register it as a new `VersionedMigration` in `crates/khive-db/src/migrations.rs`    |
| Add a new entity kind                                       | `crates/khive-pack-kg/src/vocab.rs` + ADR-001 amendment                                                                             |
| Add a new edge relation                                     | **STOP** — ADR change ([ADR-002](docs/adr/ADR-002-edge-ontology.md))                                                                |
| Allow a new edge endpoint pair (e.g. note-kind→entity-kind) | Pack's `EDGE_RULES` const ([ADR-017](docs/adr/ADR-017-pack-standard.md) §"Pack-extensible edge endpoints"); additive only           |
| Add a new note kind                                         | `crates/khive-pack-kg/src/vocab.rs` + ADR-013 amendment                                                                             |
| Add a new pack                                              | New crate implementing `Pack` + `PackRuntime` ([ADR-017](docs/adr/ADR-017-pack-standard.md))                                        |
| Fix a query parser bug                                      | `crates/khive-query/src/parsers/` + add regression test                                                                             |
| Fix a storage bug                                           | `crates/khive-db/src/stores/` + test                                                                                                |

---

## Anti-patterns

- **Don't add a language SDK.** MCP is the universal interface. No Python/TS/Go client library.
- **Don't reimplement KG primitives outside `crates/`.** If you need entity CRUD in another
  language, call the MCP server — don't rewrite the storage logic.
- **Don't silently coerce invalid input.** Invalid entity kinds, note kinds, and edge relations
  must return errors with the valid values listed. Never `unwrap_or_default()`.
- **Don't add namespace checks to by-ID ops.** By-ID methods (get, update, delete, merge)
  are namespace-agnostic by design (ADR-007 Rev 6). Authorization is at the Gate. Adding
  `record.namespace == caller_namespace` post-fetch checks is the prior v1 bug pattern; it
  was removed by PR-A1 and must not return.
- **Don't edit V1 migrations.** Append a new version. V1 is immutable on existing databases.
- **Don't store research findings only as notes.** Notes are for context; entities + edges
  are for structure. If a concept is worth naming, it's an entity.
- **Don't optimize before measuring.** The bottleneck is rarely where you think.

---

## Cross-references

- **[lattice-embed](https://crates.io/crates/lattice-embed)**: Local embedding model inference.
  Consumed as a Rust dependency by `khive-runtime` (ADR-012).
- **[AGENTS.md](AGENTS.md)**: Guide for AI agents _using_ khive (not developing it).
