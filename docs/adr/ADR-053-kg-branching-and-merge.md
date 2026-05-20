# ADR-053: KG Branching and Merge

**Status**: proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 established that KG state is serialized as sorted NDJSON files in `.khive/kg/` and that
git provides the versioning layer. ADR-052 defined the storage model: `working.db` is the live
working tree; `entities.ndjson` + `edges.ndjson` are the committed snapshot; `khive kg commit`
is the bridge that exports DB state to files and invokes `git commit`.

Neither ADR addressed how branching and merging work. KGs need branches for the same reasons
code does: experiments that may not land, parallel development by separate agents, feature work
that needs review before it reaches main. Without a defined branching model, users either avoid
branches (losing the primary benefit of the git foundation) or treat `git checkout` as a plain
file operation and discover that the `working.db` is now out of sync with the NDJSON files they
just checked out.

This ADR defines how `khive kg branch`, `checkout`, and `merge` map to git operations, what
happens to `working.db` on a branch switch, how NDJSON merge conflicts are resolved at the
entity level, and how remotes (GitHub and khive.ai cloud) interact with the branching model.

### Relationship to ADR-052

ADR-052 defines the `.state/` directory under `.khive/kg/`: `working.db` (the live DB) and
`HEAD` (the committed snapshot reference). Status — whether the working tree is dirty — is
computed via a DB-vs-committed-NDJSON diff as specified in ADR-052. This ADR sits on top of
that model: every branch operation must maintain the ADR-052 invariant that `working.db`
reflects the state of the current branch's committed snapshot plus any uncommitted working
changes.

## Decision

### 1. Branch Model — KG Branches Are Git Branches

KG branches are git branches. There is no separate KG-level branch metadata, no `kg_branches`
table, no custom ref store. A KG branch is exactly the git branch that contains the `.khive/kg/`
files.

```
khive kg branch create experiments   →  git checkout -b experiments
                                        (working.db unchanged — see §2)
khive kg branch list                 →  git branch --list
khive kg branch delete experiments   →  git branch -d experiments
khive kg checkout main               →  git checkout main
                                        + rebuild working.db from NDJSON
khive kg merge experiments           →  git merge experiments
                                        + rebuild working.db from merged NDJSON
```

**Rationale**: git already solves branch management, ref storage, and remote tracking.
Reinventing any of this in Rust atop SQLite is months of work that delivers a worse UX on a
private API. Our only value-add is the DB rebuild step that makes the branch switch transparent
to callers using the verb surface.

The `.khive/kg/.state/HEAD` file (defined in ADR-052) is kept in sync with the git branch: it
holds the current branch name and is updated by `khive kg checkout` and `khive kg branch create`.
It is not authoritative — `git rev-parse --abbrev-ref HEAD` is always ground truth. It exists
as a cheap local read for the khive CLI to avoid a subprocess call on every verb dispatch.

### 2. Checkout — Git Checkout + DB Rebuild

Switching branches changes which NDJSON files are present on disk. `working.db` must be rebuilt
from the new files. The sequence is:

1. **Dirty check**: run the ADR-052 DB-vs-committed-NDJSON diff (same operation as
   `khive kg status`). If the working tree is dirty (uncommitted changes present), refuse
   the checkout and print:

   ```
   error: cannot checkout 'experiments' — uncommitted KG changes present
     (use 'khive kg commit' to commit, or 'khive kg stash' to stash)
   ```

   This mirrors `git checkout`'s behavior when there are uncommitted file changes. The user
   must resolve their working state before switching context.

2. **`git checkout <branch>`**: switches the NDJSON files to the target branch's committed state.
   The `--no-verify` flag is NOT passed — git hooks run normally.

3. **Rebuild `working.db`**: drop and reimport from the new NDJSON files. This is the same
   operation as `khive kg import --on-conflict update` applied to a fresh database. The
   implementation reuses the `import.rs` path from `khive-vcs`.

   Performance estimate: 10K entities + 50K edges takes approximately 2–3 seconds on modern
   hardware with batch INSERT. For large KGs (100K+ entities), an incremental rebuild is
   preferable — see §9 on deferred optimizations.

