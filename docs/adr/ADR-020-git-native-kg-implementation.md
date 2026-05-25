# ADR-020: Git-Native KG Implementation

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive

## Context

[ADR-010](ADR-010-kg-versioning.md) adopted git as the versioning layer for khive KGs and
delegated the implementation contract to "future companion ADRs." This is that companion.

The strategic position is fixed: KG state lives as sorted NDJSON files in a git repository,
git provides commits/branches/merges/remotes/access control, and GitHub provides the social
layer (PRs, review, CI). What ADR-010 does not specify is the concrete file format, the
storage model that couples a queryable database to the committed files, the CLI workflow,
the conflict resolution surface, the schema evolution mechanism, and the cross-repo
reference protocol.

Each of these is small enough to specify on its own. They are bundled into one ADR because
they share invariants: the sort key in the NDJSON file determines whether git auto-merges
non-overlapping additions; the schema manifest determines what `khive kg validate`
accepts; the CLI workflow assumes the storage model; the schema evolution mechanism writes
files in the layout defined here. Splitting them would force a reader to chase
cross-references for invariants that are inseparable in practice.

### Scope

This ADR is the full implementation contract for KG versioning. It covers:

- Directory layout under `.khive/`
- NDJSON serialization format (entity and edge records)
- `schema.yaml` ontology manifest
- Two-layer storage model (working DB vs. committed NDJSON)
- CLI verb surface for KG git workflows
- Reconciliation protocol (DB ↔ NDJSON)
- Cross-repo references and pin resolution
- Schema evolution via declarative migrations
- Branching and merge semantics (delegating to git)

### Binary topology — `khive` vs `kkernel`

The user-facing product CLI is **`khive`** (Deno, distributed via npm). The Rust admin
binary is **`kkernel`** (per-platform npm subpackages, per ADR-026). Throughout this ADR
all `khive kg <verb>` commands are product workflows that internally invoke `kkernel`
primitives (`kkernel sync`, `kkernel export`, `kkernel import`, `kkernel validate`,
`kkernel db migrate`, etc.). The split:

- **`khive`** — git workflows, file scaffolding, hook installation, user CLI ergonomics,
  network pack install, and (future) hosted product features.
- **`kkernel`** — storage, validation primitives, pack registry, coordinator, MCP server,
  schema migrations. Pure Rust; no Deno or product UX.

LLM-agent clients connect directly to `kkernel mcp`. Humans use `khive`. See
[ADR-003](ADR-003-system-architecture.md) for the architectural split and
[ADR-026](ADR-026-rust-binary-packaging.md) for distribution.

- Git hooks for automatic sync
- CI integration

It does NOT cover:

- Federation across multiple backends ([ADR-010](ADR-010-kg-versioning.md) defers to a future
  federated-snapshot manifest ADR)
- Notes versioning (KG v1 covers entities + edges only — [ADR-010](ADR-010-kg-versioning.md))
- Frozen canonical-JSON spec with golden vectors (future canonicalization ADR)

### What ADR-010 retains, this ADR replaces

- `KgArchive` in-memory representation: **retained** as the import/export type
- Content-hash algorithm (SHA-256 of canonical JSON): **retained** for snapshot identity
- Conflict taxonomy: **retained** as input to `khive kg resolve`
- Custom `khive-vcs` command set: **replaced** by git CLI + this ADR's `khive kg` verbs
- Custom `khive-sync` HTTP server: **replaced** by git push/pull
- Custom merge engine: **replaced** by git's three-way line merge + entity-aware resolve
- Snapshot SQL tables (`kg_snapshots`, `kg_branches`, `kg_vcs_state`): **deleted**

## Decision

### 1. Directory layout

```
.khive/
├── .gitignore          # allowlist: only kg/ and khive.toml are tracked
├── khive.toml         # project config — git-tracked
└── kg/                 # committed knowledge graph — git-tracked
    ├── schema.yaml         # ontology manifest
    ├── entities.ndjson     # sorted by UUID
    ├── edges.ndjson        # sorted by (source, target, relation)
    └── migrations/         # schema migration sequence
        └── .gitkeep
.khive/state/           # gitignored (covered by allowlist) — ephemeral working state
└── working.db          # SQLite with FTS5 + sqlite-vec, rebuilt by `khive kg sync`
```

`.khive/.gitignore` uses an **allowlist** pattern so that any new working-state directory
introduced later is gitignored by default:

```gitignore
*
!.gitignore
!kg/
!kg/**
!khive.toml
```

`khive kg init` creates the layout. `working.db` is fully reconstructable from the
NDJSON files via `khive kg sync` and is never committed.

**One KG per repository.** There is no `.khive/kg/<name>/` namespace multiplexer. Multiple
independent KGs live in separate repositories, with cross-repo edges (§7) handling the
multi-project case. The single-KG-per-repo constraint removes a `--kg` qualifier from
every verb and is a deliberate simplicity choice, not a limitation.

### 2. NDJSON format

One self-contained JSON record per line, UTF-8, with a trailing `\n` after every record
(including the last). Empty files (zero records, zero bytes) are valid and represent an
empty graph.

