# ADR-052: KG Storage Model — DB and File Layer Reconciliation

**Status**: proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 defined the git-native NDJSON serialization format for committed KG state.
ADR-051 defined the CLI commands for the KG git workflow (`khive kg commit`, `push`,
`pull`, `status`, `branch`, `log`).

Neither ADR specified the mechanics of how the SQLite working database and the NDJSON
files stay in sync: what the directory layout is, which layer is authoritative at which
point in the lifecycle, how a diff is computed for `khive kg status`, how rebuilding
the DB from files works, and how `khive kg status` computes entity-level diff output without
requiring a `git status` round-trip.

This ADR fills that gap. It defines the storage model for the KG working state and the
reconciliation protocol that binds the two layers.

### The git analogy

The design is structurally identical to git's own storage model:

| git | khive KG |
|-----|---------|
| Working tree | `working.db` — live, editable via `khive create/link/update` |
| Object store / committed snapshot | `entities.ndjson`, `edges.ndjson` — committed, git-tracked |
| `git add && git commit` | `khive kg commit` (exports DB → files → `git commit`) |
| `git checkout` / `git pull` | `khive kg pull` (files → DB rebuild) |
| `.git/` | `.khive/kg/.state/` (gitignored, ephemeral) |

The DB is git's working tree: where active edits happen. The NDJSON files are the committed
snapshot: what git tracks, diffs, and merges. The `.state/` directory is the internal
bookkeeping that makes transitions between them efficient.

## Decision

### 1. Directory layout

```
.khive/kg/
  schema.yaml              # ontology definition (git-tracked)
  entities.ndjson          # committed entity data (git-tracked, diffable)
  edges.ndjson             # committed edge data (git-tracked, diffable)
  .state/                  # gitignored — working state (ephemeral)
    working.db             # SQLite with FTS5 + vector indexes
    HEAD                   # current branch name (plain text, mirrors git HEAD)
```

`khive kg init` creates the full structure and appends `.khive/kg/.state/` to
`.gitignore`. The `.state/` directory is not committed. It is fully reconstructable
from the NDJSON files at any time via `khive kg pull` (files → DB rebuild).

The three files under `.khive/kg/` (`schema.yaml`, `entities.ndjson`, `edges.ndjson`)
are committed to the project's git repository alongside source code. `entities.ndjson`
and `edges.ndjson` begin empty (`khive kg init` writes zero-length files with a
trailing newline to satisfy the NDJSON "newline after last record" rule).

### 2. One KG per repository

`.khive/kg/` is the KG for this repository. There is no multi-KG support within a
single repo. If a project needs multiple independent KGs, they live in separate repos.

Rationale: every verb that touches KG state — `create`, `search`, `link`, `traverse`
— would otherwise require a `--kg` qualifier. Removing that ambiguity simplifies the
verb surface, the CLI, the MCP dispatch layer, and the mental model. Multiple KGs
under one logical project should be rare; when they arise, git worktrees or submodules
are the correct structural answer, not a namespace multiplexer inside `.khive/kg/`.

Cross-project connections are expressed as cross-repo edges using the remote reference
syntax (`<remote>:<uuid>`) defined in ADR-048 §5. That mechanism already handles the
multi-project case without requiring multiple KGs in one repo.

### 3. Two-layer source of truth

Neither layer is unconditionally "the" source of truth. Each is authoritative at a
different phase of the lifecycle:

| Phase | Authoritative layer | Operation |
|-------|---------------------|-----------|
| Active work (between commits) | `working.db` | Reads and writes go to DB |
| Committed state | `entities.ndjson`, `edges.ndjson` | Files are what git tracks |
| Committing | DB wins | `commit` exports DB → files |
| Checking out / pulling | Files win | Files → DB rebuild |

The transition is always one-directional at a time. There is no bidirectional sync.
Bidirectional sync requires conflict resolution at the record level, which collapses
into the same problem as operational transformation or CRDTs — complexity without benefit
for a file-backed VCS system. git is the conflict resolution layer; it operates on files.
The DB is a materialized view of the files, rebuilt deterministically when needed.

### 4. Working database schema

