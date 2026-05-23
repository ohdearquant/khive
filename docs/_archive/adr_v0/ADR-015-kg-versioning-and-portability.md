# ADR-015: KG Versioning Model

**Status**: planned (implementation deferred to v0.4+)\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

ADR-010 established the "GitHub for knowledge graphs" strategic direction and named the primitives
(Snapshot, Branch, Diff, Merge, Remote) without deciding their concrete shape. ADR-014 defined the
curation surface (entity/edge CRUD, merge). This ADR designs the two remaining open questions:

1. **Import/export capabilities**: The v0.1 JSON archive is implemented in `portability.rs`. What
   are the next-level capabilities: partial exports, notes/events inclusion, streaming for large
   KGs?
2. **Versioning model**: Concrete answers to commit/branch/merge/checkout semantics — the "GitHub
   for KGs" vision made operational.

The Rust core is mostly built. Storage capability traits support the substrate. The curation surface
is specced in ADR-014. The agent-facing tool naming is defined in ADR-023 (verb-consolidated
surface). What's missing is the commit/branch/merge layer and the portability extension plan.

### Prerequisite

This ADR assumes the curation surface defined in ADR-014 is in place: `update_entity`,
`merge_entity`, `get_edge`, `update_edge`, `delete_edge`, `list_edges`. The portability operations
(`export_kg`, `import_kg`) are introduced by this ADR.

## Decision

Agent-facing tool naming is defined in ADR-023 (verb-consolidated MCP surface). The versioning
operations in this ADR follow that convention.

### Part C — Import/export capabilities (v1 floor + v0.2 extensions)

The v1 format is implemented: `KgArchive {format, version, namespace, exported_at, entities[], edges[]}`.
No embeddings (regenerable). This ADR adds `export_kg` and `import_kg` to the MCP surface. This
part designs what comes next.

#### C.1 Notes inclusion

**Decision: notes are not included in the default export. An `include_notes: true` flag enables
opt-in export. Events are never exported.**

Notes (memories, decisions, observations) are agent working memory. They are:

- Subjective: they record what an agent observed, not facts about the world.
- Session-local: a "decision to use FlashAttention-2" note is meaningful to the agent that wrote it,
  not to a collaborator receiving an exported archive.
- Ephemeral: notes have decay factors and salience scores that only make sense in their original
  context.

When a researcher shares a KG archive with a collaborator, they want to share the structural
knowledge (entities + edges). They do not want to share their private research notes. Default must
protect privacy.

The `include_notes` opt-in exists for one valid use case: an agent wants to checkpoint its full
working state (graph + observations) for later restoration or handoff to another agent in the same
project. This is a local workflow, not a sharing workflow.

Events are the immutable audit log. They are per-instance artifacts (what happened on _this_ khive
instance), not knowledge artifacts. Exporting events would create a false impression that replay is
meaningful on the target instance. Events are never exported.

#### C.2 Streaming / chunked format for large KGs

**Decision: no streaming in v0.1. NDJSON streaming via a `format: "khive-kg-stream"` variant is the
designated v0.2 extension path.**

At v0.1 scale (research KGs built by individual researchers or small agent teams), the expected
namespace size is 10K–100K entities. At a generous 600 bytes per entity in JSON, 100K entities = 60
MB. This is too large for an inline MCP response but easy to write to a file.

The v0.1 approach: `export_kg` accepts an optional `output_path` parameter. When provided, the
archive is written to the file path and the tool returns a summary (`{entities, edges, path}`)
rather than the archive inline. When absent, the archive is returned inline (suitable for KGs under
~10K entities / ~5 MB response).

The v0.2 streaming design: newline-delimited JSON (NDJSON), one JSON object per line, with a header
line:

```
{"format":"khive-kg-stream","version":"0.2","namespace":"local","exported_at":"2026-05-15T..."}
{"type":"entity","id":"...","kind":"concept","name":"FlashAttention",...}
{"type":"entity","id":"...","kind":"document",...}
{"type":"edge","source":"...","target":"...","relation":"extends","weight":1.0}
```

