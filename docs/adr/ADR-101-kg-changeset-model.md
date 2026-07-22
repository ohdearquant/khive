# ADR-101: KG change-set model with a producer-agnostic op-list and stage-time stable IDs

**Status**: Accepted
**Date**: 2026-07-08
**Depends on**: ADR-002 (edge ontology), ADR-016 (request DSL: the op shape this format serializes), ADR-017 (pack standard), ADR-020 (git-native KG implementation: `KgArchive` as the import/export envelope this ADR does not replace)
**Related**: ADR-100 (store backup and replication: whole-graph snapshots, a different artifact from the change-set this ADR defines)

---

## Context

Writes into a khive knowledge graph can originate from interactive clients, import adapters, or
batch transformations. Any producer that does not write immediately needs a durable, inspectable
artifact between proposing a mutation and applying it to the graph. That artifact must support
validation, diffing, approval, and application as a unit.

The live `request` batch surface is not a typed staging format. Without a serializable
intermediate form, three problems follow:

1. **No stable cross-producer identifier.** If a producer references an entity or edge it is
   about to create later in the same batch (a `link` targeting a `create` earlier in the file),
   nothing mints that identifier until the write actually lands. A change-set that is staged,
   reviewed, and applied hours or days apart from when it was produced needs identifiers that
   are stable from the moment of staging, not assigned at commit time.
2. **No format a reviewer can render without executing it.** Rendering "what would this batch
   do" today means running it and inspecting the result, which is exactly the operation review
   exists to gate before it happens.
3. **No single artifact shape multiple producers and consumers can agree on.** Without
   a canonical staged format, each batch producer would grow its own ad hoc output shape, and
   each downstream tool (validator, diff renderer, review UI) would grow a parser per producer.

This ADR defines that artifact: a producer-agnostic change-set model, its serialization, and the
ingester pattern by which a producer's native output becomes one. [ADR-102](./ADR-102-tiered-validate-and-merge.md)
is the companion ADR that defines validation, review, application, and revert behavior. This ADR
is scoped to the artifact itself: what it is, how it is minted, how
it is serialized, and which crates own that model as durable, UI-agnostic logic.

### Why not extend `KgArchive`

[ADR-020](./ADR-020-git-native-kg-implementation.md) already defines `KgArchive` as the
in-memory import/export representation used by whole-graph export and import. It is tempting to
reuse it here, since both are "a bag of entities and edges." They solve different problems and
conflating them would corrupt both:

- `KgArchive` is a **state snapshot**: the full or filtered contents of the graph at a point in
  time, sorted and diffable as a whole document. It has no notion of an individual operation, no
  distinction between "this row is new" and "this row already existed," and no place to attach
  per-op provenance.
- A change-set is an **op-list**: a sequence of discrete, individually-typed mutations
  (`create`, `link`, `update`, `delete`, `merge`) that have not yet been applied. Its unit of meaning is
  the operation, not the resulting state.

`KgArchive` remains exactly what ADR-020 defines it as: the whole-graph import/export envelope.
It is explicitly **not** the change-set format, and this ADR introduces no change to its shape
or its consumers.

---

## Decision

### D1: Canonical staged artifact: a producer-agnostic typed op-list

> **Amended 2026-07-08**: `update` operations also capture a stage-time preimage, scoped to
> exactly the fields the operation changes rather than the full prior record. The requirement
> is stated in place below, alongside the `delete`/`merge` preimage requirement it extends.

The canonical staged artifact is a **typed op-list**: an ordered sequence of operations, each
one of `create`, `link`, `update`, `delete`, or `merge`, over the same entity/edge/note
vocabulary and edge-endpoint contract ADR-001/ADR-002/ADR-013 already define. It mirrors the
mutation surface the live request DSL (ADR-016) exposes, so nothing expressible as a live
write is inexpressible as a staged one. An op-list is producer-agnostic
by construction: it names no producer, encodes no producer-specific shape, and carries only
what any consumer (a rule evaluator, a diff renderer, a live-write applier) needs: the op kind,
its target substrate, its fields, the identifier(s) involved, and, for `update`, `delete`, and
`merge`, the stage-time preimage described below.

**Stage-time stable IDs are a hard requirement, not an optimization.** Every entity or note a
`create` op introduces is minted a UUID at the moment the op is staged, not at the moment it is
applied. A `link` op staged later in the same or a different change-set that targets that
identifier resolves correctly regardless of how much time elapses between staging and
application, and regardless of whether the two ops are ever applied together. This is what
makes cross-producer handoff possible: one producer can stage a `create`, and a second producer
or a human reviewer can stage a `link` against it before the first op has been applied,
because the identifier already exists and is stable. An identifier minted at apply time would
make that handoff impossible without an intermediate resolution step every consumer would have
to reimplement.

**Destructive operations capture their preimage at stage time.** A `delete` op records the
full prior state of the record it removes, and a `merge` op records both prior entities and
the incident edges the merge will rewire, as part of the staged operation itself. This is what
makes op-list inversion ([ADR-102](./ADR-102-tiered-validate-and-merge.md) D5) well-defined for
destructive operations: an inverse can only restore what was captured. An operation staged
without its preimage cannot be surgically reverted, and any revert of it must fall back to the
coarser mechanisms ADR-102 D5 names.

