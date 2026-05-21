# ADR-053: KG Branching and Merge

**Status**: accepted (Phase B1+B2 — `khive kg resolve` with `--ours`/`--theirs`/`--merge-properties` + per-entity/edge overrides + NDJSON re-sort + validate-on-finish landed in Deno CLI; B3 schema conflict categorisation and B4 `khive kg log --entity` deferred)\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 established that KG state is serialized as sorted NDJSON files in `.khive/kg/` and that
git provides the versioning layer. ADR-052 defined the storage model: `working.db` is the live
working tree; `entities.ndjson` + `edges.ndjson` are the committed snapshot. ADR-051 defined the
CLI workflow: `khive kg commit` bridges DB to git, `khive kg sync` rebuilds the DB from NDJSON,
and git hooks automate sync after pull/checkout/merge.

Neither ADR addressed how KG-specific merge conflicts are resolved. When two branches modify the
same entity, git produces NDJSON conflict markers that contain raw JSON lines. Raw conflict markers
are workable for developers but hostile to researchers — the conflict is at the entity field level
(e.g., one branch changed a description, the other changed properties), but git presents it as
two competing JSON lines.

This ADR defines:

1. Why sorted NDJSON gives most merges for free (no conflicts).
2. How `khive kg resolve` handles the remaining cases at the entity/edge level.
3. How schema conflicts in `schema.yaml` are categorized and resolved.
4. How cross-repo references interact with merge.

### Design principle: don't wrap git

Branching, checkout, push, pull, stash, and log are standard git operations. The sorted NDJSON
format is designed so that git's three-way merge handles them correctly. The only KG-specific
operation is **conflict resolution** — understanding the entity structure inside NDJSON conflict
markers. Everything else is git, and the user uses git directly (with `khive kg sync` running
automatically via git hooks per ADR-051 §6).

### Relationship to ADR-052

ADR-052 defines the `.khive/state/` directory: `working.db` and `HEAD`. Status — whether the
working tree is dirty — is computed via a DB-vs-committed-NDJSON diff. Every branch operation must
maintain the ADR-052 invariant that `working.db` reflects the current branch's committed NDJSON
plus any uncommitted working changes. The `khive kg sync` command (ADR-051 §5) enforces this
invariant after any git operation that changes NDJSON files.

## Decision

### 1. Branch Model — KG Branches Are Git Branches

KG branches are git branches. There is no separate KG-level branch metadata, no `kg_branches`
table, no custom ref store. A KG branch is exactly the git branch that contains the `.khive/kg/`
files.

```
git checkout -b experiments          standard git — create branch
git checkout main                    standard git — switch branch
                                     (post-checkout hook runs khive kg sync)
git merge experiments                standard git — merge
                                     (post-merge hook runs khive kg sync)
git push origin main                 standard git — push
git pull                             standard git — pull
                                     (post-merge hook runs khive kg sync)
```

The only KG-specific command in the branching workflow is `khive kg resolve`, invoked when
`git merge` produces NDJSON conflicts that need entity-level resolution.

### 2. NDJSON Merge Properties

The sorted NDJSON format (ADR-048 §2) is specifically designed for clean git merges. Because each
entity is one line at a deterministic UUID-sorted position and each edge is one line at a
deterministic composite-key position, git's three-way line merge handles the common cases:

| Scenario                                                            | Git result                                                                  |
| ------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| Two branches add different entities                                 | Clean merge — different lines inserted at different UUID positions          |
| Two branches add different edges                                    | Clean merge — different lines inserted at different composite-key positions |
| Two branches edit different entities                                | Clean merge — different lines modified                                      |
| Two branches edit the same entity                                   | Conflict — same line modified in both                                       |
| One branch deletes an entity, the other edits it                    | Conflict — delete vs. modify on the same line                               |
| Two branches add the same entity (same UUID) with different content | Conflict — same line inserted at same position                              |

Most KG work is additive (new entities, new edges). The conflict-free cases dominate in practice,
so most merges auto-complete without human intervention.

### 3. Entity-Level Conflict Resolution (`khive kg resolve`)

When `git merge` produces conflicts in NDJSON files, the conflict markers appear inline in the
file:

```
<<<<<<< HEAD
{"id":"abc123","kind":"concept","name":"LoRA","description":"Low-rank adaptation method","properties":{},"tags":[]}
=======
{"id":"abc123","kind":"concept","name":"LoRA","description":"Parameter-efficient fine-tuning via low-rank matrices","properties":{},"tags":[]}
>>>>>>> experiments
```

`khive kg resolve` handles these:

1. Parses conflict markers in `entities.ndjson` and `edges.ndjson`.
2. For each conflicting entity, shows a field-level diff between the two versions.
3. Accepts a resolution strategy:
   - `--ours`: keep the current branch version for all conflicts.
   - `--theirs`: keep the incoming branch version for all conflicts.
   - `--merge-properties`: for each conflicting entity, merge non-overlapping property
     changes from both sides; for properties changed on both sides, use the `--ours` value
     and emit a warning listing the discarded fields.
   - `--entity <id> --ours|--theirs|--manual`: resolve a specific entity interactively,
     overriding the global strategy for that entity.
4. After resolution, re-sorts the NDJSON file (conflict resolution may have left the sort
   order intact, but an explicit sort guarantees it).
5. Runs `khive kg validate` on the resolved files to confirm no referential integrity
   violations were introduced.
6. Prints: `Resolved N entity conflicts, M edge conflicts. Run 'git add' and 'git commit'
   to finalize the merge.`

`--merge-properties` is the recommended strategy for agent-driven merges where both branches
legitimately extended the same entity's properties in non-overlapping ways. It fails loudly on
any property key that appears in both versions with different values, requiring explicit
`--ours`/`--theirs` override for those keys.

### 4. Edge Conflict Resolution

Edges can conflict under the same conditions as entities. The conflict key for an edge is the
composite `(source, target, relation)` triple — this is the semantic identity of an edge, not the
internal UUID.

| Scenario                                 | Resolution                                                                                                                                                                                                                                                                                                               |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Weight change on same edge               | `--ours` / `--theirs` apply to the conflicting edge line                                                                                                                                                                                                                                                                 |
| Property change on same edge             | `--merge-properties` merges non-overlapping property changes; overlapping changes use `--ours` with a warning                                                                                                                                                                                                            |
| Delete vs. edit on same edge             | Treated as a conflict; requires explicit `--ours` or `--theirs`                                                                                                                                                                                                                                                          |
| Same composite key, different edge UUIDs | Strategy determines the winning line: `--ours` keeps our edge UUID (the full line from the current branch), `--theirs` keeps their edge UUID (the full line from the incoming branch). The `(source, target, relation)` composite key is the conflict trigger; the strategy determines which edge_id and properties win. |

`--ours` applies the current branch's full edge line for all conflicting edges. `--theirs`
applies the incoming branch's full edge line. `--merge-properties` keeps the current branch's
edge_id but merges non-overlapping property changes from the incoming branch; for overlapping
property keys, the current branch value wins and a warning is emitted.

`--entity <id>` has a corresponding `--edge <source> <target> <relation>` for per-edge
overrides when a global strategy is in use.

After edge conflict resolution, `edges.ndjson` is re-sorted by the composite key (same sort
invariant as `entities.ndjson` by UUID).

### 5. Schema Conflict Resolution

`schema.yaml` conflicts are less frequent than entity conflicts but structurally more serious.

Not all schema conflicts carry the same weight. Three categories:

1. **Base ontology changes** — changes to the 13 closed edge relations from ADR-002 (compile-time
   Rust enums). These always require manual review. There is no automated strategy. Adding,
   renaming, or removing a base relation is an architectural decision.

2. **Pack-scoped and additive changes** — new entity kinds, new optional properties, new pack
   additions, new edge endpoint rules declared in a pack's `EDGE_RULES`. These follow ADR-054's
   schema merge rules: additive changes auto-merge; conflicting changes require manual resolution.

3. **Property schema changes** — changes to property type definitions or validation rules. These
   also defer to ADR-054's merge rules.

