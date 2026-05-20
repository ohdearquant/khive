# ADR-048: Git-Native KG Versioning

**Status**: proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Scope

This ADR covers entity and edge versioning for the git-native KG format. Notes, tasks, and events
are pack-specific data managed by their respective packs (GTD, memory) and are NOT included in the
git-native KG format. The `.khive/kg/` directory contains the knowledge graph structure only.
Pack-specific persistence is deferred to pack-level ADRs.

The authoritative definition of entity-level status output (dirty tracking, DB-vs-NDJSON diff) is
ADR-052. This ADR defines the serialization format; ADR-052 defines the reconciliation protocol.

## Context

ADR-042 designed a custom VCS layer for knowledge graph versioning: content-addressed snapshots
stored in SQLite, a bespoke HTTP push/pull protocol (`khive-sync`), and custom branch pointer
tables. ADR-043 designed a three-way merge algorithm implemented in a dedicated `khive-merge`
crate. Together they amount to implementing a version control system inside khive.

Two problems with this approach have become clear during design review:

1. **Custom VCS duplicates solved problems.** Snapshots, branch pointers, merge, conflict
   visualization, access control, social review, CI, and federation are all solved problems in
   git/GitHub. Building equivalent functionality in Rust atop SQLite is months of work that users
   will still find unfamiliar.

2. **The custom remote protocol creates a walled garden.** Sharing a research KG today requires
   both parties to run a khive sync server, exchange bearer tokens, and use khive-specific tooling.
   A KG stored as files in a git repo can be shared with anyone on GitHub — no khive installation
   required to read, browse, diff, or fork.

ADR-010's strategic framing is "GitHub for knowledge graphs." The most direct path to that
positioning is to treat git and GitHub as the versioning infrastructure rather than rebuild their
functionality. This ADR proposes replacing the snapshot/branch/remote design of ADR-042 with a
file-based serialization format whose versioning, diffing, merging, reviewing, and federation are
handled by git.

### What changes and what does not

- ADR-010 (strategic direction): unchanged. "GitHub for knowledge graphs" is still the positioning;
  this ADR is a better path to it.
- ADR-043 three-way merge algorithm: substantially simplified. Git handles line-level NDJSON merge
  for non-conflicting additions. ADR-043's entity/edge categorization logic shrinks to a validation
  pass over post-merge files rather than a full merge engine.
- ADR-042 custom VCS operations: replaced. `commit`, `branch`, `checkout`, `merge_branch`, `log`,
  `push`, `pull`, and the `khive-sync` HTTP server are all replaced by the git CLI and GitHub.
  The `khive-vcs` crate's scope reduces to `export`, `import`, and `validate`.
- `KgArchive` type in `khive-runtime/src/portability.rs`: preserved as the in-memory
  representation. Export serializes it to NDJSON files; import deserializes from them.

## Decision

### 1. File layout

The KG for a project is serialized into three files under `.khive/kg/`:

```
.khive/kg/
  schema.yaml          # ontology: entity kinds, edge relations, endpoint rules, remotes
  entities.ndjson      # one entity record per line, sorted by UUID
  edges.ndjson         # one edge record per line, sorted by source_id+target_id+relation
```

These files are committed to the project's git repository alongside source code, documentation,
and configuration. Git provides the commit history; GitHub provides diffs, PRs, and review.

Every file is UTF-8 plain text. The `.khive/kg/` directory should be included in the project's
`.gitattributes` with `text eol=lf` to ensure line-ending stability across platforms, which is
required for stable NDJSON diffs.

### 2. NDJSON format

NDJSON (Newline-Delimited JSON) uses one self-contained JSON record per line with a Unix newline
(`\n`) after each record, including the last.

**Sorting rule**: files are kept in sorted primary-key order at all times.

- `entities.ndjson`: sorted by entity UUID (string, case-insensitive ascending).
- `edges.ndjson`: sorted by `(source_id, target_id, relation)` (all ascending, lexicographic).

Sorting is the key design choice. It ensures that:

- A new entity is always an `+` line in a well-defined location in the file, not appended at the
  end. This makes diffs readable and merge patches non-overlapping.
- Git's 3-way merge can combine non-overlapping additions from two branches automatically. Two
  agents each adding different entities will produce a clean merge with no human intervention.
- Two exports from the same logical graph state always produce bit-identical files, enabling
  integrity checks.

#### Entity record shape

```json
{"id":"<uuid>","kind":"<EntityKind>","name":"<string>","description":"<string|null>","properties":{...},"tags":["..."],"created_at":"<ISO8601|omit>","updated_at":"<ISO8601|omit>"}
```

Field ordering within the JSON object is fixed to: `id`, `kind`, `name`, `description`,
`properties`, `tags`, `created_at`, `updated_at`. The timestamp fields are **optional**: they
appear when present in the database and are omitted when absent (for compatibility with NDJSON
files produced before timestamps were recorded). Within `properties`, keys are sorted
alphabetically. Within `tags`, values are sorted lexicographically. This fixed ordering ensures
that re-exporting the same logical entity always produces the same bytes, making the file diff
meaningful (changed fields are visible) and the SHA-256 of the file stable.