`working.db` is a SQLite database with FTS5 and vector extensions (same as the main
khive database per ADR-009). Its schema mirrors the NDJSON fields, plus derived metadata
columns for timestamps that are not present in every NDJSON record (older exports omit them):

#### `entities` table

```sql
CREATE TABLE entities (
    id          TEXT PRIMARY KEY,      -- UUID, canonical form
    kind        TEXT NOT NULL,         -- EntityKind string
    name        TEXT NOT NULL,
    description TEXT,
    properties  TEXT NOT NULL DEFAULT '{}',  -- JSON object
    tags        TEXT NOT NULL DEFAULT '[]',  -- JSON array
    created_at  TEXT DEFAULT NULL,     -- ISO8601 or NULL if absent in NDJSON
    updated_at  TEXT DEFAULT NULL      -- ISO8601 or NULL if absent in NDJSON
);

CREATE INDEX idx_entities_kind ON entities(kind);
CREATE INDEX idx_entities_name ON entities(name);

CREATE VIRTUAL TABLE entities_fts USING fts5(
    name,
    description,
    content='entities',
    content_rowid='rowid'
);
```

#### `edges` table

```sql
CREATE TABLE edges (
    edge_id     TEXT NOT NULL,         -- UUID (edge identity, carried from ADR-048 D1)
    source      TEXT NOT NULL,
    target      TEXT NOT NULL,         -- local UUID or '<remote>:<uuid>'
    relation    TEXT NOT NULL,
    weight      REAL NOT NULL DEFAULT 1.0,
    properties  TEXT NOT NULL DEFAULT '{}',  -- JSON object
    created_at  TEXT DEFAULT NULL,     -- ISO8601 or NULL if absent in NDJSON
    updated_at  TEXT DEFAULT NULL,     -- ISO8601 or NULL if absent in NDJSON
    PRIMARY KEY (source, target, relation)   -- composite, mirrors NDJSON sort key
);

CREATE INDEX idx_edges_source ON edges(source);
CREATE INDEX idx_edges_target ON edges(target);
CREATE INDEX idx_edges_relation ON edges(relation);
```

The composite primary key `(source, target, relation)` matches the sort key used in
`edges.ndjson` (ADR-048 D5). The `edge_id` is carried for identity preservation across
export/import cycles (ADR-048 D1) but is not the primary key — the composite key is.

### 5. Reconciliation protocol

#### Commit flow: DB → files

Triggered by `khive kg commit`. Produces bit-identical output for identical logical state
(idempotent over the same DB content).

1. Open `working.db`. Run the §7 diff against committed NDJSON. If the diff is empty, report
   "nothing to commit, working KG clean" and exit.
2. Export all entities: `SELECT * FROM entities ORDER BY id ASC`.
3. For each entity row, serialize to a JSON object with fixed key ordering:
   `id`, `kind`, `name`, `description`, `properties`, `tags`, followed by `created_at` and
   `updated_at` when non-null. Within `properties`, keys sorted alphabetically. Within `tags`,
   values sorted lexicographically.
4. Write one serialized entity per line to `entities.ndjson`, with a trailing `\n` after
   the last record.
5. Export all edges: `SELECT * FROM edges ORDER BY source ASC, target ASC, relation ASC`.
6. For each edge row, serialize with fixed key ordering:
   `edge_id`, `source`, `target`, `relation`, `weight`, `properties`, followed by `created_at`
   and `updated_at` when non-null.
7. Write one serialized edge per line to `edges.ndjson`, with a trailing `\n`.
8. Stage the three KG files: `git add .khive/kg/entities.ndjson .khive/kg/edges.ndjson .khive/kg/schema.yaml`.
9. `git commit -m "<message>"` (message from CLI arg or interactive prompt).

The export is a full snapshot — there is no delta or incremental export. Given that
NDJSON files are small relative to a code repository, and that the files are git-tracked
so unchanged lines produce zero delta in the git object store, full-snapshot export is
simpler and more correct than tracking a change log.

#### Checkout/pull flow: files → DB (atomic)

Triggered by `khive kg pull` or `khive kg checkout`. Also triggered automatically by
`khive kg init` if the repo already contains committed NDJSON files.