| Scenario                                                  | Strategy                                                                                              |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| Two branches add different entity kinds                   | Additive — auto-merge per ADR-054                                                                     |
| Two branches add the same entity kind                     | Conflict — manual resolution                                                                          |
| Two branches add different edge relations (base ontology) | NOT automatically mergeable — always manual review (ADR-002)                                          |
| Two branches add pack-scoped edge endpoint rules          | Additive — auto-merge per ADR-054                                                                     |
| Two branches change the same property definition          | Conflict — manual resolution per ADR-054                                                              |
| Two branches change the same remote pin                   | Manual resolution required — the correct SHA is determined by which branch's remote state is intended |

`khive kg resolve` detects schema conflicts and, where no automated strategy applies, blocks the
merge and prints:

```
error: schema.yaml has merge conflicts that require manual resolution
  Conflict in edge_relations — edge relation changes require architectural review (ADR-002)
  Edit .khive/kg/schema.yaml to resolve, then run 'khive kg validate' before committing.
```

### 6. Cross-Repo References During Merge

From ADR-048 §5, edges can reference entities in other repos via `<remote>:<uuid>` syntax in
the `target` field. During merge:

- Cross-repo edge targets are not validated locally — the remote repo's NDJSON is not fetched
  as part of the merge process.
- A merged edge with a cross-repo target is structurally valid even if the remote entity no
  longer exists at the pinned commit.
- Remote reference validation is deferred to `khive kg validate --resolve-remotes`, which fetches
  the pinned remote NDJSON and checks all cross-repo UUIDs. This is the mode run by CI.
- Merge never fails due to unresolvable remote references. The failure surface is `validate`.

**Rationale**: requiring network access during a local merge would make offline branching
impossible and slow down interactive merge workflows. The commit-SHA pinning in `schema.yaml`
means the remote state is immutable — if the reference was valid when written, it remains valid
at that exact pin. The only failure mode is a pin bump that removes an entity, and that is caught
by `khive kg update <remote>` (ADR-048 §4), not at merge time.

## Alternatives Considered

### Custom branch metadata (not git branches)

A `kg_branches` table in SQLite storing branch names, HEAD commit references, and per-branch
status fields. Rejected: this recreates git's ref infrastructure in a worse form. Branches would
not interoperate with GitHub PRs, CI runners, or any git tooling. The complexity of keeping
custom branch state in sync with git state adds maintenance cost without benefit.

### Per-branch DB files

`.khive/state/<branch>.db` — one SQLite file per branch. Rejected: disk space accumulates
with every branch and is never reclaimed when branches are deleted (git branch delete does not
know about the DB files). A 10K-entity KG at ~20MB per DB file would consume 200MB for ten
active branches. Clean rebuild from NDJSON via `khive kg sync` is simpler, has no accumulation
problem, and is fast enough for the expected KG sizes.

### Three-way merge at the entity level (not git line merge)

A custom merge algorithm that operates on the entity/edge level rather than the NDJSON line
level. Deferred: git's line-level merge handles 90%+ of cases correctly because of the sorted
NDJSON invariant. Entity-level three-way merge would eliminate the residual 10% of cases where
line-level merge produces a false conflict (two branches modifying different fields of the same
entity), but implementing this before the conflict pattern is validated on real workloads would
be premature. The `--merge-properties` strategy in `khive kg resolve` handles this class of
conflict interactively.

### CRDT-based merge

Using a Conflict-free Replicated Data Type for graph state. Rejected: CRDTs silently accept all
writes, including semantically contradictory ones. ADR-010 explicitly rejected CRDTs for KG merge.
Silent corruption is worse than a paused merge.

### Wrapping all git operations (push/pull/branch/checkout/stash/log)

Wrapping every git command with `khive kg` equivalents. Rejected: the only KG-specific operation
is DB rebuild (handled by `khive kg sync` via git hooks, ADR-051 §6) and conflict resolution
(this ADR). Wrapping push, pull, branch, checkout, stash, and log adds maintenance cost, forces
users to learn khive-specific commands for universal git operations, and only works in the CLI
(not IDE/GUI git clients). Git hooks + `khive kg sync` + `khive kg resolve` achieve the same
result with less code and broader compatibility.

## Consequences

### Positive

- Branch operations compose with the entire git ecosystem. GitHub PRs work for KG branches.
  CI (ADR-048 §6) runs on any PR that touches `.khive/kg/`. Code review tools show entity-aware
  diffs via `khive kg diff` as a git diff driver.