4. **Update `.state/HEAD`**: write the new branch name.

### 3. Branch Create — No Immediate Rebuild

`khive kg branch create <name>` only runs `git checkout -b <name>`. It does NOT rebuild
`working.db` because no files changed — the new branch starts at the same commit as the current
branch. The `working.db` state, including any uncommitted changes, carries over to the new branch
(exactly as unstaged edits carry over in `git checkout -b` when there are no conflicts).

This matches the expected mental model: "I'm starting a new experiment branch from where I am
now. Everything I've done so far is still here; I just haven't committed it to the new branch
yet."

### 4. NDJSON Merge Properties

The sorted NDJSON format (ADR-048 §2) is specifically designed for clean git merges. Because each
entity is one line at a deterministic UUID-sorted position and each edge is one line at a
deterministic composite-key position, git's three-way line merge handles the common cases:

| Scenario | Git result |
|---|---|
| Two branches add different entities | Clean merge — different lines inserted at different UUID positions |
| Two branches add different edges | Clean merge — different lines inserted at different composite-key positions |
| Two branches edit different entities | Clean merge — different lines modified |
| Two branches edit the same entity | Conflict — same line modified in both |
| One branch deletes an entity, the other edits it | Conflict — delete vs. modify on the same line |
| Two branches add the same entity (same UUID) with different content | Conflict — same line inserted at same position |

Most KG work is additive (new entities, new edges). The conflict-free cases dominate in practice,
so most merges will auto-complete without human intervention.

### 5. Entity-Level Conflict Resolution

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
6. Instructs the user to run `khive kg commit` to finalize the merge.

`--merge-properties` is the recommended strategy for agent-driven merges where both branches
legitimately extended the same entity's properties in non-overlapping ways. It fails loudly on
any property key that appears in both versions with different values, requiring explicit
`--ours`/`--theirs` override for those keys.

### 5b. Edge Conflict Resolution

Edges can conflict under the same conditions as entities. The conflict key for an edge is the
composite `(source, target, relation)` triple — this is the semantic identity of an edge, not the
internal UUID.

| Scenario | Resolution |
|---|---|
| Weight change on same edge | `--ours` / `--theirs` apply to the conflicting edge line |
| Property change on same edge | `--merge-properties` merges non-overlapping property changes; overlapping changes use `--ours` with a warning |
| Delete vs. edit on same edge | Treated as a conflict; requires explicit `--ours` or `--theirs` |
| Same composite key, different edge UUIDs | Strategy determines the winning line: `--ours` keeps our edge UUID (the full line from the current branch), `--theirs` keeps their edge UUID (the full line from the incoming branch). The `(source, target, relation)` composite key is the conflict trigger; the strategy determines which edge_id and properties win. |

`--ours` applies the current branch's full edge line for all conflicting edges. `--theirs`
applies the incoming branch's full edge line. `--merge-properties` keeps the current branch's
edge_id but merges non-overlapping property changes from the incoming branch; for overlapping
property keys, the current branch value wins and a warning is emitted.

`--entity <id>` has a corresponding `--edge <source> <target> <relation>` for per-edge
overrides when a global strategy is in use.

After edge conflict resolution, `edges.ndjson` is re-sorted by the composite key (same sort
invariant as `entities.ndjson` by UUID).

### 6. Schema Conflict Resolution

`schema.yaml` conflicts are less frequent than entity conflicts but structurally more serious.

Not all schema conflicts carry the same weight. ADR-053 distinguishes three categories:

1. **Base ontology changes** — changes to the 13 closed edge relations from ADR-002 (compile-time
   Rust enums). These always require manual review. There is no automated strategy. Adding,
   renaming, or removing a base relation is an architectural decision.

2. **Pack-scoped and additive changes** — new entity kinds, new optional properties, new pack
   additions, new edge endpoint rules declared in a pack's `EDGE_RULES`. These follow ADR-054's
   schema merge rules: additive changes auto-merge; conflicting changes require manual resolution.
   ADR-053 defers to ADR-054 for this category.

3. **Property schema changes** — changes to property type definitions or validation rules. These
   also defer to ADR-054's merge rules.