Soft-deleted entities are excluded from the export. The NDJSON files represent live graph state,
consistent with ADR-042 §1's canonical hash algorithm.

#### Edge record shape

```json
{"edge_id":"<uuid>","source":"<uuid>","target":"<uuid|remote_ref>","relation":"<EdgeRelation>","weight":<float>,"properties":{...},"created_at":"<ISO8601|omit>","updated_at":"<ISO8601|omit>"}
```

Field ordering within the JSON object is fixed to: `edge_id`, `source`, `target`, `relation`,
`weight`, `properties`, `created_at`, `updated_at`. The `properties` field is always present (an
empty object `{}` when no properties are set). The timestamp fields are **optional**: they appear
when present in the database and are omitted when absent (for compatibility with NDJSON files
produced before timestamps were recorded). The `edge_id` preserves edge identity across
export/import cycles (see D1).

The `target` field may be either a local UUID or a remote reference (see §5 on cross-repo edges).

### 3. `schema.yaml` format

`schema.yaml` is the ontology manifest for the project's KG. It declares what kinds and relations
are in use, what cross-repo references exist, and pins the schema version.

```yaml
format_version: "1.0.0"   # semver; file format compatibility version (what fields are valid here)
khive_version: "0.1.0"    # CLI version that wrote this file (informational)

entity_kinds:
  - concept
  - document
  - dataset
  - project
  - person
  - org

edge_relations:
  - relation: contains
    category: structure
    endpoints:
      - [concept, concept]
      - [project, concept]
  # ... one entry per canonical relation from ADR-002
  # endpoint pairs list the (source_kind, target_kind) pairs that are legal
  # omitting endpoints means the relation accepts any (entity, entity) pair

properties:
  concept:
    - key: type
      values: [paper, algorithm, technique, architecture, model, benchmark, dataset, tool, adr]
    - key: domain
    - key: status
      values: [concept, researched, prototyped, implemented, shipped, deprecated]
    - key: title
    - key: authors
    - key: year
    - key: source
  # ... per-kind property keys and optional allowed value sets

remotes:
  - name: lattice
    repo: ohdearquant/lattice
    path: .khive/kg
    commit: a1b2c3d4e5f6789012345678901234567890abcd   # full 40-char SHA — immutable
  - name: atlas
    repo: ohdearquant/atlas
    path: .khive/kg
    commit: f9e8d7c6b5a4321098765432109876543210fedc
```

`schema.yaml` is the only file that changes when a remote reference is bumped or when an ontology
amendment is made. This makes ontology changes reviewable in PRs as a single-file diff against the
previous version, independent of entity and edge data.

The `format_version` field is the file format compatibility version (what fields are valid in this
`schema.yaml`), not a version counter for the graph data itself or the ontology. Data versioning
is git's job. Ontology versioning (entity kinds, relations, property schemas) is tracked separately
via `ontology_version` defined in ADR-054.

The `commit` field in each remote entry must be a full 40-character git commit SHA. This is the
only field that unambiguously identifies a point in a remote repository's history:

- Tags can be moved (`git tag -f`) — a tag ref is not a stable pointer.
- Branch names are mutable by definition — `main` today is not `main` in six months.
- A commit SHA is content-addressed and immutable. The same SHA always resolves to the same tree.

Tags are accepted as input to `khive kg update <remote> --ref v0.3.0` but are immediately
resolved to their underlying commit SHA before writing to `schema.yaml`. The stored value is
always the SHA. This ensures that `khive kg import` at any point in the project's git history
resolves remote references to the exact same remote graph state that was current when the import
was first recorded — full historical reproducibility.

### 4. CLI commands

Four commands replace all of ADR-042's VCS operations:

#### `khive kg init`

Creates `.khive/kg/` with a default `schema.yaml` using the full ADR-001 entity kinds and ADR-002
edge relations. The generated `schema.yaml` includes `format_version` and `khive_version` (the CLI
version that created it, informational). Creates empty `entities.ndjson` and `edges.ndjson` files.
Errors if `.khive/kg/` already exists.

#### `khive kg export`

Reads all live (non-soft-deleted) entities and edges from the local SQLite database for the
current namespace and writes them to `.khive/kg/entities.ndjson` and `.khive/kg/edges.ndjson`.
Entities are sorted by UUID; edges are sorted by `(source_id, target_id, relation)`.

After export, the files can be committed with `git add .khive/kg/ && git commit`.

Export is idempotent: running it twice with no intervening writes produces bit-identical files.

#### `khive kg import`

Reads `.khive/kg/entities.ndjson` and `.khive/kg/edges.ndjson`, resolves any cross-repo references
in edge targets (see §5), and loads all records into the local SQLite database for the current
namespace.

Import accepts an `--on-conflict` parameter:

- `error` (default): fail if any UUID already exists in the database.
- `skip`: silently skip records that already exist.
- `update`: overwrite existing records with the file contents.

On failure, the import transaction is rolled back entirely (all-or-nothing).

#### `khive kg validate`

Checks the NDJSON files against the schema without touching the database:

1. **Schema compliance**: every `kind` in `entities.ndjson` appears in `schema.yaml#entity_kinds`.
   Every `relation` in `edges.ndjson` appears in `schema.yaml#edge_relations`. Property keys
   match the per-kind declarations.