**Sort invariant** (the design choice that makes git auto-merge work):

- `entities.ndjson` — sorted by entity UUID, case-insensitive ascending
- `edges.ndjson` — sorted by `(source, target, relation)` triple, lexicographic ascending

Sorting ensures:

1. New entity additions land at deterministic non-overlapping positions in the file. Two
   agents on different branches adding different entities produce a clean three-way git
   merge with no conflict.
2. Re-exporting the same logical graph state produces bit-identical files. Diffs between
   exports are meaningful; equal logical states have equal byte representations.
3. Cross-repo edge groupings are readable — all edges from entity A appear contiguously,
   then all edges from B, etc.

**Entity record shape** (fixed top-level key order: `id`, `kind`, `entity_type`, `name`,
`description`, `properties`, `tags`, `created_at`, `updated_at`. JSON keys sorted
alphabetically within `properties`; `tags` sorted lexicographically):

```json
{"id":"<uuid>","kind":"<EntityKind>","entity_type":"<string|omit>","name":"<string>","description":"<string|null>","properties":{...},"tags":["..."],"created_at":"<ISO8601|omit>","updated_at":"<ISO8601|omit>"}
```

`entity_type` is the governed subtype field per [ADR-001](ADR-001-entity-kind-taxonomy.md).
It is omitted when the entity has no registered subtype, never serialized as `null`. On
read, both an absent field and `"entity_type": null` deserialize to `None`; on write, the
canonical writer always omits.

`created_at` and `updated_at` are optional — they appear when the database has them and
are omitted when absent. This preserves round-trip identity for NDJSON files exported by
older khive versions that did not record timestamps.

Soft-deleted entities are excluded from export. NDJSON represents live graph state.

**Edge record shape**:

```json
{"edge_id":"<uuid>","source":"<uuid>","target":"<uuid|remote_ref>","relation":"<EdgeRelation>","weight":<float>,"properties":{...},"created_at":"<ISO8601|omit>","updated_at":"<ISO8601|omit>"}
```

`edge_id` carries edge identity across export/import cycles. The composite
`(source, target, relation)` is the sort key and the semantic identity; `edge_id` is the
opaque stable handle for `update`/`delete`. `target` may be a local UUID or a
`kg://<remote>/<namespace>/<id>` cross-repo reference (§8, per
[ADR-037](ADR-037-remote-resolution-and-hash-verification.md)).

Field ordering within both record types is fixed at the serializer level. Re-exporting
the same logical state always produces the same bytes — a property the SHA-256 snapshot
hash from [ADR-010](ADR-010-kg-versioning.md) depends on.

### 3. `schema.yaml` manifest

The single ontology declaration for the KG. Two version fields, distinct purposes:

| Field              | Semantics                                                                                                                                                             |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `format_version`   | File-format compatibility — bumped when `schema.yaml`'s structure gains new top-level keys. Parsers reject files whose major `format_version` they do not understand. |
| `ontology_version` | Schema evolution — bumped when entity kinds, edge relations, properties, or pack vocabulary changes. Migrations (§9) gate major bumps.                                |

Both use semver. A third informational field `khive_version` records the CLI version that
last wrote the file.

```yaml
format_version: "2.0.0"
ontology_version: "1.0.0"
khive_version: "0.1.0"

entity_kinds:
  - concept
  - document
  - dataset
  - project
  - person
  - org
  - artifact
  - service

entity_types: # governed subtypes per (kind), per ADR-001
  concept:
    - algorithm
    - technique
    - architecture
    - model_family
    - theory
    - research_gap
    - design_pattern
    - metric
  document:
    - paper
    - report
    - blog_post
    - book
    - specification
    - documentation
    - thesis
  dataset:
    - benchmark
    - corpus
    - training_set
    - evaluation_set
  project:
    - library
    - framework
    - tool
    - application
  org:
    - academic_institution
    - company
    - research_lab
  artifact:
    - checkpoint
    - snapshot
    - profile
  service:
    - inference_engine
    - retrieval_engine
    - embedding_engine
    - api

edge_relations:
  - relation: contains
    category: structure
    endpoints:
      - [concept, concept]
      - [project, concept]
  # ... one entry per canonical relation from ADR-002

properties: # additional property schemas beyond entity_type
  concept:
    - key: domain
    - key: status
      values: [concept, researched, prototyped, implemented, shipped, deprecated]

packs:
  - name: kg
    version: "1.0.0"
    source: builtin
  - name: gtd
    version: "1.0.0"
    source: builtin

remotes:
  - name: lattice
    repo: ohdearquant/lattice
    path: .khive/kg
    commit: a1b2c3d4e5f6789012345678901234567890abcd
```

`entity_kinds`, `entity_types`, `edge_relations`, and `properties` are the **merged**
vocabulary — written by `khive pack install` / `khive pack remove` so the file is
self-contained for offline validation. `packs` is the source of truth for which
vocabularies are active; the merged sections are a committed cache (see
[ADR-023](ADR-023-declarative-pack-format.md)). `entity_kinds` is the closed 8-element
set from ADR-001; only `entity_types` is pack-extensible.