Parsers detect `"format": "khive-kg-stream"` and read the file line by line. v0.1 parsers see an
unknown format and return a clear error. Forward compatibility is achieved via the `format` field.

This design is registered here so the v0.1 `format: "khive-kg"` string choice and the `version`
field are set with deliberate forward intent, not just because they seemed like a good idea.

#### C.3 Partial export (subgraph by selection)

**Decision: full namespace export only in v0.1. Subgraph export (bounded by traversal from roots) is
the designated v0.2 extension.**

Partial export has a natural implementation: run `traverse(roots, max_depth, ...)` to collect
reachable entity IDs, then export only those entities and the edges between them. The traversal
primitives already exist. The work is plumbing, not design.

The v0.2 `export_kg` signature extension:

```
export_kg({
  namespace: string,
  roots: UUID[] (optional) — if provided, export only the subgraph reachable from these roots
  max_depth: integer (optional, default: unlimited when roots not specified)
  relations: string[] (optional) — filter traversal to specific relation types
  include_notes: boolean (optional, default false)
})
```

When `roots` is absent, the existing behavior (full namespace export) is preserved.

---

### Part D — Versioning model

This is the "GitHub for KGs" vision made concrete. The design answers five questions in order,
because each answer constrains the next.

#### D.1 What is a commit?

**Decision: a commit is a full snapshot of all entities and edges in a namespace at a point in time,
identified by a content hash. It is NOT a delta.**

A delta (operation log) would record: "entity X was created with properties P, then edge Y was
added." A snapshot records: "the namespace contains entity X with properties P and edge Y."

For v0.1, snapshots are correct. The reasons:

1. **Reconstruction is O(1)**: any commit can be checked out by restoring the snapshot directly — no
   need to replay an operation chain. Delta storage requires replaying potentially thousands of
   operations to reconstruct state at commit N.

2. **Merge is O(N) either way**: a three-way merge requires reading two branch heads and their
   common ancestor. Whether those are stored as snapshots or as deltas, the merge logic reads all
   three full states. Snapshots don't make merge harder.

3. **Storage cost at v0.1 scale is acceptable**: a 10K-entity namespace at 600 bytes/entity = 6 MB
   per snapshot. With 100 commits, that's 600 MB — uncomfortable for a local single-user instance.
   The mitigation is content-addressing: entities and edges that didn't change between commits share
   their storage. See the content-addressing note below.

4. **Delta storage adds reconstruction complexity that is not justified before reaching v0.4**: The
   v0.4 ship target for the `khive-diff` crate (ADR-010) is when delta storage becomes interesting.
   Until then, snapshots are the right abstraction.

**Content-addressing**: A snapshot's ID is computed as the SHA-256 of the sorted canonical JSON of
all entities and edges. Two snapshots with the same content have the same ID — deduplication is
free. In practice, most commits change a small fraction of the namespace, so the hash will differ
(no dedup at the snapshot level), but the content-addressing property enables future object-store
de-duplication without changing the snapshot ID contract.

**Storage model**: Snapshots are stored in a new `kg_snapshots` table with columns:
`(id TEXT, namespace TEXT, parent_id TEXT, message TEXT, author TEXT, created_at INTEGER, archive_json TEXT)`.
The `archive_json` column stores the `KgArchive` serialization — the same format as export. This is
intentional: commit/checkout and export/import share the same serialization layer. A commit is an
export with a parent pointer and a message.

#### D.2 What is a branch?

**Decision: a branch is a named pointer to a snapshot, stored in a `kg_branches` table. Branch HEAD
is mutable; snapshots are immutable.**

```
kg_branches table:
  name TEXT PRIMARY KEY
  namespace TEXT
  head_snapshot_id TEXT (FK → kg_snapshots.id)
  created_at INTEGER
  updated_at INTEGER
```

The default branch is `main`. Branches are namespace-scoped: `local/llm-research` has its own `main`
branch separate from `local/optical-flow`'s `main`.

**Branch isolation model**: A branch is NOT a separate namespace. It is a pointer to a snapshot
within a namespace. The "working tree" of a branch is the current live namespace state, which the
agent mutates freely with curation operations. A commit snapshots the current live state and
advances the branch HEAD.