1. Run the §7 diff against committed NDJSON. If the diff is non-empty, refuse with:
   "Uncommitted changes in working.db. Run `khive kg commit` or `khive kg reset` first."
2. **Validate first**: run `khive kg validate` against the NDJSON files before touching
   the database. If validation fails, abort with a structured error report. No DB changes
   are made on validation failure.
3. Create a temporary SQLite file at `.state/working.db.tmp` with the §4 DDL.
4. Open a single write transaction on `.state/working.db.tmp`.
5. Parse `entities.ndjson` line by line. For each line, `INSERT INTO entities`. Any
   JSON parse error aborts the transaction and removes `.state/working.db.tmp`.
6. Parse `edges.ndjson` line by line. `INSERT INTO edges`. Any error aborts the
   transaction and removes `.state/working.db.tmp`.
7. Rebuild FTS5 index: `INSERT INTO entities_fts(entities_fts) VALUES('rebuild')`.
8. Commit the transaction on `.state/working.db.tmp`.
9. Atomically rename `.state/working.db.tmp` → `.state/working.db` (replaces old DB).
10. Write current git branch name to `.state/HEAD`:
    `git rev-parse --abbrev-ref HEAD`.
11. (No dirty file to remove — status is computed from DB-vs-NDJSON diff, not a flag.)

The rename in step 9 is the only instant at which the DB transitions — either the old DB
survives intact (on any failure before step 9) or the new DB is fully populated (after step 9).
There is no partially-imported state observable by other processes.

The rebuild is idempotent. Running checkout twice on the same files produces the same DB.
This is the invariant that makes the DB a true materialized view of the NDJSON.

#### Reset flow: DB → files state (undo uncommitted changes)

`khive kg reset` discards uncommitted changes in the DB and rebuilds from the committed files:

1. Run the §7 diff to check whether there are uncommitted changes. If no changes, report
   "nothing to reset, working KG clean" and exit.
2. Run the checkout flow (atomic, steps 2–11 above), which validates and rebuilds `working.db`
   from the committed NDJSON files.
3. This effectively discards all changes made since the last commit.

### 6. Status tracking

`khive kg status` always computes status by comparing the current DB export against the committed
NDJSON files. There is no `.state/dirty` flag. This approach is canonical: it catches uncommitted
DB changes that `git status .khive/kg/` cannot see (because the DB is gitignored), while
remaining correct across crash recovery, concurrent writes, and process restarts.

When `khive kg status` runs:

1. Export the current DB to an in-memory sorted NDJSON representation (same serialization
   as commit flow, but no file write).
2. Parse the committed `entities.ndjson` and `edges.ndjson` into in-memory maps.
3. Diff and render output (see §7 for the diff algorithm).
4. If the diff is empty: report "On branch <HEAD> — nothing to commit, working KG clean." Exit 0.
5. If the diff is non-empty: render the entity-level diff and exit non-zero.

The full-comparison cost is bounded: for graphs up to ~100K entities the in-memory diff is
sub-second on modern hardware (§7). At larger scales, a row-level change log table in
`working.db` is an optimization path deferred to a later version.

The `.state/` directory layout shown in §1 no longer includes a `dirty` file. The directory
contains only `working.db` and `HEAD`.

### 7. Status and diff computation

`khive kg status` computes an entity-level diff between the committed NDJSON and the current DB
state. This is always computed on every `khive kg status` invocation (see §6 for rationale).

Algorithm:

1. Export the current DB to an in-memory sorted NDJSON representation (same serialization
   as commit flow, but no file write).
2. Parse the committed `entities.ndjson` into an in-memory map keyed by entity UUID.
3. Parse the committed `edges.ndjson` into an in-memory map keyed by `(source, target, relation)`.
4. Diff entities:
   - UUIDs in DB but not in committed files → new entities ("+").
   - UUIDs in committed files but not in DB → deleted entities ("-").
   - UUIDs in both with different serialized JSON → modified entities ("~").
5. Diff edges using the composite key.
6. Render summary output:

```
On branch main
KG status (3 uncommitted changes):
  Modified entities: 1
    ~ concept "LoRA" (671b882a) — properties.status changed
  New entities: 1
    + concept "QLoRA" (a3f2c1d4)
  New edges: 1
    + LoRA --[extends]--> QLoRA
```

The diff runs in memory; no temporary files are written. For graphs up to ~100K entities the
full-snapshot comparison is sub-second on modern hardware. If performance becomes a concern at
larger scale, a row-level change log table in `working.db` can replace the full comparison — this
is a deferred optimization, not a v0.4 requirement.

### 8. Standalone vs. git-native mode

`khive create`, `search`, `link`, and other KG verbs work in either mode:

**Standalone mode** (no `khive kg init` has been run): The verb surface writes to the
main khive database (`~/.khive/khive.db` or the configured path). No `.khive/kg/` directory
exists. No versioning. This is the existing behavior — this ADR does not change it.

**Git-native mode** (`khive kg init` has been run): The verb surface writes to
`.khive/kg/.state/working.db`. The `khive kg` subcommands (`commit`, `status`, `pull`,
`reset`) manage the lifecycle. Requires git on `$PATH`.

Mode detection at runtime: if `.khive/kg/.state/working.db` exists in the current directory
or any parent (walking up to the filesystem root), the KG verbs route to that DB. Otherwise,
they route to the main database.

This is the same heuristic git uses to locate `.git/` — the CLI walks up from `$CWD` to find
the nearest `.khive/kg/.state/working.db`. If found, git-native mode is active. If not found,
standalone mode is used. The search stops at the filesystem root.

### 9. Initialization

`khive kg init` performs the following:

1. **Detect state**:
   - If `.khive/kg/` exists AND `.state/working.db` exists: error
     "KG already initialized in this directory (working.db found). Run `khive kg status` to
     check state, or `khive kg reset` to rebuild from committed files."
   - If `.khive/kg/` exists AND `.state/working.db` is absent (fresh clone):
     bootstrap mode — skip to step 3 (skip creating files that already exist).
   - If `.khive/kg/` does not exist: fresh init — execute all steps below.

2. If the current directory is not a git repository: run `git init`.

3. Create `.khive/kg/` and `.khive/kg/.state/` if they do not already exist.

4. If `schema.yaml` does not already exist: write default `schema.yaml` (full ADR-001 entity
   kinds + ADR-002 edge relations, `format_version: "1.0.0"`, `ontology_version: "1.0.0"`,
   `khive_version: "<current>"`, empty `remotes: []`).

5. If `entities.ndjson` does not already exist: write empty `entities.ndjson` (single `\n`).
   If `edges.ndjson` does not already exist: write empty `edges.ndjson` (single `\n`).

6. If committed NDJSON files are non-empty (bootstrap mode): run the §5 atomic rebuild
   (validate → temp DB → swap) to populate `working.db` from the existing NDJSON files.
   If the NDJSON files are empty (fresh init): create empty `working.db` with the §4 schema.

7. Write current git branch to `.state/HEAD`.

8. Append `.khive/kg/.state/` to `.gitignore` (creating `.gitignore` if absent, idempotent
   if the entry already exists).

9. Stage the three KG files and `.gitignore`:
   `git add .khive/kg/schema.yaml .khive/kg/entities.ndjson .khive/kg/edges.ndjson .gitignore`.

10. Emit: "KG initialized. Run `khive kg status` to check state."

`khive kg init` does not make a git commit. Staging the files is sufficient — the first
`khive kg commit` will commit them alongside actual graph content.

**Fresh clone bootstrap**: On a fresh clone, `.khive/kg/` contains tracked NDJSON files but no
`.state/` directory (`.state/` is gitignored). Running `khive kg init` detects this state
(`.khive/kg/` present, `working.db` absent) and bootstraps `.state/working.db` via the atomic
rebuild flow (§5). This is the expected onboarding path for contributors joining an existing KG
project. No data is lost or overwritten — the NDJSON files are the source of truth, and the
DB is rebuilt from them deterministically.

## Alternatives Considered

### NDJSON as direct editing surface

Users edit `entities.ndjson` and `edges.ndjson` directly, with no intermediate DB. The
runtime re-parses the files on every read.