2. **Referential integrity**: every edge `source` and `target` UUID (excluding remote references)
   resolves to an entity UUID present in `entities.ndjson`.
3. **Remote resolution**: for each remote reference `<remote>:<uuid>`, the named remote in
   `schema.yaml#remotes` exists, and the entity UUID can be resolved at the pinned `ref` (via
   sparse git checkout or GitHub API, see §5).
4. **No duplicate UUIDs**: no entity UUID appears more than once in `entities.ndjson`. No edge
   composite key `(source, target, relation)` appears more than once in `edges.ndjson`.
5. **Sort order**: entity lines are UUID-ascending; edge lines are composite-key-ascending.

`validate` exits with a non-zero code and a structured error report on any violation. It exits
with 0 and no output on a clean graph.

#### `khive kg status`

Detailed entity-level status — computing which entities and edges are uncommitted relative to the
NDJSON files — is defined in ADR-052 §6 and §7. ADR-052 is the canonical definition of the status
contract. The DB-vs-NDJSON diff approach (comparing a live DB export against committed files) is
the authoritative mechanism; it catches uncommitted DB changes that `git status` cannot see.

#### `khive kg diff`

Renders entity-aware diff output from two `entities.ndjson` or `edges.ndjson` files (or between
the working tree and a git ref). Rather than raw NDJSON line diffs, `khive kg diff` parses both
files and shows changes at the entity/edge level:

```
~ entity 671b882a (concept "LoRA")
    properties.status: "researched" → "implemented"

+ entity a3f2c1d4 (concept "QLoRA")
    name: QLoRA
    kind: concept
    properties.type: technique

- edge 671b882a --[competes_with]--> c9e4b3f2 (QLoRA)
```

This is a presentation layer over git diff, not a custom diff engine. The underlying diff is
computed by `git diff` on the NDJSON files; `khive kg diff` parses the unified diff output and
re-renders it in entity-aware terms.

#### `khive kg update <remote>`

Advances the `commit` SHA for a named remote in `schema.yaml`. The update source is:

- **Default** (`khive kg update lattice`): resolves the HEAD commit of the remote repo's default
  branch. Fetches the latest commit SHA via the GitHub API or a git `ls-remote` call without
  cloning.
- **Tag** (`khive kg update lattice --ref v0.3.0`): resolves the named tag to its underlying
  commit SHA via `git ls-remote --tags`. The stored value is the commit SHA, not the tag name.
  This ensures the pin is immutable even if the tag is later moved.
- **Branch** (`khive kg update lattice --ref feat/new-entities`): resolves the branch HEAD SHA.
  A warning is emitted noting that the pin will drift if the branch advances further. Use case:
  tracking a feature branch during active development; pin to a tag before merging to production.

After computing the new SHA, `update` writes it to `schema.yaml#remotes[<remote>].commit` and
runs `validate` to confirm all cross-repo edge references resolve at the new commit. If validation
fails (an entity UUID referenced in a local edge was removed from the remote at the new commit),
the SHA update is reverted and the failure is printed with the specific missing UUIDs listed.

The SHA change in `schema.yaml` is a one-line diff in PRs. Reviewers can click through to
`github.com/<repo>/commit/<sha>` to inspect exactly what changed in the remote KG between the
previous and new pins.

### 5. Cross-repo references

Edges can reference entities in remote repositories. The `target` field in an edge record uses a
`<remote>:<uuid>` prefix to indicate a cross-repo entity:

```json
{"source":"671b882a-...","target":"lattice:c9e4b3f2-...","relation":"implements","weight":1.0}
```

The `lattice` prefix maps to a remote defined in `schema.yaml#remotes`:

```yaml
remotes:
  - name: lattice
    repo: ohdearquant/lattice
    path: .khive/kg
    commit: a1b2c3d4e5f6789012345678901234567890abcd   # full SHA — immutable
```

#### Remote entity resolution

`khive kg validate` and `khive kg import` resolve remote references in two ways, in order of
preference:

1. **Sparse git checkout**: `git archive --remote=https://github.com/ohdearquant/lattice.git
   a1b2c3d4e5f6789012345678901234567890abcd .khive/kg/entities.ndjson | tar -x` produces a local
   copy of the pinned file without cloning the full repository. The UUID is then looked up in the
   extracted file. Using a commit SHA here guarantees the archive matches the pinned state exactly.

2. **GitHub Contents API**: `GET /repos/ohdearquant/lattice/contents/.khive/kg/entities.ndjson?ref=a1b2c3d4e5f6789012345678901234567890abcd`
   returns the file content as base64. Used as a fallback when sparse checkout is unavailable
   (e.g., unauthenticated access to a private repo). The `?ref=<sha>` query pins the retrieval to
   the exact commit, not a branch HEAD.

Remote resolution results are cached in `.khive/kg/.remote-cache/<remote>-<sha>.ndjson` to
avoid repeated network calls during validation runs. The cache is keyed by `(remote, commit-sha)`
— because `schema.yaml` always stores a full commit SHA, the cache entry is immutable and never
expires. The only time a cache entry is stale is if a remote repo force-pushed and the same SHA
now points to different content, which is a violation of git immutability invariants and not
defended against here.

#### Version pinning rules

