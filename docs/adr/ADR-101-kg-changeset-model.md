# ADR-101: KG Change-Set Model — Producer-Agnostic Op-List with Stage-Time Stable IDs

**Status**: Proposed
**Date**: 2026-07-08
**Depends on**: ADR-002 (edge ontology), ADR-016 (request DSL — the op shape this format serializes), ADR-017 (pack standard), ADR-020 (git-native KG implementation — `KgArchive` as the import/export envelope this ADR does not replace)
**Related**: ADR-088 (git-lifecycle pack — the future-work loop below), ADR-100 (store backup and replication — whole-graph snapshots, a different artifact from the change-set this ADR defines)

---

## Context

Writes into a khive knowledge graph arrive from more than one kind of producer: an interactive
agent issuing MCP `request` ops directly against the live daemon, and periodic batch
producers — extraction pipelines that scan a corpus on a schedule and emit a block of proposed
graph changes for later application. The second class needs a durable, inspectable staging
artifact between "a producer decided what to write" and "the write lands in the live graph,"
so that the change can be diffed, reviewed, and committed as a unit rather than applied as a
sequence of independent, unreviewable side effects.

No such artifact exists today. A batch producer's only path into the graph is the same live
`request` batch surface an interactive agent uses — there is no typed, serializable
intermediate form a producer can emit, a reviewer can render, and a later stage can apply.
Three problems follow directly from that gap:

1. **No stable cross-producer identifier.** If a producer references an entity or edge it is
   about to create later in the same batch (a `link` targeting a `create` earlier in the file),
   nothing mints that identifier until the write actually lands. A change-set that is staged,
   reviewed, and applied hours or days apart from when it was produced needs identifiers that
   are stable from the moment of staging, not assigned at commit time.
2. **No format a reviewer can render without executing it.** Rendering "what would this batch
   do" today means running it and inspecting the result, which is exactly the operation review
   exists to gate before it happens.
3. **No single artifact shape multiple producers and multiple consumers can agree on.** Without
   a canonical staged format, each batch producer would grow its own ad hoc output shape, and
   each downstream tool (validator, diff renderer, review UI) would grow a parser per producer.

This ADR defines that artifact: a producer-agnostic change-set model, its serialization, the
ingester pattern by which a producer's native output becomes one, and the provenance carried on
commit. [ADR-102](ADR-102-tiered-validate-and-merge.md) is the companion ADR that defines what
happens to a change-set once it exists — validation, the tier split, review, and the commit and
revert mechanics. This ADR is scoped to the artifact itself: what it is, how it is minted, how
it is serialized, and which crates own that model as durable, UI-agnostic logic.

### Why not extend `KgArchive`

[ADR-020](ADR-020-git-native-kg-implementation.md) already defines `KgArchive` as the
in-memory import/export representation used by whole-graph export and import. It is tempting to
reuse it here, since both are "a bag of entities and edges." They solve different problems and
conflating them would corrupt both:

- `KgArchive` is a **state snapshot** — the full or filtered contents of the graph at a point in
  time, sorted and diffable as a whole document. It has no notion of an individual operation, no
  distinction between "this row is new" and "this row already existed," and no place to attach
  per-op provenance.
- A change-set is an **op-list** — a sequence of discrete, individually-typed mutations
  (`create`, `link`, `update`, `delete`, `merge`) that have not yet been applied. Its unit of meaning is
  the operation, not the resulting state.

`KgArchive` remains exactly what ADR-020 defines it as: the whole-graph import/export envelope.
It is explicitly **not** the change-set format, and this ADR introduces no change to its shape
or its consumers.

---

## Decision

### D1 — Canonical staged artifact: a producer-agnostic typed op-list

> **Amended 2026-07-08**: `update` operations also capture a stage-time preimage, scoped to
> exactly the fields the operation changes rather than the full prior record. The requirement
> is stated in place below, alongside the `delete`/`merge` preimage requirement it extends.

The canonical staged artifact is a **typed op-list**: an ordered sequence of operations, each
one of `create`, `link`, `update`, `delete`, or `merge`, over the same entity/edge/note
vocabulary and edge-endpoint contract ADR-001/ADR-002/ADR-013 already define — the same
mutation surface the live request DSL (ADR-016) exposes, so nothing expressible as a live
write is inexpressible as a staged one. An op-list is producer-agnostic
by construction — it names no producer, encodes no producer-specific shape, and carries only
what any consumer (a rule evaluator, a diff renderer, a live-write applier) needs: the op kind,
its target substrate, its fields, the identifier(s) involved, and — for `update`, `delete`, and
`merge` — the stage-time preimage described below.