Rejected: NDJSON files are not searchable. `khive search`, `khive traverse`, and
`khive query` require SQLite's FTS5 and graph index structures. Parsing 10K-entity NDJSON
on every search call would make the tool unusable. The DB is not optional for the verb
surface; it is the index structure that makes the verb surface fast.

### DB as sole source of truth, NDJSON export-only

The DB is always authoritative. NDJSON is a one-way export format, like `git archive`.
Merging and branching are done at the DB level.

Rejected: This was the ADR-042 approach, superseded by ADR-048. The problem is that SQLite
binary files are not git-diffable or git-mergeable. Two branches that each add entities
cannot be automatically merged by git — the result is a binary conflict. NDJSON with sorted
keys is specifically designed to be line-addressable so that non-overlapping entity additions
merge cleanly. The NDJSON-as-committed-state model is the entire value proposition of ADR-048.

### Bidirectional sync (CouchDB / Ditto style)

Changes in either direction (DB edits or file edits) are continuously synced in both
directions, with automatic conflict detection at the record level.

Rejected: Bidirectional sync requires a change log in the DB, a change log in the files
(or file-level change detection), a comparison algorithm that identifies conflicting edits,
and a resolution protocol. This is the problem that CRDTs and operational transformation
solve — both of which ADR-010 explicitly rejected for the KG layer. The one-directional
model (DB for writes, files for commits) eliminates the sync problem entirely by never
having two concurrent authoritative sources.

### SQLite files in git directly

Commit `working.db` to git instead of exporting to NDJSON. Git tracks the binary.

Rejected: SQLite files produce binary diffs that git cannot render or merge. A PR that
adds 5 entities shows as a changed binary blob with no readable diff. The entire review
workflow — which is central to "GitHub for knowledge graphs" — requires human-readable,
line-addressable diffs. This is the status quo that ADR-048 was designed to escape.

### Multiple KGs per repository (namespace multiplexer)

Support `.khive/kg/<name>/` with multiple independent KGs per repo, selected by a
`--kg` flag or a config key.

Rejected: Every command that touches the KG — `create`, `search`, `link`, `status`,
`commit` — would require a `--kg` qualifier or a per-session config to disambiguate.
The verb surface becomes ambiguous by default. The appropriate way to have multiple
independent KGs is to have multiple repos — which is already supported via cross-repo
edges. The one-KG-per-repo constraint is a deliberate simplicity decision, not a
limitation of the implementation.

## Consequences

### Positive

- The DB is always reconstructable from the NDJSON files. No backup strategy is needed
  for `.state/working.db` — if it is lost or corrupted, `khive kg pull` rebuilds it.
- `khive kg status` is always computed from a DB-vs-NDJSON diff, which catches DB changes
  that `git status` cannot see (the DB is gitignored). The diff is bounded by graph size
  and is sub-second for graphs up to ~100K entities.
- The serialization is deterministic: the same logical graph state always produces
  bit-identical NDJSON files. This means `git diff` on the NDJSON files is a reliable
  signal — if `git diff` shows no changes, the DB and files are in sync.
- The DB schema mirrors the NDJSON fields exactly. Import is a mechanical field mapping:
  optional NDJSON timestamps become DB column values (NULL when absent from the NDJSON record).
  Export omits NULL timestamps, matching the source. This makes import → export a round-trip
  identity: importing an old NDJSON file without timestamps writes NULL to the DB, and exporting
  that DB omits the timestamp fields — producing bit-identical output to the source file.
- Mode detection is implicit and consistent with git's own heuristic. Users do not need
  to configure which mode they are in — the presence of `working.db` in the directory
  tree is sufficient.

### Negative

- `khive kg pull` is destructive: it drops and rebuilds `working.db`. Uncommitted changes
  are lost. The guard in step 1 of the checkout flow (refuse if DB-vs-NDJSON diff is
  non-empty) mitigates accidental data loss, but the user must explicitly run
  `khive kg reset` or `khive kg commit` before pulling.
- The full-snapshot diff in `khive kg status` reads the entire DB and both NDJSON files.
  For large graphs (100K+ entities), this may be slow. Mitigation: a row-level change log
  table is an optimization path, deferred to a later version.