The `commit` field in `schema.yaml#remotes` must be a full 40-character git SHA. This is
validated by `khive kg validate` — any other format (short SHA, branch name, tag name) is a
validation error:

```
ERROR: remote 'atlas' commit field is not a full 40-char SHA: "main"
  Run 'khive kg update atlas' to resolve and pin to a commit SHA.
```

The rationale is reproducibility: a consumer who checks out any commit in the project's git
history must be able to resolve all cross-repo references at the exact remote state that was
current when those edges were recorded. A commit SHA is the only reference type that guarantees
this — tags and branch names are mutable pointers that may no longer point to the same tree.

`khive kg validate` does not resolve remote SHAs on every run (that would require a network call).
It only validates that the format is correct. The `--resolve-remotes` flag triggers full remote
resolution (downloads the pinned remote NDJSON and checks that all referenced UUIDs exist in it);
this is the mode used by CI.

### 6. CI integration

A GitHub Actions workflow is generated by `khive kg init` and placed at
`.github/workflows/kg-validate.yml`. It runs on any push or pull request that touches
`.khive/kg/**`:

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
      - uses: cargo-bins/cargo-binstall@main
      - run: cargo binstall khive-cli --no-confirm
      - run: khive kg validate
```

The workflow catches:

- Malformed NDJSON (parse errors).
- Invalid entity kinds or edge relations (not in `schema.yaml`).
- Dangling edge endpoints.
- Unresolvable cross-repo references at the pinned ref.
- Duplicate UUIDs.
- Out-of-sort lines (which would cause merge surprises).

PRs that add or modify entities and edges cannot be merged until CI passes. This enforces the same
referential integrity guarantee that `link` enforces at runtime (ADR-039 §boundary condition).

### 7. Relationship to ADR-042 and ADR-043

This ADR supersedes the implementation approach of ADR-042 and substantially reduces the scope of
ADR-043.

#### ADR-042 supersession

| ADR-042 component                            | Disposition in ADR-048                              |
| -------------------------------------------- | --------------------------------------------------- |
| `kg_snapshots` + `kg_snapshot_archives` SQL  | Deleted. Git provides the commit history.           |
| `kg_branches` SQL table                      | Deleted. Git branches replace this.                 |
| `kg_vcs_state` dirty-flag table              | Deleted. Status is computed via DB-vs-NDJSON diff (ADR-052 §6–§7). |
| SHA-256 canonical hash algorithm             | Retained. Export determinism ensures stable hashes. |
| `khive-sync` HTTP server + push/pull API     | Deleted. `git push` / `git pull` replace this.      |
| `commit`, `branch`, `checkout`, `log` tools  | Deleted. `git commit`, `git branch`, etc., replace. |
| `push`, `pull` MCP tools                     | Deleted. `git push` / `git pull` replace these.     |
| `MergeEngine` trait + `NoOpMergeEngine`      | Deleted. Git merge replaces for NDJSON conflicts.   |
| `khive-vcs` crate                            | Reduced to `export`, `import`, `validate`, `diff`.  |
| `KgArchive` in `portability.rs`              | Preserved as the in-memory representation.          |
| `RemoteConfig` + `.khive/remotes.toml`       | Replaced by `schema.yaml#remotes`.                  |

#### ADR-043 scope reduction

ADR-043's three-way merge algorithm is replaced by git's line-level merge on sorted NDJSON files
for the common case. The remaining role for ADR-043-style logic is:

- **Merge conflict resolution**: when git cannot auto-merge (two branches modified the same entity
  line), `khive kg validate --conflicts` parses the conflict markers in the NDJSON file and
  renders them in entity-aware terms (which fields on which entities conflict). The agent then
  edits the live database, re-exports, and commits. This is narrower than ADR-043's full
  three-archive merge engine.
- **Post-merge validation**: after `git merge` produces a clean NDJSON merge, `khive kg validate`
  checks that the merged state is semantically valid (no dangling edges, no duplicate UUIDs
  introduced by the merge). This is a validation pass, not a merge computation.

`khive-merge` as a separate crate is no longer needed. The merge pass that remains fits in
`khive-vcs` alongside the other CLI commands.

## Rationale

### Why file-based serialization delegates to git

The core argument is that the versioning problem is solved. Git provides content-addressed commits,
branch pointers, three-way merge for line-addressable files, push/pull over HTTPS, and a
widely-deployed social layer in GitHub. Reimplementing these in Rust atop SQLite means years of
work to reach equivalence, a bespoke UX that researchers must learn, and a closed ecosystem that
cannot integrate with existing tooling (CI runners, code review tools, GitHub Actions).

NDJSON with sorted keys is line-addressable in a way that JSON or binary formats are not. Git's
line-level diff and merge are exact-fit primitives for a sorted NDJSON file: two researchers each
adding distinct entities to different UUID positions in the sorted file will merge cleanly with
zero conflicts. The merge algorithm that ADR-043 specified in detail is, for the non-conflicting
case, delegated entirely to git.

### Why NDJSON over alternative serialization formats