| Scenario | Strategy |
|---|---|
| Two branches add different entity kinds | Additive — auto-merge per ADR-054 |
| Two branches add the same entity kind | Conflict — manual resolution |
| Two branches add different edge relations (base ontology) | NOT automatically mergeable — always manual review (ADR-002) |
| Two branches add pack-scoped edge endpoint rules | Additive — auto-merge per ADR-054 |
| Two branches change the same property definition | Conflict — manual resolution per ADR-054 |
| Two branches change the same remote pin | Manual resolution required — the correct SHA is determined by which branch's remote state is intended |

For additive schema changes (new kinds, new optional properties, pack additions), ADR-054 defines
the merge resolution. ADR-053 reserves manual review only for base ontology changes (the 13 closed
edge relations from ADR-002) and incompatible property type changes.

`khive kg resolve` detects schema conflicts and, where no automated strategy applies, blocks the
merge and prints:

```
error: schema.yaml has merge conflicts that require manual resolution
  Conflict in edge_relations — edge relation changes require architectural review (ADR-002)
  Edit .khive/kg/schema.yaml to resolve, then run 'khive kg validate' before committing.
```

### 7. Remote Operations

The remote model delegates to git for transport. Cloud sync (khive.ai) is an optional additional
push target, not the primary mechanism.

**Push:**

```
khive kg push                        →  git push (current branch to configured remote)
                                        + optional POST /v1/projects/:ns/sync to khive.ai
khive kg push --remote origin        →  git push origin <current-branch>
khive kg push --cloud                →  POST /v1/projects/:ns/sync to khive.ai only
                                        (no git remote required)
```

The cloud sync call sends the committed NDJSON files to khive.ai and returns a sync receipt. It
does not replace git push — it enables users without their own git hosting to share KGs through
the khive.ai platform.

**Pull:**

```
khive kg pull                        →  git pull (merge or rebase per git config)
                                        + rebuild working.db from merged NDJSON
khive kg pull --remote origin        →  git pull origin <current-branch>
                                        + rebuild working.db
khive kg pull --cloud                →  fetch NDJSON from khive.ai
                                        + write to .khive/kg/
                                        + rebuild working.db
```

After any pull that changes the NDJSON files, `working.db` is rebuilt. If the pull produces
conflicts (from `git pull --merge` hitting a conflict), the process pauses at the conflict
resolution step (§5) before rebuilding.

**Local-only mode (no remote configured):**

`khive kg push` and `khive kg pull` print an error if neither a git remote nor `--cloud` is
specified. All branching and merging continue to work locally via git.

### 8. Stash Support

```
khive kg stash                       →  export working.db to temp NDJSON patch
                                        git stash (stashes any modified .khive/kg/ files)
                                        rebuild working.db from clean NDJSON (base state)
khive kg stash pop                   →  git stash pop
                                        rebuild working.db from popped NDJSON state
khive kg stash list                  →  git stash list (filtered to KG stash entries)
```

`git stash` operates on file changes, not on the DB. Before stashing, `khive kg stash` runs
`khive kg export` to ensure the working DB state is captured in the NDJSON files (without
committing), then invokes `git stash` on those files, then rebuilds `working.db` from the
committed base.

This means stash captures exactly the changes that `khive kg status` would show as uncommitted —
the same state that `khive kg commit` would commit. Stash pop reverses the process.

### 9. Log and History

```
khive kg log                         →  git log --oneline -- .khive/kg/
                                        (KG-touching commits only)
khive kg log --entity <id>           →  git log -p -- .khive/kg/entities.ndjson
                                        (diffs filtered to lines matching the entity UUID)
khive kg log --since <date>          →  git log --since=<date> -- .khive/kg/
```

Entity-level history is derived from git's line-level history. Because each entity occupies one
line, `git log -p` on `entities.ndjson` followed by filtering for the entity's UUID shows every
commit that created, modified, or deleted that entity. `khive kg log --entity` parses the
filtered diff output and renders field-level changes per commit:

```
a3b4c5d (2026-05-18) feat: research LoRA fine-tuning approach
  ~ concept "LoRA" (abc123)
    properties.status: "researched" → "implemented"

f1e2d3c (2026-05-15) feat: add LoRA entity
  + concept "LoRA" (abc123)
    kind: concept
    properties.type: technique
```

This is a presentation layer over `git log -p`, not a custom history engine.

### 10. Cross-Repo References During Merge

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

`.khive/kg/.state/<branch>.db` — one SQLite file per branch. Rejected: disk space accumulates
with every branch and is never reclaimed when branches are deleted (git branch delete does not
know about the DB files). A 10K-entity KG at ~20MB per DB file would consume 200MB for ten
active branches. The lifecycle management (create on checkout, delete on branch delete) requires
hooking into git operations or periodic garbage collection. Clean rebuild from NDJSON is simpler,
has no accumulation problem, and is fast enough for the expected KG sizes.

### Incremental DB patching on checkout

Instead of dropping and reimporting `working.db` on branch switch, compute the NDJSON diff
between the old and new committed states and apply only the changed entities and edges. Deferred
to a later optimization pass, not rejected. For MVP, the clean rebuild is correct by construction
and fast enough (2–3 seconds for 10K entities). Incremental rebuild is the right optimization
for large KGs (100K+ entities) where rebuild time becomes noticeable, but implementing it before
measuring the actual bottleneck violates the no-premature-optimization principle. The interface
is identical — the optimization is internal to `khive kg checkout`.

### Three-way merge at the entity level (not git line merge)

A custom merge algorithm that operates on the entity/edge level rather than the NDJSON line
level: loads all three versions of the KG (base, ours, theirs) into memory, computes a
structured three-way merge at the field level, and writes the result directly. Deferred: git's
line-level merge handles 90%+ of cases correctly because of the sorted NDJSON invariant. Entity-
level three-way merge would eliminate the residual 10% of cases where line-level merge produces
a false conflict (two branches modifying different fields of the same entity), but implementing
this before the conflict pattern is validated on real workloads would be premature. The
`--merge-properties` strategy in `khive kg resolve` (§5) handles this class of conflict
interactively.

### CRDT-based merge

Using a Conflict-free Replicated Data Type for graph state, eliminating conflicts entirely by
construction. Rejected: CRDTs silently accept all writes, including semantically contradictory
ones (two branches establishing mutually exclusive property values, two branches each believing
they are "the canonical" definition of a concept). ADR-010 explicitly rejected CRDTs for KG
merge. Silent corruption is worse than a paused merge. The design optimizes for the case where
conflicts are rare (additive KG work), not for the case where conflicts are expected.

## Consequences

### Positive

- Branch operations compose with the entire git ecosystem. GitHub PRs work for KG branches.
  CI (ADR-048 §6) runs on any PR that touches `.khive/kg/`. Code review tools show entity-aware
  diffs via `khive kg diff` as a git diff driver.
- No new state to manage. Branches live in git refs; the only khive-specific state is
  `working.db` (rebuilt on checkout) and `.state/HEAD` (a cheap local cache of the current
  branch name).
- Stash, log, and history commands delegate to git, inheriting all of git's guarantees
  (e.g., stash stack is preserved across machine restarts).
- Cross-repo references are merge-transparent. Edges referencing remote entities pass through
  merge without requiring network access.

### Negative

- Every branch switch requires a DB rebuild. At 10K entities, this is 2–3 seconds — noticeable
  but acceptable. At 100K entities, this becomes 20–30 seconds — unacceptable for interactive
  use. Mitigation: incremental rebuild (§ Alternatives Considered) addresses this when the scale
  warrants it.
- Uncommitted changes block checkout. Users who work in long-running sessions with uncommitted
  DB changes must stash or commit before switching branches. This is the same friction as git
  itself, but it may surprise users accustomed to switching branches freely in a database
  context.
- Merge conflicts in NDJSON are raw JSON lines in the conflict markers. `khive kg resolve`
  renders them in entity-aware terms, but users who open the file directly see raw JSON. This is
  a UX gap compared to purpose-built merge tools.