The `packs` section is the only structural addition beyond ADR-010's baseline. Other
top-level keys are stable across the `1.x.x` format range.

### 4. Two-layer storage model

The KG has two layers, each authoritative at a different phase:

| Phase                         | Authoritative layer                | Operation                               |
| ----------------------------- | ---------------------------------- | --------------------------------------- |
| Active work (between commits) | `working.db`                       | `create` / `link` / `update` write here |
| Committed state               | `entities.ndjson` + `edges.ndjson` | What git tracks, diffs, merges          |
| Committing                    | DB wins                            | `commit` exports DB → files             |
| Checkout / pull               | Files win                          | Atomic rebuild of DB from files         |

Transitions are always one-directional. There is no bidirectional sync — that problem
collapses into CRDT or operational-transformation complexity which [ADR-010](ADR-010-kg-versioning.md)
explicitly rejects.

The DB is git's working tree: where edits happen. The NDJSON files are the committed
snapshot: what git tracks and merges. The `.khive/state/` directory is the internal
bookkeeping that makes transitions efficient.

**Working DB schema** mirrors the NDJSON fields. Two tables (`entities`, `edges`), each
with one FTS5 virtual table and the indexes from [ADR-005](ADR-005-storage-capability-traits.md).
Timestamps are nullable to match the optional NDJSON fields — `NULL` round-trips to an
omitted field on export.

The composite primary key on `edges` is `(source, target, relation)`, matching the NDJSON
sort key.

**Mode detection.** The KG verbs (`create`, `link`, `search`, `traverse`, `query`) walk
from `$CWD` up to the filesystem root looking for a `.khive/state/working.db`. If found,
git-native mode is active and writes go there. If not found, the verbs use the main
khive database (`~/.khive/khive.db`) in standalone mode. This is the same heuristic git
uses to locate `.git/`.

### 5. CLI verb surface

The `khive kg` subcommand tree has eleven verbs. Each one adds KG-specific intelligence
that raw git cannot provide; everything else (push, pull, branch, checkout, log, stash)
uses git directly.

| Verb                                                                                  | Purpose                                                                                             |
| ------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| `khive kg init`                                                                       | Create `.khive/kg/`, default `schema.yaml`, empty NDJSON, allowlist `.gitignore`, install git hooks |
| `khive kg export`                                                                     | DB → NDJSON files (sorted, deterministic)                                                           |
| `khive kg import [--on-conflict error\|skip\|update] [--force]`                       | NDJSON → DB (atomic, validates first)                                                               |
| `khive kg validate [--resolve-remotes] [--schema-compat <ref>]`                       | Schema compliance + referential integrity + sort check                                              |
| `khive kg commit -m <msg>`                                                            | export + validate + git add + git commit                                                            |
| `khive kg sync [--quiet]`                                                             | Atomic rebuild of `working.db` from NDJSON                                                          |
| `khive kg status`                                                                     | DB-vs-NDJSON entity-level diff                                                                      |
| `khive kg diff [<ref>]`                                                               | Entity-aware diff renderer over `git diff`                                                          |
| `khive kg resolve [--ours\|--theirs\|--merge-properties] [--entity <id>\|--edge ...]` | Entity/edge-level merge conflict resolution                                                         |
| `khive kg update <remote> [--ref <ref>]`                                              | Resolve remote ref to commit SHA, bump `schema.yaml#remotes`                                        |
| `khive kg migrate [--dry-run] [--to <version>]`                                       | Apply schema migrations from `.khive/kg/migrations/`                                                |

The eleven verbs are the **complete** KG-specific surface. There is no `khive kg push`,
`khive kg pull`, `khive kg branch`, or `khive kg checkout` — those are standard
git commands, run directly, with git hooks (§10) ensuring `working.db` rebuilds
automatically when NDJSON files change.

### 6. Reconciliation protocol

#### Commit flow: DB → files

1. Run §7 status diff. If empty: report "nothing to commit" and exit.
2. `SELECT ... FROM entities ORDER BY id ASC`. Serialize with fixed key order. Write
   `entities.ndjson`.
3. `SELECT ... FROM edges ORDER BY source, target, relation`. Serialize. Write
   `edges.ndjson`.
4. `khive kg validate` against `schema.yaml`. Abort on validation failure (no git
   operations performed).
5. `git add .khive/kg/{entities,edges,schema}.{ndjson,yaml}`.
6. `git commit -m <message>`.
7. Print branch, short SHA, entity/edge counts, and DB-vs-NDJSON diff summary.

Export is a full snapshot. There is no delta or incremental export — for typical KG
sizes, full export is fast (sub-second for 10K entities), and the NDJSON files compose
with git's own delta encoding in the object store so unchanged lines produce zero delta
on disk.

#### Checkout / pull flow: files → DB (atomic)

1. Run §7 status diff. If non-empty: refuse with "Uncommitted changes in working.db. Run
   `khive kg commit` or `khive kg reset` first."
2. **Validate first**: run `khive kg validate` against the NDJSON. Abort on failure
   before touching the database.