| Format         | Diff quality         | Merge quality           | Human readability | Why not chosen                           |
| -------------- | -------------------- | ----------------------- | ----------------- | ---------------------------------------- |
| NDJSON sorted  | Line-per-entity, clean | Non-overlapping = clean | Good              | **Chosen**                               |
| JSON (one blob)| Entire file changes  | Always conflict         | Good              | Merge unusable; one change = full diff   |
| RDF/Turtle     | Semantic, line-level | Clean for additions     | Poor              | Complex parser; non-standard tooling     |
| Parquet/binary | Not diffable         | Not mergeable           | None              | Entirely wrong abstraction               |
| SQLite file    | Not diffable by git  | Binary merge always fails | None             | The current status quo — what we escape  |
| CSV            | Line-per-row         | Same as NDJSON          | Limited           | No type system; properties not expressible |

### Why sorted by UUID rather than by name or creation time

UUID sort is deterministic across all implementations. Name sort breaks when an entity is renamed
(the line moves, making diff noisy). Creation-time sort is non-stable across imports. UUID sort
also produces the most stable diffs for concurrent contributors: two agents adding entities in
separate sessions will insert at deterministic non-overlapping positions regardless of the order
they committed.

### Why `schema.yaml` is a separate file from the NDJSON data

Ontology amendments (adding a new entity kind, adding an endpoint rule) are a different class of
change from data additions. A PR that adds 50 new concept entities should not visually conflict
with a PR that adds a new allowed property key to `schema.yaml`. Separating the files means
ontology changes and data changes have independent diff surfaces and can be reviewed by different
people with different expertise.

### Why cross-repo references use a `<remote>:<uuid>` syntax rather than a full URL

Full URLs in edge records couple the graph data to a specific hosting location. If a repo moves
from `github.com/old-org/lattice` to `github.com/new-org/lattice`, every edge referencing it would
need a data migration. The `remote:` prefix is a stable logical name that is resolved through
`schema.yaml`, which is the single place where the URL and commit SHA are recorded. Moving a
remote requires changing one or two lines in `schema.yaml`, not thousands of edge records.

### Why commit SHAs rather than tags or branch names as the pin

Tags in git are mutable by default (`git tag -f` moves them). An `annotated` tag is harder to
move, but the tooling cannot distinguish annotated from lightweight tags without an extra API call.
Branch names are explicitly mutable. The only reference type that is content-addressed and
immutable is a commit SHA. A commit SHA is a cryptographic commitment: if it resolves at all, it
resolves to exactly the same tree it always has.

Using SHAs as the stored pin also enables exact reproducibility across time: a project's git log
records the `schema.yaml` at every commit, and therefore records the exact remote SHA each edge
was valid against when it was written. Any future consumer can reconstruct the exact knowledge
state of the project at any historical commit, including all cross-repo references. This property
is impossible with mutable tag or branch pins.

### Why sparse checkout / GitHub API for remote resolution rather than full clone

Full cloning a remote repo to validate a single UUID lookup is unnecessary and slow. The
`.khive/kg/entities.ndjson` file is the only artifact needed. Sparse checkout fetches exactly
that file. For public repos, the GitHub Contents API achieves the same result without git at all.
Both approaches are bounded in bandwidth regardless of the size of the remote repository.

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
| --- | --- | --- | --- |
| Keep ADR-042 custom VCS (snapshots + `khive-sync`) | Full control; works offline without git installed | Months of work to reach git parity; closed ecosystem; foreign UX for researchers | Duplicates solved problems; wrong abstraction level |
| JSON blob (one file for all entities) | Single file to commit | Every entity change diffs as a full-file rewrite; merge always conflicts | Merge unusable |
| RDF serialization (Turtle, N-Triples) | Semantic web interop; line-addressable | Non-standard tooling; complex parser; no typed property model | Complexity without benefit for the target use case |
| Dolt (MySQL-compatible versioned relational DB) | Versioned SQL tables; git-like CLI | Binary storage; no NDJSON; no GitHub PR review without DoltHub; adds a runtime dependency | Not git-native; additional infrastructure dependency |
| TerminusDB / TDB2 | Specialized graph VCS | Heavy infrastructure; not embeddable; foreign data model | Wrong abstraction for an embeddable Rust KG library |
| CRDT-based automatic merge | No conflicts; always produces a result | Semantic contradictions silently accepted; ADR-010 explicitly rejected CRDTs | Safety requirement: silent corruption is worse than a paused merge |
| Store snapshot archives in git LFS | Full-fidelity snapshots; git history | LFS is not universally available; binary blobs; same diff problem as JSON blob | NDJSON file is better diffable and does not require LFS |

### Comparison to Existing Tools

**Neo4j**: A mature property graph database with Cypher query language and APOC procedures. Not
git-native — state lives in a binary data directory that is not diffable or mergeable. Requires a
running server; no offline-first use. Sharing a graph requires replication tooling or dump/restore
workflows. No text-diff collaboration or GitHub PR review of graph changes.

**Wikidata**: A centralized, publicly editable knowledge base using RDF and the SPARQL/Wikidata
Query Service. Operates as a global singleton — not per-project or per-repository. Data is
RDF-triples, not typed entities with property schemas. No offline mode, no per-project ontology,
no git-native history. Collaboration is centralized editorial rather than git-forking.

