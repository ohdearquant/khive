# ADR-102: Tiered Validate-and-Merge — Rule-Gated Fast Path and Reviewed Change-Set Path

**Status**: Accepted
**Date**: 2026-07-08
**Amends**: [ADR-020](ADR-020-git-native-kg-implementation.md) (restores the `kg commit` CLI
primitive to the `kkernel kg` subcommand surface; see "Amendment to ADR-020" below)
**Depends on**: [ADR-101](ADR-101-kg-changeset-model.md) (the change-set artifact this ADR
validates, tiers, and merges), ADR-002 (edge ontology — the contract the rule engine
mechanizes), ADR-010 (KG versioning — the git-native strategic frame), ADR-037 (atomic
staging-then-rename, reused for the snapshot-commit lane), ADR-055 (epistemic relations —
`supports`/`refutes` as tier-2 triggers), ADR-067 (single-writer daemon — the write path
tier-1 rides)

---

## Context

[ADR-101](ADR-101-kg-changeset-model.md) defines the change-set artifact: a producer-agnostic,
stage-time-ID-stable op-list. That artifact needs a path from "staged" to "landed in the live
graph," and not every staged operation carries the same risk. Creating a new entity and adding
a routine link are low-risk, high-volume, and time-sensitive — a batch producer's whole value
proposition depends on its output becoming searchable within minutes, not after a review queue
clears. Marking one claim as superseded, refuting another, deleting a row, or merging two
entities together are comparatively rare, harder to undo cleanly, and materially more expensive
to get wrong: they change what the graph asserts, not just what it contains.

Applying every staged operation through the same gate is wrong in both directions. Gating
everything on review starves the high-volume, low-risk case of the latency it needs. Gating
nothing lets the small number of judgment-bearing operations land unreviewed, which is exactly
the failure mode a change-set model exists to prevent. This ADR splits the write path in two by
risk, and defines the rule engine that decides which path a given operation takes.

### What is already shipped and what this ADR builds on

Two relevant surfaces exist in the codebase today, and this ADR extends both rather than
inventing beside them:

- **`kkernel kg validate`** already runs a configurable, TOML-driven rule pass over KG data
  with a severity model (`error` / `warning` / `info`). [ADR-020](ADR-020-git-native-kg-implementation.md)
  and [ADR-034](ADR-034-kg-validation-pipelines.md) define that validation surface; the shipped
  implementation has since extended it with five individually
  enable/disable/configurable rule classes: **edge-endpoint-types** (validates each
  edge's source/relation/target kinds against the canonical endpoint contract),
  **edge-direction-conventions** (flags likely-inverted directional edges against a configurable
  forward pattern per relation), **dangling-refs** (a configurable counterpart to the always-on
  referential-integrity check), **naming-conventions** (entity name hygiene with
  per-entity-kind overrides), and **citation-date-lint** (flags forward-dated citation values).
  This ADR does not add a sixth rule class or change any of the five; it adds a **tier
  predicate** that reads a change-set's proposed operations and this same rule pass's findings
  to decide which write path an operation takes (D2 below).
- **The `kkernel kg` subcommand tree** ([ADR-020](ADR-020-git-native-kg-implementation.md) §5)
  specifies eleven verbs, including `kg commit` (export + validate + git add + git commit). Of
  those, `validate`, `init`, `fetch`/`sync`, `export`, `import`, `status`, and hook management
  are implemented; `kg commit` is not — it exists today only as an unimplemented, proposed
  design concept. This ADR restores it as a real CLI primitive, scoped to the tier-2 flow
  defined below.

---

## Decision

### D1 — Tier split of the write path

Every staged change-set operation resolves, before it is applied, to exactly one of two tiers:

- **Tier-1 (fast path)**: stage-time rule validation, then the **existing live batch write**
  (the same MCP batch-op surface an interactive agent already uses, preserving today's batch
  write latency), followed by an **asynchronous snapshot-commit** that records the write into
  the git-native history after the fact.
- **Tier-2 (reviewed path)**: the operation stays a **staged NDJSON change-set**, a **rendered
  diff** is produced against current graph state, an **independent review** gates it, and only
  on approval does it become a live write plus a synchronous commit.

Tier-1 optimizes for the case ADR-101's context describes: high-volume, low-risk, time-sensitive
writes that a batch producer needs searchable within minutes. Tier-2 optimizes for correctness
on the small number of operations whose blast radius justifies a human or cross-model check
before they land. Neither tier is optional or bypassable per D2; every staged operation is
classified, not just the ones a caller happens to flag.

### D2 — Rule engine and tier predicate

**Rules run at stage time**, against the change-set as staged, using the same rule pass
described in Context: **headless** via `kkernel kg validate --rules <path>`, driven by a rules
file; a graphical reviewer evaluates the identical rules file through the shared rule-evaluator
crate ([ADR-101](ADR-101-kg-changeset-model.md) D5), so a headless CI run and an interactive
review session can never disagree about whether a given change-set is clean.

**The tier predicate is configurable in the rules file, not hardcoded.** The predicate is a
declarative condition over an operation's kind, its edge relation (where applicable), its field
changes, and the rule pass's own findings for it. The seed predicate, shipped as the default and
overridable per deployment:

- **Tier-1 eligible**: `create`, a non-judgment `link` (any edge relation other than the ones
  named tier-2 below), or an `update` to a mutable entity field — **and** the rule pass reports
  no `error`-severity finding for that operation. `warning`- and `info`-severity findings do
  not block tier-1; they are recorded on the change-set so a reviewer or later reader sees
  them, but they do not gate the fast path.
- **Tier-2 required**: a `link` carrying `supersedes`, `supports`, or `refutes` (ADR-002's
  derivation category and [ADR-055](ADR-055-epistemic-edge-relations.md)'s epistemic category —
  the judgment-bearing relations by construction), any `delete`, any `merge`, any change to an
  existing edge's relation or weight, or a `link`/edge-weight change where the resulting weight
  falls below `0.7` (ADR-002's own boundary between "strong" and "plausible or weaker"
  evidential strength). Any operation carrying an `error`-severity finding is also tier-2,
  regardless of its kind — an `error` always escalates to review; it is never downgraded to a
  lower severity, suppressed, or shipped tier-1.

Because the predicate lives in the rules file rather than in code, a deployment can widen or
narrow the tier-1 set (for example, admitting more relation types to tier-1 once a producer has
an established low-error track record) without an ADR amendment or a binary rebuild, so long as
the change stays inside the rules-file contract this ADR defines. It cannot, by that same
contract, remove `supersedes`/`supports`/`refutes`/`delete`/`merge` from tier-2 — those are named
here as the ADR-002/ADR-055-derived floor, not a default a rules file is free to lower.

### D3 — Review gate: hard routing, not convention

Tier-2's independent review has one binding requirement: **the reviewer's model family must
differ from the producer's.** This is enforced as a **hard routing gate** — the tier-2 flow
refuses to present a change-set for approval to a reviewer identity sharing the producer's model
family, rather than relying on a convention or a reviewer's own judgment to self-recuse. The
gate's input is the producer model family recorded in the change-set's envelope metadata
([ADR-101](ADR-101-kg-changeset-model.md) D1) — an authoritative, stage-time-captured field,
not an out-of-band convention an implementer has to invent. The
rationale is the same one that motivates cross-family review in code review generally: a
same-family reviewer is more likely to share the producer's blind spots and less likely to catch
a systematic error class the producer itself cannot see. Making this a routing gate rather than
guidance means the property holds even when an operator forgets to configure it deliberately.

### D4 — Revert semantics

**Op-list inversion is the primary revert mechanism.** Because every tier-2 change-set is a
typed op-list ([ADR-101](ADR-101-kg-changeset-model.md) D1), each operation has a well-defined
inverse: a `create`'s inverse is a `delete` of the same identifier; a `link`'s inverse removes
that edge; an `update`'s inverse restores the prior field values captured at stage time; a
`delete`'s or `merge`'s inverse is reconstructed from the stage-time preimage ADR-101 D1
requires destructive operations to capture. Reverting a landed change-set means applying its
inverted op-list as a new, independently reviewed change-set, which keeps a revert auditable
with its own provenance, exactly like any other tier-2 write. **Inversion is defined only over
operations whose preimage was captured**: an operation staged without one, or whose preimage
has been invalidated by intervening writes to the same rows, cannot be surgically inverted and
must fall back to a reviewed compensating change-set authored against current state, or to the
git backstop below.

**Git revert of the NDJSON commit is the backstop**, used when op-list inversion is unavailable
or insufficient (for example, a revert requested long after intervening writes have touched the
same rows, where a straightforward field-level inversion would clobber later legitimate state).
**Stated plainly, because the two are easy to conflate**: a snapshot-level git revert reverts
**whole-graph state as committed at that snapshot**, not the surgical effect of one operation. It
is a coarse instrument — correct for "put the graph back exactly as it was at commit X," wrong
for "undo just this one edge without touching anything else that has changed since." Op-list
inversion is the surgical tool; git revert is the blunt one, kept available deliberately for the
cases the surgical tool cannot reach.

### D5 — Topology: MCP-client-only access to the live graph

Tooling built for this ADR (the headless CLI and any future reviewer surface) accesses the
**live graph exclusively as an MCP client of the warm daemon** — the same stdio-MCP-client
topology the daemon already serves other clients through, preserving the single DB handle and
single-writer discipline [ADR-067](ADR-067-write-owner-daemon.md) establishes. **No tooling in
this ADR's scope opens a second process handle on the live SQLite file.** Staged change-sets and
committed snapshots are read as NDJSON files directly from disk — they are not live-graph state
and carry no daemon-coexistence hazard, so file access to them is unrestricted.

This deliberately diverges from patterns elsewhere in the codebase that link against the graph
store in-process for a shared KG; the divergence is explicit and normative for this ADR's
tooling, not an oversight. The same principle extends to the read side of tier-1's
snapshot-commit: exporting the graph for the periodic snapshot commit reads from a **replica**,
not the live store — a sustained read against the live database is the same
WAL-checkpoint-pinning hazard class this topology constraint exists to prevent, and a
periodically-refreshed replica is current enough for a snapshot cadence measured in hours while
adding zero live-store contention.

### D6 — Local-only change-set and snapshot repository (binding constraint)

**The repository holding staged change-sets and committed snapshots is local-only.** Exports
carry live graph content — including notes and memories that may never leave the local host
without an explicit, separate decision — so this repository **MUST NOT** have a git remote
added to it, and no lane, tool, or automation defined by this ADR may add one. This is a
normative constraint, not a default that happens to be unconfigured: a future need for off-host
replication of this repository's content is a new decision, made explicitly and covered by its
own ADR, not a configuration change slipped into an existing lane.

Rationale: unlike the source-code repository this design otherwise takes its git-native
inspiration from ([ADR-010](ADR-010-kg-versioning.md), [ADR-020](ADR-020-git-native-kg-implementation.md)),
this repository's content is not source code — it is a live export of graph data that may include
material never intended for any shared or public destination. Treating "local-only" as the
default-safe posture, and requiring a deliberate future decision to change it, is the same
data-sensitivity posture the codebase already applies to backup and replication targets
elsewhere; it is restated here as a binding constraint specifically because a remote is the one
mistake that would be silent, easy to make by habit, and hard to undo.

### Amendment to ADR-020: restoring `kg commit`

[ADR-020](ADR-020-git-native-kg-implementation.md) §5 specifies `kg commit -m <msg>` — export,
validate, `git add`, `git commit` — as part of the `kkernel kg` CLI surface. It was never
implemented; the shipped subcommand tree covers `validate`, `init`, `fetch` (aliased `sync`),
`export`, `import`, `status`, and hook management, but not `commit`. This ADR restores it, scoped
specifically to the tier-2 flow this ADR defines: `kkernel kg commit` runs validate against the
staged change-set's rules file, and on a clean pass performs the git add and commit that lands
an approved tier-2 change-set, carrying the provenance trailer [ADR-101](ADR-101-kg-changeset-model.md)
D4 defines. This is additive to ADR-020's CLI surface — no existing verb's behavior changes —
and the `commit` verb is scoped to this ADR's own local, non-remote repository (D6), not to
the project-repository-embedded `.khive/kg/` layout ADR-020 §1 otherwise describes.

---

## Consequences

Honest costs, stated rather than assumed away:

- **Two write paths to maintain.** Tier-1 and tier-2 are genuinely different code paths with
  different latency, different failure modes, and different testing needs. A bug specific to
  one tier does not automatically surface in the other, and both need their own regression
  coverage.
- **Review latency on tier-2.** Any operation routed to tier-2 — including a legitimate,
  correct `supersedes` edge from a well-behaved producer — waits on the cross-family review gate
  (D3) before it lands. This is the deliberate trade this ADR makes: correctness on
  judgment-bearing writes, at the cost of latency those writes did not previously pay when they
  went through the same live-write path as everything else.
- **The rules file becomes a contract surface.** Because the tier predicate lives in the rules
  file (D2) and both the headless CLI and any graphical reviewer evaluate it identically, the
  rules file is no longer a soft convenience — it is load-bearing configuration that determines
  which operations get reviewed. A malformed or misconfigured rules file is now a correctness
  risk (silently widening tier-1 past the ADR-002/ADR-055 floor is explicitly not permitted, per
  D2, but a bug in an implementation that fails to enforce that floor would be a real regression
  worth guarding with its own test).
- **No new edge relations, no new entity or note kinds.** This ADR mechanizes the existing
  ADR-002 contract — the tier predicate reads relation types and weights ADR-002 and
  [ADR-055](ADR-055-epistemic-edge-relations.md) already define — and introduces no schema
  change. Everything in D2's seed predicate is expressible against the closed taxonomies as they
  stand today.
- **`kg commit`'s restoration (Amendment to ADR-020) is scoped, not a general revival.** It
  lands specifically as the tier-2 commit primitive against this ADR's local-only repository
  (D6); it does not resurrect the whole of ADR-020 §5's original eleven-verb workflow or its
  project-repository-embedded layout, both of which remain a separate, larger piece of work if
  ever pursued.

---

## References

- ADR-002 — Closed Edge Ontology: the relation vocabulary and weight scale the tier predicate
  (D2) reads.
- ADR-010 — KG Versioning Strategy: the git-native strategic frame this ADR's commit and revert
  mechanics operate inside.
- ADR-020 — Git-Native KG Implementation: the `kg commit` primitive this ADR restores (Amendment
  to ADR-020), and the shipped `kkernel kg validate` rule-pass surface this ADR's tier predicate
  extends.
- ADR-034 — KG Validation Pipelines: the configurable rule-pass surface whose shipped rule-class
  extensions this ADR's tier predicate reads findings from.
- ADR-037 — Remote Entity Resolution and Content-Hash Verification: the atomic
  staging-then-rename pattern reused for writing the snapshot-commit lane's export before it is
  committed.
- ADR-055 — Epistemic Edge Relations: `supports`/`refutes` as two of the relations this ADR's
  tier predicate routes to tier-2 by construction.
- ADR-067 — Write-Owner Daemon: the single-writer, single-DB-handle discipline this ADR's
  topology section (D5) preserves by requiring MCP-client-only access to the live graph.
- ADR-101 — KG Change-Set Model: the op-list artifact this ADR validates, tiers, reviews, and
  commits.