**Stage-time stable IDs are a hard requirement, not an optimization.** Every entity or note a
`create` op introduces is minted a UUID at the moment the op is staged, not at the moment it is
applied. A `link` op staged later in the same or a different change-set that targets that
identifier resolves correctly regardless of how much time elapses between staging and
application, and regardless of whether the two ops are ever applied together. This is what
makes cross-producer handoff possible: one producer can stage a `create`, and a second producer
— or a human reviewer — can stage a `link` against it before the first op has been applied,
because the identifier already exists and is stable. An identifier minted at apply time would
make that handoff impossible without an intermediate resolution step every consumer would have
to reimplement.

**Destructive operations capture their preimage at stage time.** A `delete` op records the
full prior state of the record it removes, and a `merge` op records both prior entities and
the incident edges the merge will rewire, as part of the staged operation itself. This is what
makes op-list inversion ([ADR-102](ADR-102-tiered-validate-and-merge.md) D4) well-defined for
destructive operations: an inverse can only restore what was captured. An operation staged
without its preimage cannot be surgically reverted, and any revert of it must fall back to the
coarser mechanisms ADR-102 D4 names.

**`update` operations capture a field-scoped preimage at stage time.** An `update` op's
preimage records the prior value of exactly the fields its patch touches — every field the
patch sets to a new value and every field it explicitly clears to null — and no field the
patch leaves unchanged. This keeps preimage cost proportional to the size of the write rather
than the size of the record: a one-field rename on an entity carrying a large `properties` map
captures one prior value, not the whole entity. It is also what makes
[ADR-102](ADR-102-tiered-validate-and-merge.md) D4's inversion claim for `update` — that its
inverse "restores the prior field values captured at stage time" — true rather than
aspirational. An `update` staged without this field-scoped preimage cannot be surgically
inverted and falls back to the same coarser mechanisms a preimage-less `delete` or `merge`
would.

**Operations are producer-agnostic; the change-set envelope is attributed.** While no
individual operation names a producer, the change-set as a whole carries a small **envelope
metadata block**, captured at stage time, recording the producer's identity and its model
family. The envelope exists for exactly two consumers: reviewer routing (ADR-102 D3's
cross-family gate takes the producer's model family as its input) and commit provenance (D4
below). No op-level consumer — rule evaluator, diff renderer, applier — reads it, so the
op-list itself stays producer-agnostic.

### D2 — Serialization: NDJSON-delta

A change-set serializes as **NDJSON-delta**: one JSON object per line, one line per operation,
in stage order. This is a deliberate departure from `KgArchive`'s sorted-by-primary-key,
whole-state NDJSON (ADR-020 §2) — a change-set's ordering is operation order, not a diffable
canonical sort, because operation order is semantically load-bearing (a `link` can depend on an
earlier `create` in the same file) in a way a state snapshot's row order is not. Each line
carries enough to be applied independently of the file's byte layout: op kind, target kind,
resolved or newly-minted identifiers, the op's fields, and — for `update`, `delete`, and
`merge` — the captured stage-time preimage(s) D1 requires (field-scoped to the changed fields
for `update`, full-record for `delete` and `merge`). The concrete field-level shape is
implementation detail of the `khive-changeset` crate (D5) and is not pinned by this ADR beyond
the op-kind/identifier/fields shape above; it evolves as an internal format under the crate's
own versioning, not as a wire contract this ADR freezes.

### D3 — Ingester pattern: producers convert native output into the op-list

A change-set producer does not emit NDJSON-delta directly from its own internal representation.
It emits its **native output** — whatever shape is natural to how it works — and an **ingester**
converts that native output into the canonical op-list. This keeps the op-list format
independent of any one producer's internals and lets a new producer be onboarded by writing one
ingester, not by reshaping the format to fit it.

The first ingester converts a periodic extraction-pipeline's block output — a batch of proposed
entities, edges, and notes recovered from scanning a corpus on a schedule — into the canonical
op-list. This producer's adoption is unblocked by shipping first, and it is treated as exactly
that: the first of what is expected to be several ingesters (an interactive-agent op recorder,
a future bulk-import adapter), not a privileged or format-defining one. Nothing in the op-list
model, the NDJSON-delta serialization, or the crates in D5 encodes anything specific to this
producer; the ingester is where producer-specific translation lives, and it is the only layer
that is.

### D4 — Provenance: batch description in the commit message, batch identifier as a commit trailer

When a change-set is committed (per [ADR-102](ADR-102-tiered-validate-and-merge.md)'s tier-2
flow), the git commit that lands it carries provenance at two levels:

- The **commit message body** carries a human-readable description of the batch — what was
  produced and why, in the producer's own words.
- A **producer-assigned batch identifier** rides as a commit trailer (a standard
  `Key: value` trailer line, in the convention `git interpret-trailers` already recognizes).
  The identifier is opaque to this ADR — it is whatever token the producer uses internally to
  track the unit of work that generated the change-set — and its only contract here is that it
  round-trips: a reader of the commit can extract it verbatim and correlate it back to the
  producer's own tracking, without this ADR or the graph needing to understand what it means.