This is analogous to git's model: the working directory is not "on a branch" in a strict storage
sense — it is the uncommitted working state. The branch is the pointer that tracks where to create
the next commit.

The implication: two branches in the same namespace share the live working state. An agent on
`experimental` and `main` both see and mutate the same live entities. This is weaker isolation than
git's working tree checkout, but it is the correct v0.1 model because:

1. KG namespaces are not cheap to fork. Creating a copy of 50K entities for every experiment branch
   would be prohibitively expensive at v0.1.
2. The primary use case is sequential: an agent works on `main`, creates `experimental`, does some
   work, merges back. Concurrent multi-branch editing on the same namespace is a v0.2 concern
   (requiring namespace forks or copy-on-write).

**Full branch isolation (namespace fork)** is the v0.2 model: `branch(name, fork=true)` creates a
new child namespace (`local/llm-research/experimental`) as a copy of the current namespace state,
allowing fully isolated editing. The parent/child namespace relation (ADR-007's `is_child_of`) is
the natural anchor for this.

#### D.3 What is a merge?

**Decision: three-way merge with explicit conflict reporting. Auto-merge for non-conflicting
changes; structured conflict report for conflicts. The agent resolves conflicts manually before the
merge is finalized.**

Three-way merge requires three inputs: base (common ancestor), ours (current branch HEAD), theirs
(the branch being merged). The merge result is either a clean new snapshot or a conflict report the
agent must resolve.

**Conflict taxonomy** (from ADR-010 analysis):

1. **Added-added (no conflict)**: both branches added different entities / edges. Merge takes both.
   This is the common case.

2. **Modified-modified (property conflict)**: both branches modified the same property on the same
   entity to different values. Reported as a conflict: `{entity_id, key, ours, theirs}`.

3. **Deleted-modified (existence conflict)**: one branch deleted an entity that the other branch
   modified. Reported as: `{entity_id, deleted_in: "ours"|"theirs", modified_in: "ours"|"theirs"}`.

4. **Edge-endpoint conflict (dangling reference)**: a merge would produce an edge pointing to an
   entity that was deleted. Always reported as a conflict.

5. **Semantic contradiction** (future): both branches added edges that contradict each other (e.g.,
   `A extends B` and `A supersedes B` from different branches). This requires semantic analysis
   beyond structural merge — deferred to the `khive-merge` crate (ADR-010 v0.5).

**Auto-merge rules** (no agent intervention needed):

- An entity or edge present in `theirs` but not in `base` is added to the merge result.
- An entity or edge deleted in `theirs` (soft-deleted) but unchanged in `ours` is deleted in the
  merge result.
- Property keys present in `theirs` but not modified in `ours` are taken from `theirs`.
- Tags are unioned.

**Conflict resolution**: When conflicts exist, `merge_branch` returns the full `MergeConflicts`
object (list of structured conflicts, each with entity/edge IDs and both versions). The agent
inspects the conflicts, calls `update(kind="entity")` / `update(kind="edge")` /
`delete(kind="entity")` to resolve them on the working state, then calls `merge_branch` again with
`force=true` to finalize (skipping conflict detection, treating the current working state as the
resolved merge result).

This is deliberate: the agent, not the system, decides semantic questions like "which year is
correct" or "should this edge be kept or dropped." The system provides structure; the agent provides
judgment.

#### D.4 Identity preservation

**Decision: entity identity is the UUID. Two entities with the same UUID in different branches are
the same entity (modifications to a shared entity). Two entities with different UUIDs that happen to
have the same name are candidates for `merge(kind="entity")`, not the same entity.**

This has one important implication: if an agent in branch `experimental` creates a new entity (UUID
never seen in `main`), and `main` happens to also have an entity with the same name (added by a
different agent since the branch diverged), the merge will NOT auto-merge them. They arrive as two
entities in the merge result. The agent must notice the name collision and issue
`merge(kind="entity")` to deduplicate.

This is correct behavior: name equality does not imply semantic equality in a KG (two entities named
"Adam" might be the optimizer and the person). The system cannot resolve identity — the agent can.
The curation surface (`merge(kind="entity")`, plus batched `create` via `request`) gives agents the
tools to perform this resolution.

The alternative — "match by name on merge" — would silently merge entities with coincidental name
overlap and corrupt the graph. Name-based identity matching belongs in a deduplication heuristic,
not in core merge semantics.

#### D.5 MCP shape for versioning tools

**Decision: `commit`, `branch`, `checkout`, `merge_branch`, `log`, `diff`, `apply_diff`.**

These seven tools cover the primary agent versioning loop. The names follow ADR-023's
verb-consolidated surface (versioning operations are domain-specific verbs that do not need a
`kind=` discriminant — they always operate on whole-namespace state). `apply_diff` is defined in
ADR-017 §7 and listed here for the unified versioning tool roster.

---

**`commit`** — Snapshot the current namespace state and advance the current branch HEAD.

```
Parameters:
  namespace: string (optional, default "local")
  message: string (required) — human-readable description of the changes
  author: string (optional) — agent or user identifier
```

Returns the new `Snapshot {id, parent_id, message, author, created_at, entity_count, edge_count}`.

---

**`branch`** — Create a new branch pointing to the current HEAD snapshot.

```
Parameters:
  namespace: string (optional)
  name: string (required) — branch name (alphanumeric, hyphens, underscores)
  from_snapshot: string (optional) — snapshot ID to branch from (default: current HEAD)
  fork: boolean (optional, default false) — v0.2: create a child namespace fork for full isolation
```

Returns the new `Branch {name, namespace, head_snapshot_id, created_at}`.

---

**`checkout`** — Restore the namespace state to a specific snapshot. Destructive: replaces the
current working state with the snapshot contents.

```
Parameters:
  namespace: string (optional)
  snapshot_id: string (optional) — snapshot to restore (conflicts with branch_name)
  branch_name: string (optional) — checkout the HEAD of this branch
  force: boolean (optional, default false) — allow checkout with uncommitted changes
```

Returns `{branch_name, snapshot_id, entities_restored, edges_restored}`.

**Safety note**: `checkout` replaces the live working state. If the agent has uncommitted changes
(curation operations since the last `commit`), those changes are lost unless `force=false`
(default), which causes `checkout` to return an error listing the uncommitted change count. The
agent must either commit first or pass `force=true` to discard.

---

**`merge_branch`** — Merge another branch (or snapshot) into the current working state.

```
Parameters:
  namespace: string (optional)
  theirs: string (required) — branch name or snapshot ID to merge from
  strategy: "auto" | "ours" | "theirs" (optional, default "auto")
  force: boolean (optional, default false) — finalize merge ignoring remaining conflicts
  message: string (optional) — commit message for the merge commit (auto-generated if absent)
```

Returns either:

- `MergeResult {status: "clean", snapshot_id, entities_merged, edges_merged}` — auto-merge
  succeeded, a new commit was created.
- `MergeResult {status: "conflicts", conflicts: Conflict[]}` — conflicts exist; no commit created.
  Agent must resolve, then call `merge_branch` again with `force=true`.

---

**`log`** — List commits on a branch, most recent first.

```
Parameters:
  namespace: string (optional)
  branch_name: string (optional, default "main")
  limit: integer (optional, default 20)
```

Returns `Snapshot[]` (without the full archive_json — just metadata).

---

**`diff`** — Compute the structural diff between two snapshots.

```
Parameters:
  namespace: string (optional)
  from_snapshot: string (required) — base snapshot ID
  to_snapshot: string (required) — head snapshot ID
```

Returns
`GraphDiff {added_entities[], removed_entities[], modified_entities[], added_edges[], removed_edges[]}`
(the structure from ADR-010).

Note: `diff` lands in v0.4 when `khive-diff` ships (ADR-010 phasing). The MCP tool signature is
defined here so the agent contract is stable; the implementation is deferred.

---

**Versioning tools added by this ADR**

| Tool                     | Category    | Status                                      |
| ------------------------ | ----------- | ------------------------------------------- |
| `commit`                 | Versioning  | planned (ADR-015)                           |
| `branch`                 | Versioning  | planned (ADR-015)                           |
| `checkout`               | Versioning  | planned (ADR-015)                           |
| `merge_branch`           | Versioning  | planned (ADR-015)                           |
| `log`                    | Versioning  | planned (ADR-015)                           |
| `diff`                   | Versioning  | planned (ADR-015, impl deferred to v0.4)    |
| `apply_diff`             | Versioning  | planned (ADR-017 §7, impl deferred to v0.4) |
| `export_kg`, `import_kg` | Portability | planned (ADR-015)                           |

Curation and graph verbs (`create`, `get`, `list`, `update`, `delete`, `merge`, `supersede`, `link`,
`traverse`, `neighbors`, `query`, `search`, `request`) are defined in ADR-023.

## Rationale

### Why snapshots over deltas for v0.1?

The decision in D.1 is between two valid long-term options. Deltas are the right long-term answer
(git uses them; they're O(change) per commit vs O(N) for snapshots). But at v0.1 scale and
complexity budget, snapshots are correct because:

- Reconstruction is trivial (one SQL read of the archive_json column).
- The export/import implementation is already tested and working. Snapshots reuse it directly.
- A single researcher's KG committing daily for a year = ~365 snapshots × ~6 MB = ~2 GB. Painful but
  not blocking for a desktop tool. And content-addressing is a migration path to object-store with
  deduplicated entity blobs without changing the API.
- Delta reconstruction bugs are subtle and expensive to debug. Snapshots fail loudly and simply.

The delta model is the correct v0.4+ design once `khive-diff` ships and has a test suite.

### Why is branch isolation "pointer to snapshot" rather than "namespace fork"?

Namespace forks are the right model for true isolation (concurrent editors, experimental ontologies
that might invalidate the main graph). They are too expensive at v0.1 (copying 50K entities per
branch create). The pointer model gets agents the commit/branch/log operations that cover 90% of the
use case (sequential: work → commit → branch → experiment → merge) without the cost.

Namespace fork (`fork=true` parameter on `branch`) is reserved as the v0.2 upgrade for the 10% use
case.

### Why does `checkout` clobber the working state?

This is controversial but correct. The alternatives:

- **Refuse checkout if uncommitted changes exist** (default behavior, `force=false`): this is git's
  default and is right. It prevents accidental loss of work.
- **Stash changes before checkout**: git has this; it requires a stash object and a pop operation.
  This is v0.2 complexity. For v0.1, the agent must commit before checking out.

The `force=false` default means an agent working normally will never accidentally lose state. The
`force=true` escape hatch is for "I want to discard this branch's work" scenarios.

### Why does merge return conflicts rather than auto-resolving?

ADR-010 explicitly rejected CRDTs and auto-resolution for graph semantics. The reason: a "semantic
contradiction" conflict (two edges that together assert a falsehood about the world) cannot be
resolved by any local rule. An auto-resolver would silently accept semantic contradictions.

The `merge_branch` returning a structured conflict report puts the decision in the agent's context
window, where it can apply domain knowledge to choose. This is the correct human-in-the-loop design
even when the "human" is an AI agent.

## Alternatives Considered

| Alternative                                           | Pros                              | Cons                                                                          | Why rejected                                 |
| ----------------------------------------------------- | --------------------------------- | ----------------------------------------------------------------------------- | -------------------------------------------- |
| Delta-based versioning from v0.1                      | O(change) storage per commit      | Reconstruction complexity; subtle bugs; implementation not ready              | Defer to v0.4 with khive-diff                |
| Branch = separate namespace (fork) by default         | Full isolation                    | Copy cost O(N) per branch create; prohibitive at 50K+ entity scale            | Pointer model covers v0.1 use case           |
| Auto-resolve all merge conflicts with last-write-wins | Simple, no agent interruption     | Silently corrupts graph with semantic contradictions; violates ADR-010        | Wrong for research KG quality                |
| Include events in export                              | Full audit trail portable         | Events are instance artifacts, not knowledge; creates false replay assumption | Philosophically wrong for sharing            |
| NDJSON streaming in v0.1                              | Handles large KGs from the start  | Over-engineering for v0.1 scale; implementation cost unwarranted              | YAGNI; `output_path` parameter is sufficient |
| `diff` in v0.1                                        | Earlier feedback on graph changes | Requires khive-diff crate (ADR-010 v0.4 target); significant new crate        | Phasing constraint from ADR-010              |

## Consequences

### Positive

- Agents have a complete commit/branch/checkout/merge loop usable from v0.4 (when `diff` and
  `apply_diff` ship; the other 5 tools land earlier).
- Snapshot-based commits reuse the tested `portability.rs` serialization path — no new serialization
  format to maintain.
- Merge conflict reporting gives agents structured data to resolve conflicts with domain knowledge,
  not just a text diff.
- Versioning tools follow ADR-023's verb-consolidated naming, so the surface stays coherent with the
  curation and graph verbs.

### Negative

- Snapshot storage cost is O(N) per commit. For large namespaces (100K+ entities) committing
  frequently, storage grows quickly. Mitigated: content-addressing (same entity hash = no duplicate
  storage) is the v0.2 optimization path.
- Branch isolation is weak at v0.1 (pointer, not fork). Two branches in the same namespace share the
  live working state. Concurrent multi-agent editing on different branches will produce interleaved
  live state. Mitigated: this is expected v0.1 behavior; sequential single-agent use (the primary
  case) is unaffected.
- `diff` and `apply_diff` are defined here but implementation deferred to v0.4. Agents that call
  them before v0.4 will receive a "not implemented" error. This is acceptable: agents building on
  the versioning surface will use `log` and `merge_branch` first; `diff`/`apply_diff` are
  incremental value.
- The seven versioning tools join the verb-consolidated surface from ADR-023. The total surface is
  still much smaller than other MCP servers in the ecosystem.

### Neutral

- The snapshot `id` is a SHA-256 hash. This means commit IDs look like git commit hashes
  (`sha256:abc123...`). The analogy is intentional: it reinforces the "GitHub for KGs" positioning
  and makes the model immediately recognizable to developers.
- The `kg_branches` and `kg_snapshots` tables require a new `SchemaVersion` migration in `khive-db`.
  This is standard crate extension; no breaking change to existing tables.

## Open Questions

1. **Uncommitted-change detection for `checkout`**: How does the system know if there are
   uncommitted changes? This requires either tracking the "last committed snapshot" in application
   state and comparing it to the current live state (expensive for large namespaces), or maintaining
   a "dirty flag" that is set on every curation write and cleared on commit. The dirty-flag approach
   is simpler but requires all write paths to update it.

2. **Base snapshot for three-way merge**: The merge algorithm needs the common ancestor. The common
   ancestor is the most recent snapshot that appears in both branch histories. With the pointer
   model (branches share a snapshot history), finding the LCA requires a scan of the `kg_snapshots`
   parent chain. At 100-commit depth this is fast. At 10K commits this requires an index. Design the
   LCA query before v0.5 (`khive-merge` crate).

3. **Snapshot size threshold for `commit`**: Very large namespaces (100K+ entities) will produce
   multi-MB `archive_json` column values. SQLite handles this fine (it's just a blob), but query
   performance for `log` (which reads snapshot metadata without the full archive) requires that the
   archive_json be stored separately from the metadata columns — either as a separate row in a
   `kg_snapshot_archives` table or as a file path. Decide before shipping `commit` for production
   use.

4. **Branch names and namespace interaction**: With the pointer model, a branch name is scoped to a
   namespace. Can the same branch name exist in `local/project-a` and `local/project-b`? Yes —
   branch names are unique per namespace, not globally. This matches git's model. The composite
   primary key is `(namespace, branch_name)`.

5. **`merge_branch` and FTS5/vector index consistency**: After `checkout` restores a snapshot
   (replacing entities in the database), the FTS5 index and vector store will be stale. They need a
   rebuild. Should `checkout` trigger an async index rebuild automatically? Or return a warning
   indicating the index is stale? The answer affects search quality immediately after checkout.

## Implementation

### New crate additions

A new `khive-vcs` crate (or extension to `khive-runtime`) holds:

```
crates/khive-vcs/src/
├── snapshot.rs   // KgSnapshot struct + content-hash computation
├── branch.rs     // KgBranch struct + branch CRUD
├── commit.rs     // commit() operation: snapshot current state + advance HEAD
├── checkout.rs   // checkout() operation: restore snapshot to live state
├── merge.rs      // three-way merge: LCA + diff + conflict detection
└── diff.rs       // GraphDiff (stub for v0.4 — returns NotImplemented before khive-diff ships)
```

Alternatively, these operations can live in `khive-runtime/src/` as additional operations files
(following the existing `operations.rs` + `portability.rs` pattern) without introducing a new crate.
Prefer the no-new-crate approach for v0.1 unless the VCS operations grow beyond ~500 LOC.

### New SQL tables (via `ServiceSchemaPlan` migration in `khive-db`)

```sql
CREATE TABLE kg_snapshots (
    id          TEXT PRIMARY KEY,           -- SHA-256 hash, prefixed "sha256:"
    namespace   TEXT NOT NULL,
    parent_id   TEXT REFERENCES kg_snapshots(id),
    message     TEXT NOT NULL,
    author      TEXT,
    created_at  INTEGER NOT NULL,           -- Unix microseconds
    entity_count INTEGER NOT NULL DEFAULT 0,
    edge_count  INTEGER NOT NULL DEFAULT 0,
    archive_id  TEXT NOT NULL               -- FK to kg_snapshot_archives for large archive separation
);

CREATE TABLE kg_snapshot_archives (
    id          TEXT PRIMARY KEY,           -- same as snapshot id
    archive_json TEXT NOT NULL              -- KgArchive serialization
);

CREATE TABLE kg_branches (
    name        TEXT NOT NULL,
    namespace   TEXT NOT NULL,
    head_id     TEXT NOT NULL REFERENCES kg_snapshots(id),
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (namespace, name)
);

CREATE INDEX idx_snapshots_namespace_created ON kg_snapshots(namespace, created_at DESC);
```

The `kg_snapshots` / `kg_snapshot_archives` split keeps the `log` query fast (reads only metadata)
while allowing large archives in the same database.

### MCP tool additions (in `khive-mcp/src/tools/version.rs`)

Six new tools following the existing `#[tool(description = ...)]` + `Parameters<*Params>` pattern.
Wired into `KhiveMcpServer` via `#[tool_router]`.

## Worked Example — Complete Agent Loop (Curation + Versioning)

This walks through the full loop combining ADR-014 curation tools with ADR-015 versioning.

**Scenario**: An agent reads a survey paper, adds entities and edges, commits the work, creates an
experimental branch to test a contested claim, fails to verify the claim, and discards the
experiment.

---

**Step 1: Create entities** (batch via `request`)

```json
request({"ops": "[create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention\", properties={\"domain\":\"attention\",\"type\":\"algorithm\"}), create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention-2\"), create(kind=\"entity\", entity_kind=\"document\", name=\"FlashAttention-2: Faster Attention with Better Parallelism\", properties={\"authors\":\"Tri Dao\",\"year\":2023})]"})
```

Returns: `[{id: "ea1..."}, {id: "ea2..."}, {id: "ea3..."}]`

---

**Step 2: Create edges** (batch via `request`)

```json
request({"ops": "[link(source_id=\"ea2\", target_id=\"ea1\", relation=\"extends\", weight=1.0), link(source_id=\"ea2\", target_id=\"ea3\", relation=\"introduced_by\", weight=1.0)]"})
```

---

**Step 3: Commit the work** (via ADR-015 `commit`)

```json
commit({
  "namespace": "local/llm-research",
  "message": "Add FlashAttention-1 and -2 from survey paper reading",
  "author": "agent:paper-reader"
})
```

Returns: `{id: "sha256:4a7f...", parent_id: null, entity_count: 3, edge_count: 2}`

The first commit has no parent — it is the genesis snapshot.

---

**Step 4: Create an experimental branch** (via ADR-015 `branch`)

The agent wants to investigate whether FlashAttention-2 "supersedes" FlashAttention-1 (a contested
claim — the original paper only claims "extends").

```json
branch({
  "namespace": "local/llm-research",
  "name": "supersedes-experiment"
})
```

Returns: `{name: "supersedes-experiment", head_snapshot_id: "sha256:4a7f..."}`

---

**Step 5: Make experimental edits** (via `update(kind="edge")`)

On the `supersedes-experiment` branch (working state), change the relation:

```json
update({
  "kind": "edge",
  "id": "<extends-edge-id>",
  "relation": "supersedes",
  "weight": 0.6
})
```

Note: with the pointer model, this edit affects the shared live working state. The agent is
implicitly "working on" the branch it created. A full implementation should track the "current
branch" in session state so that `commit` knows which branch to advance.

---

**Step 6: Verify the claim, fail**

The agent queries additional sources and finds that FlashAttention-2 does not claim to supersede (it
specifically avoids that framing). The experimental edit is wrong.

---

**Step 7: Abandon the experiment** (via ADR-015 `checkout`)

```json
checkout({
  "namespace": "local/llm-research",
  "branch_name": "main",
  "force": false
})
```

If there are uncommitted experimental changes, this returns an error (the `extends→supersedes` edit
is uncommitted). The agent calls `checkout` with `force: true` to discard them:

```json
checkout({
  "namespace": "local/llm-research",
  "branch_name": "main",
  "force": true
})
```

The namespace is restored to the `sha256:4a7f...` snapshot. The experimental edge change is gone.

---

**Merge scenario** (separate workflow)

A collaborator exports their namespace as a `KgArchive` and shares it. The agent imports it into a
temporary namespace, creates a branch from that import, then merges into `main`.

```json
// Import collaborator's archive
import_kg({"namespace": "local/collab-import", "archive": {...}})

// Commit to make it a versionable snapshot
commit({"namespace": "local/collab-import", "message": "Import from Alice"})

// On main namespace: merge the collaborator's snapshot
merge_branch({
  "namespace": "local/llm-research",
  "theirs": "sha256:<alice-snapshot-id>",
  "message": "Merge Alice's survey additions"
})
```

If conflicts are returned:

```json
// Agent receives:
{
  "status": "conflicts",
  "conflicts": [
    {"type": "property_mismatch", "entity_id": "ea3", "key": "year", "ours": 2023, "theirs": 2022}
  ]
}

// Agent resolves: 2023 is the published year; 2022 was the preprint. Keep ours.
// No action needed (ours value is already in live state).

// Finalize merge
merge_branch({
  "namespace": "local/llm-research",
  "theirs": "sha256:<alice-snapshot-id>",
  "force": true,
  "message": "Merge Alice's survey additions (resolved year conflict)"
})
```

Returns: `{status: "clean", snapshot_id: "sha256:9bc2...", entities_merged: 12, edges_merged: 8}`

The loop is complete. The merge is committed. The collaborator's knowledge is integrated.

## References

- ADR-010: KG Versioning Direction (strategic vision; this ADR implements it for v0.1)
- ADR-014: KG Curation Surface (prerequisite; curation tools used in worked examples)
- ADR-001: Entity Kind Taxonomy (entity kinds in snapshots)
- ADR-002: Closed Edge Ontology (edge validation on import into snapshots)
- ADR-004: Substrate Observables (Event log as version history substrate — future alignment)
- ADR-005: Storage Capability Traits (no new storage traits needed for v0.1 VCS layer)
- ADR-007: Namespace as Open String (namespace hierarchy used for branch fork model in v0.2)
- ADR-011: Deno + MCP-Only (all versioning operations exposed only via MCP)
- `crates/khive-runtime/src/portability.rs`: serialization reused by snapshot storage
- `crates/khive-mcp/src/server.rs`: tool wiring pattern
- git internals: object storage model (inspiration, not imitation)
- terminusdb: prior art in graph versioning (warning: their complexity is what we are avoiding)