3. Create `.khive/state/working.db.tmp` with the §4 schema.
4. Open a single write transaction. Parse NDJSON line by line, INSERT into entities/edges.
5. Rebuild FTS5 (`INSERT INTO entities_fts(entities_fts) VALUES('rebuild')`).
6. Commit the transaction.
7. **Atomically rename** `working.db.tmp` → `working.db`. This is the only instant of
   transition: either the old DB survives intact (on any failure before step 7) or the
   new DB is fully populated.
8. Write current git branch name to `.khive/state/HEAD`.

The rebuild is idempotent. Running checkout twice on the same files produces the same DB.
This invariant makes the DB a true materialized view of the NDJSON.

#### Reset flow

`khive kg reset` runs the checkout flow against the committed NDJSON, discarding
uncommitted DB changes. Equivalent to "I want to throw away my working tree edits."

### 7. Status and diff

`khive kg status` computes an entity-level diff between the current DB state and the
committed NDJSON files. **There is no dirty flag.** The diff is computed fresh on every
invocation.

Algorithm:

1. Export current DB to an in-memory sorted NDJSON representation.
2. Parse committed `entities.ndjson` and `edges.ndjson` into UUID-keyed and
   composite-keyed maps.
3. Compute set differences:
   - UUIDs in DB but not in files → `+` (new)
   - UUIDs in files but not in DB → `-` (deleted)
   - UUIDs in both with different serialized JSON → `~` (modified)
4. Render summary.

The full-comparison cost is bounded: sub-second for KGs up to ~100K entities. At larger
scale, a row-level change log table in `working.db` becomes an optimization path —
deferred until the scale warrants it.

This approach catches DB changes that `git status` cannot see (the DB is gitignored),
remains correct across crashes and concurrent writes, and avoids the synchronization
problems of an explicit dirty flag.

`khive kg diff` renders unified git diff output in entity-aware terms — parsing the
JSON on each side, showing field-level changes:

```
~ entity 671b882a (concept "LoRA")
    properties.status: "researched" → "implemented"

+ entity a3f2c1d4 (concept "QLoRA")
    name: QLoRA
    kind: concept
    entity_type: technique

- edge 671b882a --[competes_with]--> c9e4b3f2 (QLoRA)
```

Underneath, the diff is computed by `git diff`; the kg layer is a presentation pass.

### 8. Cross-repo references

Edges may reference entities in remote repositories using the `kg://` scheme
([ADR-037](ADR-037-remote-resolution-and-hash-verification.md)) in the `target` field:

```json
{
  "edge_id": "...",
  "source": "671b882a-...",
  "target": "kg://lattice/default/c9e4b3f2-...",
  "relation": "implements",
  "weight": 1.0,
  "properties": {}
}
```

`lattice` resolves through `schema.yaml#remotes`:

```yaml
remotes:
  - name: lattice
    repo: ohdearquant/lattice
    path: .khive/kg
    commit: a1b2c3d4e5f6789012345678901234567890abcd
```

**Commit SHA pins are mandatory.** The `commit` field must be a full 40-character git
SHA. Tags and branch names are mutable and break reproducibility — a commit SHA is the
only content-addressed pin. `khive kg update <remote> --ref v0.3.0` resolves a tag to
its commit SHA at update time and stores the SHA, not the tag.

`khive kg validate --resolve-remotes` fetches the pinned remote NDJSON and confirms
every `kg://<remote>/<namespace>/<id>` reference resolves. Resolution uses two strategies in order:

1. **Sparse git checkout**: `git archive --remote=<repo-url> <sha> .khive/kg/entities.ndjson | tar -x`.
2. **GitHub Contents API** fallback: `GET /repos/<owner>/<repo>/contents/.khive/kg/entities.ndjson?ref=<sha>`.

Results cache at `.khive/kg/.remote-cache/<remote>-<sha>.ndjson`. The cache is keyed by
`(remote, sha)` — because SHAs are immutable, cache entries never expire. The cache
directory is gitignored by the allowlist.

**Merge never fetches remotes.** Cross-repo edge targets are not validated during merge;
they are validated by `validate --resolve-remotes` (the mode CI runs). Requiring network
access during local merge would make offline branching impossible.

### 9. Schema evolution

`ontology_version` is the entity-level compatibility version. Three change classes map
to semver:

| Change         | Bump  | Examples                                                                               |
| -------------- | ----- | -------------------------------------------------------------------------------------- |
| Breaking       | Major | Remove entity kind, remove edge relation, change required property type, rename a kind |
| Additive       | Minor | Add entity kind, add optional property, add pack, relax endpoint rule                  |
| Non-functional | Patch | Description updates, comment additions, remote SHA bumps                               |

Breaking changes require a migration file before `khive kg validate` accepts the new
schema against the existing NDJSON corpus. Additive changes do not. Patch bumps are
transparent.

**Migrations** live in `.khive/kg/migrations/` as YAML files ordered by filename prefix
(`0001_*.yaml`, `0002_*.yaml`, ...). Gaps in the sequence are errors. Migrations are
git-tracked.

```yaml
version_from: "1.1.0"
version_to: "2.0.0"
description: "Rename training_run to run; remove legacy experiment kind"
operations:
  - rename_kind:
      from: training_run
      to: run
  - remove_kind:
      name: experiment
      on_existing: error # "error" | "migrate_to" + target kind
```