**git-annex / DVC**: Tools for tracking large files in git using content-addressed pointers.
Designed for binary blobs (datasets, model weights) rather than structured records. Cannot
diff or merge individual entities; a changed entity file is a changed blob. Not applicable to
structured KG data that requires entity-level diff and merge.

**Dolt / TerminusDB**: Versioned relational databases with git-like branch/merge semantics. Dolt
uses a MySQL-compatible interface over a versioned B-tree; TerminusDB uses a graph-oriented
model. Neither produces human-readable text diffs in GitHub PRs — branches and commits are
database-internal, not file-based. Both add a significant runtime dependency (Dolt server,
TerminusDB server). The NDJSON approach requires only git, which is already universally installed
in development environments.

## Consequences

### Positive

- Git history is the commit log. `git log --oneline .khive/kg/` shows every change to the graph,
  who made it, when, and with what message. No custom `log` command needed.
- GitHub PRs become the review workflow for graph changes. Reviewers see entity-level diffs in the
  PR interface. Approval, merge, and rollback use standard GitHub tooling.
- Cross-instance collaboration requires no khive infrastructure on the remote side. A researcher
  sharing a KG forks the GitHub repo. The consumer runs `git clone` and `khive kg import`.
- CI validation runs on every PR through standard GitHub Actions, with no custom CI plugin.
- `khive-vcs` crate scope reduces significantly. `commit`, `branch`, `checkout`, `merge_branch`,
  `log`, `push`, `pull` operations and the `khive-sync` HTTP server are deleted.
- `khive-merge` as a separate crate is not needed. The merge-conflict surface reduces to a
  validation pass on post-merge NDJSON files.
- The export format is a stable interchange format. Any tool that understands NDJSON and the
  schema can consume a khive KG without the Rust runtime.

### Negative

- Git must be installed and on `$PATH` for `khive kg import` / `export` / `validate` to work. This
  is a new runtime dependency that the current SQLite-only binary does not have. Mitigation: git is
  nearly universally installed in development environments; document it as a prerequisite.
- Two branches that modify the same entity line produce a git merge conflict in the NDJSON file.
  The conflict markers are raw JSON, not human-readable diff output. Mitigation: `khive kg
  validate --conflicts` renders them in entity-aware terms. This is a narrower failure mode than
  ADR-042/043's design, not a broader one.
- Remote resolution requires network access during `validate` and `import`. Mitigation: the
  `.remote-cache/` directory avoids repeated fetches for pinned refs.
- The file-based format is not suitable for real-time multi-agent writes within a single session.
  During active agent work, the live SQLite database remains the authoritative state; NDJSON is a
  serialization format for commits, not a live write surface. Mitigation: the ADR explicitly scopes
  NDJSON to export/commit, not as a live database replacement.
- Soft-deleted entities do not appear in the export. A researcher who deletes an entity by
  mistake and then commits loses the entity from the git-tracked KG (though the soft-deleted record
  remains in the local SQLite DB until hard-deleted). Mitigation: document that `export` captures
  live state; deleted entities can be recovered from the local DB via `list(include_deleted=true)`
  before they are hard-deleted.

### Neutral

- The `KgArchive` type in `portability.rs` is unchanged. Export serializes it to NDJSON;
  import deserializes from NDJSON. The in-memory representation is stable.
- `schema.yaml` format versioning uses semver via the `format_version` field. The current format
  is `1.0.0`. Format upgrades (adding top-level keys) increment the minor version; breaking changes
  increment the major version. The khive CLI checks `format_version` and rejects schemas with a
  major version it does not understand. Ontology evolution (entity kinds, relations, property
  schemas) is tracked via `ontology_version` defined in ADR-054.
- Existing khive instances with SQLite-only state are not affected by this ADR. The NDJSON export
  is an additive workflow, not a database replacement.

## Implementation

### Crate changes

- `crates/khive-vcs/` scope reduces to:
  ```
  crates/khive-vcs/
  ├── Cargo.toml
  └── src/
      ├── lib.rs          — re-exports
      ├── schema.rs       — SchemaYaml type; parse, validate, version check
      ├── export.rs       — export(): KhiveRuntime → entities.ndjson + edges.ndjson
      ├── import.rs       — import(): ndjson files + remote resolution → KhiveRuntime
      ├── validate.rs     — validate(): integrity checks against schema + ndjson content
      ├── diff.rs         — diff(): entity-aware rendering of git unified diff output
      ├── remote.rs       — RemoteResolver: sparse checkout + GitHub API fallback + cache
      └── update.rs       — update_remote(): bump schema.yaml ref + re-validate
  ```

- `crates/khive-merge/` is not created (superseded before implementation).
- `kg_snapshots`, `kg_snapshot_archives`, `kg_branches`, `kg_vcs_state` SQL tables from ADR-042
  are not created. No new migrations are needed.

### MCP surface

No new MCP tools. The `khive kg` subcommands (`init`, `export`, `import`, `validate`, `diff`,
`update`) are CLI commands in the Deno CLI (`deno/src/kg/`), not MCP tools. The MCP server surface (ADR-027)
is unchanged. Git operations are not surfaced through MCP.

### Schema format

`schema.yaml` is validated by `khive-vcs` against a built-in JSON Schema on every `validate`
call. The JSON Schema is embedded in the binary via `include_str!` from
`crates/khive-vcs/src/schema/v1.json`. This ensures that an out-of-date `schema.yaml` with a
missing required key produces a structured error, not a silent parse failure.