This is deliberately generic. The mechanism by which a specific producer generates that
description or that identifier — its own scheduling, batching, or tasking model — is out of
scope for a public architectural contract and is not named here.

### D5 — Three UI-agnostic crates, no filesystem access, wasm32-compilable

Graduate exactly three crates from day one, each carrying durable model or logic and none
carrying a view:

1. **`khive-changeset`** — the op-list type and its NDJSON-delta serialization (D1, D2): the
   data model this ADR defines, with no knowledge of any producer, ingester, or UI.
2. **The rule evaluator** — the pure function from a change-set plus a rules definition to a set
   of pass/fail findings. [ADR-102](ADR-102-tiered-validate-and-merge.md) defines the rules
   themselves and how they route to tiers; this crate is the engine that runs them, shared
   identically by a headless CLI and any graphical reviewer.
3. **Diff computation** — a pure function from two NDJSON states to a structured graph-diff (the
   set of entity/edge/note-level additions, removals, and field changes between them), used both
   to render a change-set's effect before it is applied and to render the effect of a commit
   after the fact.

**Constraint, binding on all three**: no filesystem access and no I/O of any kind inside the
crate boundary — every function takes its input as an in-memory value and returns an in-memory
value. Each crate must be **wasm32-compilable**, and CI carries a **wasm-parity check**: the
same test suite run against the native target and the wasm32 target must produce byte-identical
results, and the CI job fails on any divergence between the two.

**Rationale.** A view layer — a CLI renderer, a desktop review UI, a future web frontend — is
replaceable and, at this stage of the project, is expected to be replaced more than once. The
op-list model, the rule evaluation logic, and the diff computation are not: they are the
durable asset a producer, a reviewer, and a future UI all depend on agreeing about. Keeping
them filesystem-free and wasm-compilable from day one is what keeps that guarantee real rather
than aspirational — a crate that quietly grows a filesystem read is a crate a future web
frontend cannot reuse without discovering the dependency the hard way. The CI wasm-parity check
makes the constraint enforced, not just documented.

---

## Future work (non-binding)

A change-set, once committed, is itself graph-shaped history: a sequence of typed operations,
each with provenance, applied to a graph. [ADR-088](ADR-088-git-lifecycle-pack.md) already
defines a git-lifecycle pack that ingests a git repository's own commit, issue, and
pull-request history into the graph as `commit`, `issue`, and `pull_request` note kinds. The natural composition — not committed to by this
ADR, and explicitly out of scope for the implementation this ADR authorizes — is a loop in which
the git-lifecycle pack ingests the KG's own change-set commits (the artifact this ADR defines)
back into the same graph, making the graph's own change provenance queryable as graph: which
producer proposed which operation, when, and under what batch. This would let a query answer
"what changed this entity and why" as a graph traversal rather than as a manual read of git
history. It is named here as the direction this design is deliberately compatible with, not as
a commitment this ADR or its implementation makes.

---

## Consequences

- A batch producer gains a real staging artifact instead of the live write surface as its only
  option, and gains it without the op-list model knowing anything about that producer
  specifically — the ingester pattern (D3) is the only producer-specific surface.
- Cross-producer and cross-time handoff (one producer's `create`, referenced by a `link` staged
  later, by the same or a different actor) is correct by construction because of stage-time ID
  minting (D1), at the cost of every ingester having to mint and track identifiers itself rather
  than deferring that to apply time.
- Three additional crates (D5) are now durable public surface with a wasm-parity CI obligation;
  this is a maintenance cost taken on deliberately in exchange for keeping the model, rules, and
  diff logic reusable by a view layer not yet built.
- `KgArchive` (ADR-020) is unchanged and remains the whole-graph import/export envelope; this
  ADR does not touch its shape, its consumers, or the boundary between it and the change-set
  model defined here.
- The concrete field-level schema of an NDJSON-delta line is intentionally left to the
  `khive-changeset` crate rather than pinned in this ADR text; a future amendment is expected
  once the schema stabilizes under real ingester and reviewer use.

---

## References

- ADR-001 — Entity Kind Taxonomy: the entity vocabulary an op-list's `create`/`link`/`update`
  ops draw from.
- ADR-002 — Closed Edge Ontology: the edge-relation vocabulary and endpoint contract a `link` op
  must satisfy.
- ADR-013 — Note Kind Taxonomy: the note vocabulary an op-list's note-targeting ops draw from.
- ADR-016 — Request DSL: the live op shape (`create`/`link`/`update`/`delete`/`merge` verbs) this
  format's op kinds mirror.
- ADR-020 — Git-Native KG Implementation: `KgArchive` as the retained whole-graph import/export
  envelope this ADR explicitly does not replace or extend.
- ADR-088 — Git-Lifecycle Pack: the commit/issue ingestion this ADR's future-work section
  proposes composing with.
- ADR-102 — Tiered Validate-and-Merge: the companion ADR defining what happens to a change-set
  once it exists (validation, tiering, review, commit, revert).