Supported operations:

| Operation                            | Effect                                                                            |
| ------------------------------------ | --------------------------------------------------------------------------------- |
| `add_kind` / `add_relation_endpoint` | No NDJSON rewrite; new kind/endpoint becomes accepted                             |
| `remove_kind`                        | `on_existing: error` aborts if entities exist; `migrate_to: <kind>` re-kinds them |
| `rename_kind` / `rename_property`    | Rewrite matching lines in `entities.ndjson`                                       |
| `add_property`                       | None if optional; if required, aborts unless all entities have it                 |
| `remove_property`                    | Schema-only; existing values retained but no longer validated                     |
| `change_property_type`               | `coerce: true` rewrites values; `coerce: false` aborts on existing values         |
| `remove_relation_endpoint`           | `on_existing: drop` removes matching edges; `error` aborts                        |

All operations within a migration are atomic — partial failure leaves no changes and the
`ontology_version` unchanged.

**Migrations are never auto-applied.** `khive kg migrate` is always explicit. Silent
data rewrites on `git checkout` would violate the principle that every data change is a
deliberate commit.

**Schema diff** (`khive kg schema diff [<ref>]`) renders ontology-level changes between
two `schema.yaml` versions — added/removed kinds, property changes, endpoint changes,
pack changes — as a presentation layer over `git diff schema.yaml`.

**Compatibility on pull/merge:**