### Remote cache

The remote cache at `.khive/kg/.remote-cache/` uses filenames of the form
`<remote>-<sha>.ndjson` where `<sha>` is the full 40-character commit SHA from `schema.yaml`.
This directory should be added to `.gitignore` — it is a local cache, not part of the committed
KG. Because `schema.yaml` always stores a full commit SHA, cache entries are permanently valid
and do not require expiration logic.

`khive kg init` appends `.khive/kg/.remote-cache/` to `.gitignore` automatically.

### Phasing

| Phase | Scope | Target |
| ----- | ----- | ------ |
| 1 | `schema.rs` + `export.rs` + `import.rs` (no remote resolution, no CI workflow) | v0.4 |
| 2 | `validate.rs` (schema compliance + referential integrity + sort check) | v0.4 |
| 3 | `remote.rs` (sparse checkout + GitHub API + cache) + cross-repo reference support | v0.5 |
| 4 | `diff.rs` (entity-aware diff rendering) + `update.rs` (remote ref bump) | v0.5 |
| 5 | CI workflow generation in `khive kg init` | v0.5 |

Phase 1 and 2 are independently shippable and cover the core use case: export from SQLite, commit
to git, import from git on another machine.

### Frontend: namespace-aware KG explorer

The web frontend must reflect the namespace structure introduced by git-native versioning. A flat
entity dump does not serve the "GitHub for knowledge graphs" positioning — users need to navigate
between repos, see cross-repo connections, and understand provenance.

#### Namespace picker

The top of the KG Explorer is a namespace selector, analogous to GitHub's repo dropdown. Each
namespace corresponds to one repo's `.khive/kg/`:

- **Local namespace** — the current repo's entities and edges (always present).
- **Remote namespaces** — loaded from `schema.yaml` remotes via `khive kg import`. Each remote
  becomes a read-only namespace in the local DB. The namespace name matches the remote key in
  `schema.yaml` (e.g., `lattice`, `styx`).

The selector shows entity count per namespace and the pinned commit SHA for remotes.

#### Per-namespace view

Within a namespace, the explorer shows:

1. **Entity browser** — entity list filtered to the current namespace, with kind filter badges and
   search. Paginated (50 per page) with total count.
2. **Graph view** — force-directed graph of entities within the current namespace. Edges to remote
   entities render as dashed lines terminating at a "remote ref" node (shows the remote name and
   short entity ID). Clicking a remote ref switches to that namespace.
3. **Schema tab** — renders the namespace's `schema.yaml`: entity kinds, edge relations with
   endpoint rules, property schemas, and remote declarations. For remote namespaces, this is the
   remote repo's schema.
4. **Inspector panel** — entity detail view with inline property editing, edge CRUD, and provenance
   badge. Remote entities show a read-only badge: "from lattice @ a1b2c3d".

#### Cross-namespace graph (world view)

A dedicated "All Namespaces" toggle on the graph view shows a high-level cluster map:

- Each namespace renders as a cluster (convex hull or bounding box around its entities).
- Cross-repo edges (those with `<remote>:uuid` targets) render as inter-cluster links with
  the remote prefix as label.
- Clicking a cluster enters that namespace's per-namespace view.
- This view is the "GitHub organization" level — it shows how your research repos connect.

#### Operations and permissions

- **Local namespace**: full CRUD — create/update/delete entities, add/remove edges, edit
  properties. All mutations go through the verb surface (`POST /api/request`).
- **Remote namespaces**: read-only in the frontend. To edit a remote's entities, you fork/clone
  that repo and edit there. The frontend shows a "View on GitHub" link using the `repo` field
  from `schema.yaml`.
- **Cross-repo edge creation**: creating an edge from a local entity to a remote entity writes
  a prefixed target (`"target": "lattice:uuid"`) into the local `edges.ndjson`. This is a local
  operation — the remote repo is not modified.

#### Data flow

```text
schema.yaml remotes
      ↓ khive kg import
Local DB (namespaced)
      ↓ Deno gateway (/api/entities?namespace=X)
Frontend namespace picker → per-namespace views
```

The Deno gateway gains a `namespace` query parameter on all entity/edge endpoints. The frontend
passes the selected namespace on every API call. Default is `local` (the current repo).

#### Phasing (frontend)

| Phase | Scope |
| ----- | ----- |
| F1 | Namespace picker + per-namespace entity list filter (requires `namespace` param on gateway) |
| F2 | Per-namespace graph view with dashed-line remote refs |
| F3 | Cross-namespace cluster map ("world view") |
| F4 | Schema tab rendering `schema.yaml` |
| F5 | "View on GitHub" links for remote entities + namespace provenance badges |

F1 can ship as soon as the `namespace` field is exposed on the gateway and `khive kg import` writes
namespaced entities. F2-F5 are incremental improvements.

## Design Decisions (resolved 2026-05-20)

### D1: Edge IDs in NDJSON — include them

**Decision**: Edge records include a `edge_id` UUID field.

```json
{"edge_id":"<uuid>","source":"<uuid>","target":"<uuid|remote_ref>","relation":"<EdgeRelation>","weight":<float>}
```