- Git must be installed and on `$PATH` for git-native mode to function. Standalone mode
  has no such dependency, but git-native mode's value proposition requires git.
- Two processes writing to `working.db` concurrently (two agent sessions in the same
  repo) may interleave writes. SQLite's WAL mode handles concurrent readers and single
  writers safely. `khive kg status` computing a full diff on each invocation means each
  call sees a consistent snapshot of the DB at that moment. This is an edge case that
  does not require resolution in v0.4; the worktree model (ADR-027) means each agent
  session typically has its own working directory.

### Neutral

- The `working.db` schema is a subset of the main khive database schema (`~/.khive/khive.db`).
  The two databases are structurally compatible, which simplifies the implementation of
  import from a local namespace into git-native mode.
- `schema.yaml` is not imported into `working.db`. It is read from disk during `validate`,
  `commit`, and `pull`. Schema constraints are enforced at commit time, not at write time.
  This matches git's philosophy: the working tree is permissive; the commit gate enforces
  invariants.

## Implementation

### Crate changes

The storage model is implemented primarily in `crates/khive-vcs/`:

```
crates/khive-vcs/src/
  lib.rs          — re-exports
  storage.rs      — working.db DDL, open/create, mode detection (walk $CWD → root)
  commit.rs       — commit flow (§5 commit protocol)
  checkout.rs     — checkout/pull flow (§5 checkout protocol)
  reset.rs        — reset flow (§5 reset protocol)
  status.rs       — DB-vs-NDJSON diff computation (§6, §7)
  init.rs         — khive kg init (§9)
  schema.rs       — SchemaYaml type (from ADR-048)
  export.rs       — export() (from ADR-048)
  import.rs       — import() (from ADR-048)
  validate.rs     — validate() (from ADR-048)
  diff.rs         — entity-aware diff rendering (from ADR-048)
  remote.rs       — RemoteResolver (from ADR-048)
  update.rs       — update_remote() (from ADR-048)
```

`storage.rs` is new in this ADR. The other modules (`schema.rs`, `export.rs`, `import.rs`,
`validate.rs`, `diff.rs`, `remote.rs`, `update.rs`) were defined in ADR-048; this ADR
adds `commit.rs`, `checkout.rs`, `reset.rs`, `status.rs`, and `init.rs`.

### Mode detection integration

The mode detection logic in `storage.rs` must be called by the pack verb handlers
(`crates/khive-pack-kg/src/handlers.rs`) before opening a database connection. If
`working.db` is found in the directory tree, the verb handlers use it instead of the
main database. The `KhiveRuntime` receives a `DatabasePath` parameter that the mode
detection code resolves; the verb surface itself is unchanged.

### Phasing

| Phase | Scope | Target |
|-------|-------|--------|
| S1 | `storage.rs` + `init.rs` — DB DDL, init command | v0.4 |
| S2 | `commit.rs` + `checkout.rs` — commit and pull flows | v0.4 |
| S3 | `status.rs` — DB-vs-NDJSON diff computation | v0.4 |
| S4 | `reset.rs` + mode detection in pack handlers | v0.4 |
| S5 | Performance: row-level change log for large graphs | v0.6 (deferred) |

S1–S4 form a complete v0.4 implementation. The core workflow (`init` → `create/link` →
`status` → `commit` → `pull`) is covered by S1–S3. Mode detection (S4) enables the verb
surface to route transparently to `working.db` without a `--kg` qualifier.

## References

- ADR-048: Git-Native KG Versioning — NDJSON format, sort rules, field ordering, cross-repo references
- ADR-051: CLI Authentication and KG Git Workflow Commands — `khive kg commit/push/pull/status/branch/log` CLI definitions
- ADR-042: KG Versioning Implementation — superseded; original DB-only approach this ADR replaces
- ADR-009: Backend Portability — SQLite backend, WAL mode, FTS5 extension
- ADR-022: Schema Migrations — migration system (not used for `working.db`, which is rebuilt on checkout)
- ADR-028: Request Parser Crate — `khive-request` as the DSL parser that routes verbs to handlers