- Patch or minor difference: git auto-merges (additive changes don't conflict). If both
  branches independently bumped the minor version, `khive kg schema merge-resolve`
  unions both contributions and computes a new minor version.
- Major version difference: merge is refused. The user must apply migrations on the
  current branch first to reach the same major version before the merge proceeds.
- Diverged in incompatible ways: `validate --schema-compat <branch>` reports the
  specific conflicts. Manual resolution required.

### 10. Branching and merge

KG branches ARE git branches. There is no separate KG branch metadata, no `kg_branches`
table, no custom ref store. The `.khive/kg/` files travel with the git branch that
contains them.

```bash
git checkout -b experiments    # standard git
git checkout main              # standard git (post-checkout hook runs `khive kg sync`)
git merge experiments          # standard git (post-merge hook runs `khive kg sync`)
git push origin main           # standard git
git pull                       # standard git (post-merge hook runs `khive kg sync`)
```

The only KG-specific operation in branching is `khive kg resolve`, invoked when `git
merge` produces NDJSON conflicts.

**Sorted NDJSON gives most merges for free.** Because each entity is one line at a
deterministic UUID-sorted position, git's three-way line merge handles the common cases
without intervention:

| Scenario                                          | Git result                                                   |
| ------------------------------------------------- | ------------------------------------------------------------ |
| Two branches add different entities               | Clean — different lines at different positions               |
| Two branches add different edges                  | Clean — different lines at different composite-key positions |
| Two branches edit different entities              | Clean — different lines modified                             |
| Two branches edit the same entity                 | Conflict                                                     |
| Delete vs. edit on same entity                    | Conflict                                                     |
| Two branches add same UUID with different content | Conflict                                                     |

Most KG work is additive; the conflict-free cases dominate.

**`khive kg resolve`** handles the residual cases. It parses NDJSON conflict markers,
renders field-level diffs, and applies a resolution strategy:

- `--ours` / `--theirs`: keep one side wholesale for all conflicts
- `--merge-properties`: merge non-overlapping property changes from both sides; for
  overlapping property keys, current branch wins with a warning. Recommended for
  agent-driven merges where both branches extended the same entity's properties in
  different directions.
- `--entity <id>` / `--edge <s> <t> <r>`: per-record override against the global strategy

After resolution, NDJSON files are re-sorted (resolution may have left order intact, but
explicit sort guarantees the invariant), then `khive kg validate` runs to catch any
referential integrity violations introduced by the merge (dangling edges, duplicate
UUIDs).

**Schema conflicts** are categorized:

| Scenario                                                   | Strategy                                |
| ---------------------------------------------------------- | --------------------------------------- |
| Two branches add different entity kinds                    | Additive — auto-merge                   |
| Two branches add the same entity kind with different rules | Manual resolution                       |
| Two branches add base edge relation changes (ADR-002)      | Always manual — base ontology is closed |
| Two branches add pack-scoped endpoint rules                | Additive — auto-merge                   |
| Two branches change the same property schema               | Manual resolution                       |
| Two branches bump the same remote pin to different SHAs    | Manual resolution                       |

`khive kg resolve` detects schema conflicts and refuses to merge them automatically
when no additive strategy applies.

### 11. Git hooks

`khive kg init` installs three git hooks in `.git/hooks/`:

```bash
# post-checkout, post-merge, post-rewrite
#!/bin/sh
khive kg sync --quiet 2>/dev/null || true
```

The hooks rebuild `working.db` from NDJSON automatically after any git operation that
changes the files. They work with every git interface — CLI, IDE git clients, GUI tools
— without requiring the user to remember a wrapper command.

Hooks are not committed to the repo (`.git/hooks/` is outside the working tree). Each
clone runs `khive kg sync` once on first use to bootstrap `working.db`, then the hooks
take over.

`khive kg init` does not overwrite existing hooks. If a hook file is present, it prints
instructions for the user to append `khive kg sync --quiet` to their existing hook.

The `|| true` ensures that a sync failure (e.g., NDJSON has unresolved merge conflicts)
does not block the git operation. The user runs `khive kg resolve`, then `khive kg
sync` finishes.

### 12. CI integration

`khive kg init` generates `.github/workflows/kg-validate.yml`:

```yaml
name: KG Validate
on:
  push:
    paths: [".khive/kg/**"]
  pull_request:
    paths: [".khive/kg/**"]

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: npx khive kg validate --resolve-remotes
      - run: npx khive kg migrate --dry-run
      - run: npx khive kg schema diff HEAD~1
```

The workflow catches: malformed NDJSON, unknown kinds/relations, dangling edges,
unresolvable cross-repo pins, duplicate UUIDs, out-of-sort lines, missing migrations for
major version bumps. PRs that touch `.khive/kg/**` cannot merge until CI passes.

### 13. Bulk import

Bulk import is a **CLI-level operation**, not an MCP verb. Large file-based batch
operations should not pass through the agent MCP surface. `khive kg import` reads
NDJSON files (or a `KgArchive` JSON envelope) and loads them into the local namespace,
with full validation and atomicity:

```bash
khive kg import < paper-list.ndjson --on-conflict skip
khive kg import archive.json --on-conflict update --dry-run
```

The payload contract: `{records: [...], edges: [...]}` or a full `KgArchive` envelope.
`records` accept either local refs (resolved within the import) or UUIDs (matched against
the namespace). Every edge is validated against the pack-extensible endpoint contract
([ADR-002](ADR-002-edge-ontology.md), [ADR-017](ADR-017-pack-standard.md)) — the same
validation `link` uses. The entire import is one transaction: a single bad edge fails
the whole batch.

`on_conflict`:

- `error` (default): fail on UUID collision
- `skip`: omit colliding records and their incident edges
- `update`: patch existing records with new content (`EntityPatch` semantics from
  [ADR-014](ADR-014-curation-operations.md))

`--dry-run` returns `{would_insert: {entities: N, edges: M}, errors: [...]}` without
writing.

Cap: 10,000 records per call (configurable). Larger imports must be split into batches.

External adapters (arxiv, BibTeX, Zotero, DOI) live in separate crates or out-of-tree
tools. They transform source data into the `{records, edges}` envelope and pipe it to
`khive kg import`. None of those adapters are dependencies of khive core.

## Rationale

### Why one big ADR (vs. five small ones)

These five topics — file format, storage model, CLI workflow, branching, schema evolution
— share invariants that cannot be reasoned about in isolation. The sort key in the
NDJSON file determines whether git auto-merges; the storage model assumes the file
format; the CLI workflow assumes the storage model; schema evolution writes files in the
defined layout; branching depends on the storage model's atomic rebuild guarantee. A
reader chasing one of these decisions needs to see the others in the same document.

Splitting into five ADRs would not save reading time — it would require five ADRs to be
read together to understand any one. One ADR with clear sections is the better shape.

### Why NDJSON sorted by primary key

The choice is the design crux. Sorted NDJSON is the unique format that satisfies four
constraints simultaneously:

1. **Line-addressable**: git's three-way line merge can combine non-overlapping additions
   from two branches automatically. JSON-blob or binary formats cannot.
2. **Human-readable in PRs**: diffs in the GitHub UI show entity-level changes, not
   binary blob churn.
3. **Deterministic**: re-export of the same logical state produces identical bytes,
   enabling SHA-256 snapshot identity.
4. **Streaming-capable**: line-per-record means a 100K-entity KG can be processed in
   chunks without loading the whole file.

Alternative formats fail on at least one axis. JSON blob loses (1) and (4). RDF/Turtle
loses (2) in the GitHub UI. Binary formats (SQLite, Parquet) lose (1), (2), (4). CSV
loses property structure.

### Why working DB + committed NDJSON (vs. NDJSON-only)

NDJSON is not searchable. FTS5 and sqlite-vec are the index structures that make
`search`, `traverse`, and `query` fast. Parsing 10K-entity NDJSON on every search call
would be unusable. The DB is mandatory for the verb surface.

But the DB cannot be the committed state — SQLite files are binary blobs that git cannot
diff or merge. The two-layer model is the resolution: DB for the verb surface, NDJSON
for git, and a deterministic export/import contract between them.

### Why DB-vs-NDJSON diff (vs. a dirty flag)

A dirty flag would be cheaper on every status call but adds three problems:

- It must be written transactionally with every entity write, adding write overhead.
- It is wrong if a write transaction commits but the flag update fails (rare but possible).
- It cannot answer "what changed?" — only "are there any changes?".

The diff approach is sub-second for KGs up to ~100K entities, catches all DB changes
(including ones a flag would miss after a crash), and produces the entity-level summary
the user actually wants from `khive kg status`. The cost is recomputation; the benefit
is correctness and richer output. For larger KGs, a row-level change log is an
optimization path that doesn't change the contract.

### Why commit SHA pins (vs. tags or branch names)

Tags in git are mutable (`git tag -f`). Branch names are mutable by definition. Only
commit SHAs are content-addressed and immutable. A `schema.yaml` that pins by tag or
branch name loses reproducibility — checking out an old project commit cannot guarantee
the remote KG it referenced is in the same state it was when the edge was recorded.

Resolving tags/branches at `khive kg update` time and storing the resolved SHA gives
the user the convenience of "track the latest release" without sacrificing reproducibility.

### Why KG branches = git branches (vs. custom branch metadata)

Every git tool — CLI, IDE clients, GitHub Desktop, GitHub PR review, CI runners — already
understands git branches. A custom KG branch concept that lives in a SQLite table would
not interoperate with any of them. The cost of keeping custom branch state in sync with
git state is high; the benefit is zero.

The NDJSON files travel with the git branch that contains them, exactly like source code.
The only KG-specific operation is `khive kg sync` (rebuild DB from NDJSON) and
`khive kg resolve` (entity-aware conflict resolution). Both are tightly scoped.

### Why explicit migrations (vs. auto-apply on pull)

Auto-applying migrations on `git pull` would mean opening a branch causes data rewrites
without a commit. This violates the principle that every change is a deliberate commit
with a clear message — and obscures what actually happened in the git log.

Explicit `khive kg migrate` preserves auditability. The user runs the command, sees
what changed, and commits. The git log records when each migration applied. The cost is
one extra command after pulling a branch with new migrations; the benefit is no surprise
rewrites.

### Why one KG per repo (vs. namespace multiplexer)

Every verb that touches KG state (`create`, `link`, `search`, `traverse`) would otherwise
need a `--kg` qualifier. The verb surface becomes ambiguous by default, the CLI grows a
session-config to disambiguate, and the implementation gains complexity for a case that
is better served by separate repos with cross-repo edges.

Cross-repo edges (§7) already handle multi-project scenarios. The one-KG-per-repo
constraint is a deliberate simplicity choice that removes friction from the common case.

## Alternatives Considered

| Alternative                                                       | Why rejected                                                                                                                                                   |
| ----------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Custom VCS layer (snapshots in SQLite, custom HTTP push/pull)     | The original ADR-042 design. Duplicates git/GitHub functionality; closed ecosystem; foreign UX for researchers; months of engineering for sub-parity with git. |
| Five separate ADRs (format, storage, CLI, branching, schema)      | Shared invariants cannot be reasoned about in isolation; readers would need all five to understand any one.                                                    |
| JSON blob (one file for the whole graph)                          | Every entity change diffs as a full-file rewrite; merge always conflicts; same problem as SQLite-in-git.                                                       |
| RDF/Turtle serialization                                          | Line-addressable but non-standard tooling; complex parsers; harder GitHub PR review.                                                                           |
| Dolt or TerminusDB (versioned databases)                          | Adds a runtime dependency; binary storage means no GitHub PR diffs without their dashboards; not git-native.                                                   |
| Pull = fetch + auto-merge (git's default)                         | Hides merge decisions; wrong for research KG correctness. Pull-as-fetch + explicit merge keeps the agent in the loop.                                          |
| Per-branch DB files (`.khive/state/<branch>.db`)                  | Disk space accumulates; git branch delete doesn't clean up DB files; clean rebuild via `sync` is simpler.                                                      |
| CRDT-based automatic merge                                        | Silently accepts semantic contradictions. ADR-010 explicitly rejected CRDTs for KG merge.                                                                      |
| Wrapping every git command (`khive kg push/pull/branch/checkout`) | Adds maintenance cost without KG-specific intelligence; only works in the CLI (not IDE clients); git hooks + sync achieve the same result with less code.      |
| Tags or branch names as remote pins                               | Mutable references break reproducibility; commit SHAs are the only immutable pin.                                                                              |
| Auto-apply migrations on pull                                     | Silent data rewrites violate the "every change is a commit" principle.                                                                                         |
| Multi-KG per repo via `--kg` flag                                 | Forces a qualifier on every verb; ambiguity by default; multi-repo + cross-repo edges is the cleaner answer.                                                   |

## Consequences

### Positive

- Git history IS the KG history. `git log .khive/kg/` shows every change, who, when, and
  why. No custom `log` command needed.
- GitHub PRs are the review surface. Reviewers see entity-level diffs through
  `khive kg diff` as a presentation layer. Approval, merge, and rollback use standard
  GitHub tooling.
- Cross-instance collaboration requires no khive infrastructure on the remote side. Fork
  the GitHub repo; clone it; run `khive kg sync`.
- CI runs through standard GitHub Actions. No custom CI plugin.
- The export format is a stable interchange — any tool that understands NDJSON and the
  schema can consume a khive KG without the Rust runtime.
- Git hooks make sync automatic across all git interfaces (CLI, IDE, GUI).
- `working.db` is fully reconstructable from NDJSON. No backup strategy needed; lost or
  corrupted DBs rebuild on next sync.

### Negative

- Git must be installed and on `$PATH` for git-native mode. Mitigation: git is nearly
  universal in development environments; documented as a prerequisite.
- NDJSON merge conflicts appear as raw JSON in the conflict markers. Users who open the
  file directly see JSON, not entity-level diff. Mitigation: `khive kg resolve` renders
  them in entity-aware terms.
- Remote resolution requires network access for `validate --resolve-remotes`. Mitigation:
  cached results in `.khive/kg/.remote-cache/` make repeated runs offline-safe.
- Major-version schema bumps require explicit user action (`khive kg migrate`). This is
  deliberate friction — breaking changes warrant intent.
- Branch switching triggers a DB rebuild via `post-checkout`. At 100K entities this is
  ~20–30 seconds. Mitigation: incremental rebuild is a future optimization when the
  scale demands it.
- The full-comparison `khive kg status` reads the entire DB and both NDJSON files.
  Sub-second below ~100K entities; the row-level change log is the optimization path
  beyond that.

### Neutral

- `KgArchive` from [ADR-010](ADR-010-kg-versioning.md) is preserved as the in-memory
  export/import type. Export serializes it to NDJSON; import deserializes from NDJSON.
- The `working.db` schema is a subset of the main khive database schema. The two are
  structurally compatible.
- `schema.yaml` constraints are enforced at commit time, not at write time. Matches
  git's philosophy: working tree is permissive, commit gate enforces invariants.

## Implementation

### Crate structure

```
crates/khive-vcs/
├── Cargo.toml
└── src/
    ├── lib.rs          — re-exports
    ├── schema.rs       — SchemaYaml type, format/ontology version logic
    ├── storage.rs      — working.db DDL, mode detection ($CWD walk)
    ├── export.rs       — DB → NDJSON
    ├── import.rs       — NDJSON → DB (atomic)
    ├── validate.rs     — schema + integrity + sort check
    ├── commit.rs       — export + validate + git commit
    ├── sync.rs         — atomic rebuild (validate → temp DB → rename)
    ├── status.rs       — DB-vs-NDJSON diff
    ├── diff.rs         — entity-aware diff renderer
    ├── resolve.rs      — conflict marker parser + strategy application
    ├── remote.rs       — RemoteResolver (sparse checkout + GitHub API + cache)
    ├── update.rs       — bump remote SHA + re-validate
    └── migrate.rs      — MigrationFile parser, MigrationOp enum, apply_migration()
```

`khive-vcs` is a library crate. The user-facing `khive kg <verb>` CLI surface lives in
the npm-distributed `khive` wrapper (per [ADR-026](ADR-026-rust-binary-packaging.md)), which
invokes `kkernel` admin primitives (`kkernel sync`, `kkernel export`, `kkernel import`,
`kkernel validate`, `kkernel db migrate`) that call into `khive-vcs` for the heavy lifting.

### MCP surface impact

None. `khive kg *` commands are CLI-only. The MCP server surface
([ADR-016](ADR-016-request-dsl.md)) is unchanged. Git operations are not surfaced through
MCP.

### CLI distribution

The `kkernel` CLI ships as a Deno-compiled binary via npm:

```bash
npx khive kg init        # run without install
npm install -g kkernel     # global install
```

The npm package contains pre-compiled platform binaries (darwin-arm64, darwin-x64,
linux-x64, linux-arm64, windows-x64) produced by `deno compile` in CI. Same distribution
model as `esbuild` and `turbo`.

## References

- [ADR-002](ADR-002-edge-ontology.md): Edge ontology — `validate` enforces the 15-relation
  closed enum and pack-extensible endpoint contract on every NDJSON import
- [ADR-003](ADR-003-system-architecture.md): System architecture — kkernel binary
- [ADR-005](ADR-005-storage-capability-traits.md): Storage capability traits —
  `working.db` implements the same SqlAccess + GraphStore + VectorStore + TextSearch
  capabilities as the main database
- [ADR-010](ADR-010-kg-versioning.md): KG versioning strategy — this ADR is its
  implementation contract
- [ADR-014](ADR-014-curation-operations.md): Curation operations — `import --on-conflict
  update` uses the same `EntityPatch` semantics as the `update` verb
- [ADR-015](ADR-015-schema-migrations.md): Schema migrations — storage-layer migration
  system. KG schema migrations (§9) are a parallel system at the ontology layer; the two
  are independent
- [ADR-016](ADR-016-request-dsl.md): Request DSL — unchanged by this ADR
- [ADR-017](ADR-017-pack-standard.md): Pack standard — `EDGE_RULES` validation applies to
  NDJSON imports
- [ADR-018](ADR-018-authorization-gate.md): Authorization gate — gate evaluation applies
  to write operations regardless of whether they target `working.db` or the main database
- [ADR-023](ADR-023-declarative-pack-format.md): Declarative pack format — `schema.yaml#packs`
  section integrates with this ADR's vocabulary write-back
- NDJSON specification: <https://ndjson.org/>
- git sparse-checkout: <https://git-scm.com/docs/git-sparse-checkout>
- GitHub Contents API: <https://docs.github.com/en/rest/repos/contents>