**Rationale**: Preserves edge identity across export/import cycles. Without `edge_id`, a round-trip
(export → modify file → import) loses the ability to correlate an imported edge with its source.
Edge-level operations (delete specific edge, update weight) require a stable identifier. The sort
key remains `source+target+relation` for diffability; `edge_id` is carried but not used for
ordering.

### D2: Schema evolution — strict validate, permissive import

**Decision**: `khive kg validate` rejects entities/edges that don't match `schema.yaml` (unknown
kinds, illegal endpoint pairs, unrecognized properties). `khive kg import` defaults to strict mode
but accepts `--force` to skip schema validation. This allows importing a KG from a repo with a
different ontology while still catching errors in the normal workflow.

**Rationale**: Strict-by-default catches real mistakes (typo in kind name, wrong endpoint pair).
`--force` is the escape hatch for cross-ontology imports where the target schema is intentionally
different from the source.

### D3: Namespace naming — explicit in settings, defaults to repo name

**Decision**: The namespace is set in `.khive/settings.json` under `actor.name`. Default value
when `khive kg init` runs: the git repo name (`basename $(git rev-parse --show-toplevel)`). The
namespace is a simple string, not hierarchical — no colons, slashes, or dots. Validated against
`^[a-z0-9][a-z0-9_-]{0,62}[a-z0-9]$`.

**Rationale**: Explicit config avoids ambiguity when a repo is cloned to a different directory name.
The regex ensures namespace strings are safe as SQL identifiers, filesystem paths, URL segments,
and NDJSON field values without escaping.

### D4: Existing VCS tables — drop via migration

**Decision**: A schema migration (next sequential version) drops `kg_snapshots`,
`kg_snapshot_archives`, `kg_branches`, and `kg_vcs_state` tables. These were introduced by ADR-042
but never shipped in a release (v0.1.4 does not use them). The migration is unconditional (not
gated behind a feature flag).

**Rationale**: Keeping unused tables creates confusion and maintenance cost. Since no released
version populates them, the migration has zero data-loss risk.

### D5: NDJSON edge sort key — source+target+relation

**Decision**: Edges in `edges.ndjson` are sorted by `(source, target, relation)` lexicographically.
This groups all edges from entity A together, then within those all edges to entity B, then by
relation type.

**Rationale**: This ordering produces readable diffs — when reviewing a PR that adds edges from a
new entity, all its edges appear as a contiguous block of added lines. Sorting by `edge_id` (UUID)
would scatter related edges randomly through the file.

### D6: Remote fetch — GitHub API for cloud, sparse checkout for CLI

**Decision**: Two fetch backends, selected by environment:

- **CLI (`khive kg import --resolve-remotes`)**: Uses `git sparse-checkout` to fetch only the
  `.khive/kg/` directory from the remote repo at the pinned commit. Requires git on PATH. Cached
  in `.khive/kg/.remote-cache/<remote>-<sha>/`.
- **Cloud (khive.ai hosted)**: Uses GitHub Contents API (`GET /repos/:owner/:repo/contents/.khive/kg/entities.ndjson?ref=<sha>`)
  with OAuth token. No git binary required on the server.

Both backends produce the same output: a local copy of the remote's `entities.ndjson` +
`edges.ndjson` + `schema.yaml` at the pinned commit. The `import.rs` code is backend-agnostic —
it reads files from a path, regardless of how they got there.

### D7: Namespace as explicit verb parameter

**Decision**: All MCP verbs accept an optional `namespace` parameter. When present, it overrides
the actor's default namespace for that call. The gateway passes the frontend's `?namespace=X`
query parameter through as this verb parameter.

```text
search(type="entity", kind="concept", query="LoRA", namespace="lattice")
```

**Rationale**: Cloud serves multiple projects through a single gateway process. Per-process
namespace isolation (one khive-mcp per namespace) is too heavy. Per-call namespace is stateless,
composable with batch requests, and consistent with the existing `namespace` column in the DB.

The verb-level namespace param is enforced in `khive-runtime` — it validates that the caller has
access to the requested namespace (in OSS: always allowed; in cloud: checked against API key
scope from ADR-029's Gate trait).

## References

- ADR-010: KG Versioning Direction (strategic context; "GitHub for knowledge graphs" positioning)
- ADR-042: KG Versioning Implementation (superseded implementation approach; this ADR replaces it)
- ADR-043: KG Merge Algorithm (substantially reduced in scope by this ADR; not yet implemented)
- ADR-039: Bulk Import Adapters (import conflict modes `error`/`skip`/`update` reused here)
- ADR-002: Closed Edge Ontology (edge relations validated by `khive kg validate`)
- ADR-001: Entity Kind Taxonomy (entity kinds validated by `khive kg validate`)
- ADR-014: Curation Operations (live DB operations that produce the state exported to NDJSON)
- ADR-022: Schema Migrations (no new migrations required by this ADR)
- `crates/khive-runtime/src/portability.rs` — `KgArchive` type reused as export/import target
- NDJSON specification: https://ndjson.org/
- git sparse-checkout documentation: https://git-scm.com/docs/git-sparse-checkout
- GitHub Contents API: https://docs.github.com/en/rest/repos/contents