- No new state to manage. Branches live in git refs; the only khive-specific state is
  `working.db` (rebuilt by `khive kg sync` via hooks) and `.khive/state/HEAD` (a cheap local cache).
- `khive kg resolve` provides entity-level conflict resolution that understands field-level diffs,
  not just competing JSON lines. The `--merge-properties` strategy handles the most common agent
  conflict pattern (two branches extending the same entity's properties).
- Cross-repo references are merge-transparent. No network access during merge.
- Users use standard git for branching, checkout, push, pull, stash, and log. No new commands
  to learn for git operations.

### Negative

- Every branch switch triggers a DB rebuild (via `khive kg sync` in the post-checkout hook). At
  10K entities, this is 2–3 seconds. At 100K entities, this becomes 20–30 seconds. Mitigation:
  incremental rebuild (see Alternatives) addresses this when the scale warrants it.
- Merge conflicts in NDJSON are raw JSON lines in the conflict markers. `khive kg resolve`
  renders them in entity-aware terms, but users who open the file directly see raw JSON.
- Base-ontology schema conflicts always require manual resolution. There is no automated strategy
  for changes to the 13 closed edge relations (ADR-002).
- Git hooks must be installed per-clone. Fresh clones of an existing KG repo run
  `khive kg sync` to bootstrap `working.db` (since `.khive/kg/` already exists,
  `khive kg init` would error). Hook installation for fresh clones is manual until
  `khive kg install-hooks` is provided (Phase C2). Developers who skip hook installation
  will not get automatic sync but can always run `khive kg sync` manually after branch
  switches and merges.

### Neutral

- The branch model is additive over ADR-048 and ADR-052. No existing operations change meaning.
  `khive kg commit`, `status`, `export`, `import`, and `sync` behave identically on any branch.
- The `working.db` rebuild delegates to the ADR-052 atomic rebuild path (validate → temp DB →
  atomic rename). No new import logic is needed.

## Implementation

### CLI command additions to the Deno CLI (`deno/src/kg/`)

```
khive kg resolve [--ours|--theirs|--merge-properties] [--entity <id> ...] [--edge <s> <t> <r> ...]
```

This is the only new CLI command introduced by this ADR. Branching, checkout, push, pull, stash,
and log use standard git commands, with `khive kg sync` (ADR-051) running automatically via hooks.

### `khive-vcs` additions

- `resolve.rs`: `resolve_conflicts()` — conflict marker parser, entity/edge-level strategy
  application, NDJSON re-sort, validate call. This is the only new Rust code introduced by this
  ADR.
- `merge.rs`: `detect_ndjson_conflicts()` — utility that scans `entities.ndjson` and
  `edges.ndjson` for conflict markers and returns structured conflict descriptions. Used by
  `resolve.rs` and optionally by `khive kg sync` to detect unresolved conflicts before rebuild.

### No new DB schema changes

This ADR introduces no new SQL tables or migrations. The `.khive/state/` directory structure is
defined by ADR-052 and requires no additions.

### Phasing

| Phase | Scope                                                                                         | Target |
| ----- | --------------------------------------------------------------------------------------------- | ------ |
| B1    | `khive kg resolve` — entity conflict resolution with `--ours`/`--theirs`/`--merge-properties` | v0.5   |
| B2    | Edge conflict resolution (`--edge` overrides, composite-key identity)                         | v0.5   |
| B3    | Schema conflict detection and category reporting                                              | v0.5   |
| B4    | `khive kg log --entity` (field-level history rendering, presentation only)                    | v0.6   |

B1 and B2 are the core merge conflict workflow. B3 is defensive (schema conflicts are rare but
serious). B4 is a convenience feature that can be deferred indefinitely.

## References

- ADR-002: Closed Edge Ontology — why schema conflicts require manual review
- ADR-010: KG Versioning Direction — "GitHub for knowledge graphs" positioning
- ADR-048: Git-Native KG Versioning — NDJSON format, cross-repo references
- ADR-051: CLI Workflow — `khive kg commit`, `khive kg sync`, git hooks
- ADR-052: KG Storage Model — working.db, atomic rebuild, status diff
- ADR-054: Schema Evolution — additive merge rules for packs and properties
- git merge documentation: https://git-scm.com/docs/git-merge