- Base-ontology schema conflicts always require manual resolution. There is no automated strategy
  for changes to the 13 closed edge relations (ADR-002). Additive changes (new entity kinds, new
  pack additions) auto-merge per ADR-054, but any branch that touches the base edge ontology
  requires human review before it can merge.

### Neutral

- The branch model is additive over ADR-048 and ADR-052. No existing operations change meaning.
  `khive kg commit`, `status`, `export`, and `import` behave identically on any branch.
- The `working.db` rebuild is the same codepath as `khive kg import --on-conflict update` on a
  fresh DB. No new import logic is needed.

## Implementation

### CLI command additions to the Deno CLI (`deno/src/kg/`)

```
khive kg branch create <name>        — git checkout -b + update .state/HEAD
khive kg branch list                 — git branch --list
khive kg branch delete <name>        — git branch -d
khive kg checkout <branch>           — dirty check + git checkout + rebuild DB + update .state/
khive kg merge <branch>              — dirty check + git merge + conflict check + rebuild DB
khive kg resolve [--ours|--theirs|--merge-properties] [--entity <id> ...]
                                     — parse conflict markers, apply strategy, re-sort, validate
khive kg stash                       — export + git stash + rebuild DB
khive kg stash pop                   — git stash pop + rebuild DB
khive kg stash list                  — git stash list (filtered)
khive kg push [--remote <r>] [--cloud]
khive kg pull [--remote <r>] [--cloud]
```

All commands are in the `khive kg` CLI (`deno/src/kg/`) and use the `khive-vcs` crate for the
DB rebuild step via the MCP server. Git operations are invoked as subprocess calls. No git library
dependency is added — git is already a required runtime dependency (ADR-048 §Consequences).

### `khive-vcs` additions

- `branch.rs`: `create_branch()`, `list_branches()`, `delete_branch()` — thin wrappers over
  git subprocess calls. Returns structured errors on git failure.
- `checkout.rs`: `checkout_branch()` — dirty check via `status.rs` (ADR-052), git checkout,
  DB rebuild via `import.rs`, state file update.
- `merge.rs`: `merge_branch()` — git merge, conflict detection in NDJSON files, DB rebuild
  on clean merge, block on conflict pending `khive kg resolve`.
- `resolve.rs`: `resolve_conflicts()` — conflict marker parser, strategy application, NDJSON
  re-sort, validate call.
- `stash.rs`: `stash()`, `stash_pop()` — export, git stash, rebuild.
- `remote.rs` (extends ADR-048 §Remote cache): `push()`, `pull()` — git push/pull wrappers
  with optional cloud sync POST.

### No new DB schema changes

This ADR introduces no new SQL tables or migrations. The `.state/` directory structure is
defined by ADR-052 and requires no additions.

### Phasing

| Phase | Scope | Target |
|-------|-------|--------|
| B1 | `branch create/list/delete`, `checkout`, `merge` (clean merge only) | v0.5 |
| B2 | `resolve` command (conflict resolution) | v0.5 |
| B3 | `stash` / `stash pop` / `stash list` | v0.5 |
| B4 | `push` / `pull` (git remote) | v0.5 |
| B5 | `push --cloud` / `pull --cloud` (khive.ai sync) | v0.6 |
| B6 | `khive kg log --entity` (field-level history rendering) | v0.6 |

B1 and B2 are the core branching and merge workflow. B3–B6 are supporting operations that
improve the experience but are not blockers for using branches.

## References

- ADR-010: KG Versioning Direction — "GitHub for knowledge graphs" positioning
- ADR-048: Git-Native KG Versioning — NDJSON format, cross-repo references, CLI commands
- ADR-051: CLI Authentication and KG Git Workflow Commands — `khive kg commit/push/pull/status/branch/log` interface
- ADR-052: KG Storage Model — working.db, HEAD, DB-vs-NDJSON diff status, `.state/` layout
- ADR-043: KG Merge Algorithm — original three-way merge design (substantially reduced in scope by ADR-048; further narrowed by this ADR to the conflict-resolution pass only)
- ADR-002: Closed Edge Ontology — why schema conflicts require manual review
- git merge documentation: https://git-scm.com/docs/git-merge
- git stash documentation: https://git-scm.com/docs/git-stash