**`update` operations capture a field-scoped preimage at stage time.** An `update` op's
preimage records the prior value of exactly the fields its patch touches, including every field the
patch sets to a new value and every field it explicitly clears to null, but no field the
patch leaves unchanged. This keeps preimage cost proportional to the size of the write rather
than the size of the record: a one-field rename on an entity carrying a large `properties` map
captures one prior value, not the whole entity. The field-scoped preimage makes
[ADR-102](./ADR-102-tiered-validate-and-merge.md) D5's inversion claim for `update`, which restores
the prior field values captured at stage time, true rather than aspirational. An `update` staged
without this field-scoped preimage cannot be surgically
inverted and falls back to the same coarser mechanisms a preimage-less `delete` or `merge`
would.

**The change-set envelope is not a routing policy.** The envelope carries the format version and
stage-time metadata required to decode and audit the artifact. Its concrete field set is owned by
the versioned `khive-changeset` format. Consumers must not infer approval requirements or select a
review path from producer implementation details. Validation and approval depend only on the
operations, their preimages, and rule findings defined by ADR-102.

### D2: Serialization: NDJSON-delta

A change-set serializes as **NDJSON-delta**: the first line is the versioned envelope, followed by
one JSON object per operation in stage order. This is a deliberate departure from `KgArchive`'s
sorted-by-primary-key, whole-state NDJSON (ADR-020 §2). A change-set's ordering is operation order,
not a diffable canonical sort, because operation order is semantically load-bearing (a `link` can depend on an
earlier `create` in the same file) in a way a state snapshot's row order is not. Each line
carries enough to be applied independently of the file's byte layout: op kind, target kind,
resolved or newly-minted identifiers, the op's fields, and, for `update`, `delete`, and
`merge`, the captured stage-time preimage(s) D1 requires (field-scoped to the changed fields
for `update`, full-record for `delete` and `merge`). The concrete field-level shape is
implementation detail of the `khive-changeset` crate (D4) and is not pinned by this ADR beyond
the op-kind/identifier/fields shape above; it evolves as an internal format under the crate's
own versioning, not as a wire contract this ADR freezes.

### D3: Ingester pattern: producers convert native output into the op-list

A change-set producer does not need to emit NDJSON-delta directly from its own representation.
It can emit a native shape, and an **ingester** converts that native output into the canonical
op-list. This keeps the op-list format
independent of any one producer's internals and lets a new producer be onboarded by writing one
ingester, not by reshaping the format to fit it.

Ingester examples include a bulk-import adapter and a corpus-transformation adapter. Each ingester
maps its input into the same typed operations and stage-time identifiers. Nothing in the op-list
model, NDJSON-delta serialization, or crates in D4 encodes an ingester-specific control flow.

### D4: Three UI-agnostic crates, no filesystem access, wasm32-compilable

Graduate exactly three crates from day one, each carrying durable model or logic and none
carrying a view:

1. **`khive-changeset`**: the op-list type and its NDJSON-delta serialization (D1, D2). The
   data model has no knowledge of any producer, ingester, or UI.
2. **The rule evaluator**: the pure function from a change-set plus a rules definition to a set
   of pass/fail findings. [ADR-102](./ADR-102-tiered-validate-and-merge.md) defines the rules;
   this crate is the engine shared by a headless CLI and any graphical reviewer.
3. **Diff computation**: a pure function from two NDJSON states to a structured graph-diff (the
   set of entity/edge/note-level additions, removals, and field changes between them), used both
   to render a change-set's effect before it is applied and to render an applied change-set after
   the fact.

**Constraint, binding on all three**: no filesystem access and no I/O of any kind inside the
crate boundary: every function takes its input as an in-memory value and returns an in-memory
value. Each crate must be **wasm32-compilable**, and CI carries a **wasm-parity check**: the
same test suite run against the native target and the wasm32 target must produce byte-identical
results, and the CI job fails on any divergence between the two.

**Rationale.** A view layer, such as a CLI renderer, desktop review UI, or future web frontend, is
replaceable and, at this stage of the project, is expected to be replaced more than once. The
op-list model, the rule evaluation logic, and the diff computation are not: they are the
durable asset a producer, a reviewer, and a future UI all depend on agreeing about. Keeping
them filesystem-free and wasm-compilable from day one is what keeps that guarantee real rather
than aspirational: a crate that quietly grows a filesystem read is a crate a future web
frontend cannot reuse without discovering the dependency the hard way. The CI wasm-parity check
makes the constraint enforced, not just documented.

---

## Consequences

- A producer gains a real staging artifact instead of the live write surface as its only
  option, and gains it without the op-list model knowing anything about that producer
  specifically: the ingester pattern (D3) is the only producer-specific surface.
- Cross-producer and cross-time handoff (one producer's `create`, referenced by a `link` staged
  later, by the same or a different actor) is correct by construction because of stage-time ID
  minting (D1), at the cost of every ingester having to mint and track identifiers itself rather
  than deferring that to apply time.
- Three additional crates (D4) are now durable public surface with a wasm-parity CI obligation;
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

- ADR-001: Entity Kind Taxonomy: the entity vocabulary an op-list's `create`/`link`/`update`
  ops draw from.
- ADR-002: Closed Edge Ontology: the edge-relation vocabulary and endpoint contract a `link` op
  must satisfy.
- ADR-013: Note Kind Taxonomy: the note vocabulary an op-list's note-targeting ops draw from.
- ADR-016: Request DSL: the live op shape (`create`/`link`/`update`/`delete`/`merge` verbs) this
  format's op kinds mirror.
- ADR-020: Git-Native KG Implementation: `KgArchive` as the retained whole-graph import/export
  envelope this ADR explicitly does not replace or extend.
- ADR-102: Tiered Validate-and-Merge: the companion ADR defining what happens to a change-set
  once it exists (validation, tiering, review, application, and revert).
