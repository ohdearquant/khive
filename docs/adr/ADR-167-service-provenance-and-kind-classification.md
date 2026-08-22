# ADR-167: Service Provenance, Service/Concept Classification, and Kind Migration

**Status**: proposed\
**Date**: 2026-08-20\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-002](ADR-002-edge-ontology.md) — the closed 17-relation set and the base endpoint contract
- [ADR-055](ADR-055-epistemic-edge-relations.md) — same-substrate restriction on `supports` / `refutes`

**Amends**:

- [ADR-002](ADR-002-edge-ontology.md) — add one derivation pair, `Service introduced_by Document`
- [ADR-001](ADR-001-entity-kind-taxonomy.md) — add a deterministic tie-break to the
  classification decision tree for the `service` / `concept` boundary

## Context

Three findings from operating a production store, stated with what was measured rather than
inferred. The store holds 19 entities of kind `service`.

**1. A service cannot record where it came from.** The base endpoint contract admits no pair in
which a `Service` participates in a derivation or provenance relation with a `Document`. A
service may be contained by an org, be an instance of a project, supersede another service, and
relate to concepts through `enables` / `implements` / `instance_of`. It cannot point at the
specification, ADR, or paper that introduced it. The kinds that can are `Concept`, `Artifact` and
`Document` — those are the only `introduced_by` sources the contract admits, and `Service` sits
outside that set. The gap does not look deliberate — the 2026-07-08 amendment that added three
`introduced_by` pairs was reasoning about documents and orgs and did not consider services.

The practical consequence is that a service's origin gets recorded as an annotation on a note
instead of as an edge, where it is invisible to lineage traversal.

**2. The service-versus-concept split is under-determined at exactly the boundary that matters.**
[ADR-001](ADR-001-entity-kind-taxonomy.md) §"Agent Classification Heuristics" already provides an
ordered decision tree (a running operational instance resolves to `Service` at step 5 before an
abstract idea resolves to `Concept` at step 8) and a signal table for each kind. What it does not
resolve is the case the store actually mishandled: a deployable system that is not currently
running, where "running operational instance" reads false and the record falls through to
`Concept` even though its endpoint needs are `Service`-shaped. The two kinds have materially
different endpoint contracts, so that fall-through silently decides what edges the record will
ever be allowed to carry. The gap is a missing tie-break rule inside ADR-001's existing
procedure, not a missing procedure.

**3. Kind is immutable, so a misclassification is only correctable by delete-and-recreate, and
that path destroys edges under the only deletion mode that can actually free the record.**
Measured against the 19-service population: 16 carry at least one edge and 3 carry none. A
HARD delete-and-recreate across the population would destroy **62 edges**, of which **42 are
`annotates`** — the figure is stated for `DeleteMode::Hard` specifically, because soft deletion
destroys none of them and instead strands all 62 on a deleted endpoint (Decision 3). Because each of those 42 runs note → entity in the measured
population (the contract itself admits edge targets for `annotates` as well — the closure
requirement in Decision 3 exists for exactly that case), each destroyed annotates
edge leaves a note whose subject no longer exists — the note survives, saying something about
nothing, which is worse than either deleting it or keeping it attached.

Enumerating that blast radius has a complication of its own, because an edge may itself be an
endpoint. `endpoint_exists_clause` in `crates/khive-db/src/stores/graph.rs` admits an undeleted
`graph_edges` row as a valid endpoint alongside entities, notes and events, so an `annotates` edge
may point AT another edge, and those rows are part of any migration's affected set. The inbound
filter `list(kind="edge", target_id=…)` does reach them: the earlier report that it returns empty
(issue #2085) has been retracted, and an integration test covering the edge-as-target case matches.

What remains open is exhaustive ENUMERATION, which is a different question from whether the filter
matches. Two independent gaps stand between the listing surface and the set the purge will delete.

First, offset paging over multiple namespaces was unsound when this ADR was drafted. Issue #2088
recorded that the multi-namespace visibility path fetched each namespace's rows ordered by
`created_at`, re-sorted the union by UUID, and then sliced `[offset, offset+limit)`: the window
floats as the prefix grows, so successive pages both duplicate and skip rows while reporting
nothing wrong, in both filter directions (`source_id` and `target_id` share the listing path).
That defect has since been fixed: the multi-namespace branch now issues a single statement over
the whole namespace set with a real `LIMIT`/`OFFSET` (the change that closed #2088), and the
exact-enumeration and page-stability regressions are owned by that change's own tests. This ADR
does not re-own them; the defect remains in this record because it is measured evidence of how
quietly an enumeration can be incomplete, which is the failure class the contract below exists
to exclude.

Second — and this holds even now that the listing paths are sound — the listing surface answers
a narrower question than the purge asks. `list_edges_after` (`crates/khive-runtime/src/operations.rs`) walks
the durable insertion-sequence ledger correctly, but it is visibility-scoped to the caller's
namespaces and, like every listing path, filters to live rows (`deleted_at IS NULL`). The purge is
`DELETE FROM graph_edges WHERE source_id = ?1 OR target_id = ?1` — no namespace predicate, no
tombstone predicate. An incident edge in a namespace the migrating caller cannot see, or one
already soft-deleted, passes a visible/live enumeration and its matching count while the purge
deletes it anyway. A completeness check scoped narrower than the destructive statement it guards
is not a completeness check.

The enumeration contract in Decision 3 is therefore stated against the purge's own predicate: the
authoritative enumeration is a direct query executed inside the migration's own transaction using
the same `source_id = ?1 OR target_id = ?1` predicate as the purge, with no visibility scoping and
no live-row filter, so the enumerated set equals the destructive set by construction. The cursor
walk remains the sound primitive for caller-facing pre-checks and planning reads; it is not the
completeness instrument.

## Decision

### 1. Add one derivation pair

Add to the base endpoint contract, Derivation relations:

| Source    | Relation        | Target     |
| --------- | --------------- | ---------- |
| `Service` | `introduced_by` | `Document` |

Direction is unchanged from every other `introduced_by` row: the source is the thing whose origin
is being recorded, the target is the origin.

This is deliberately one pair and not a class of them. The wider forms considered and **rejected
for lack of evidence** are `Service introduced_by Person`, `Service introduced_by Org`, and any
`derived_from` pair involving a service. `Org contains Service` already carries organizational
attribution, and no measured refusal in the store's rejection record asked for the `derived_from`
forms. Adding pairs that nothing has asked for widens the contract that ADR-002's "why closed"
rationale exists to keep narrow.

The relation set stays at 17. The closed-set property is unaffected; this is one row in the
endpoint table.

### 2. Amend ADR-001's decision tree with a deterministic service/concept tie-break

ADR-001 §"Agent Classification Heuristics" stays the single classification source. This ADR does
not introduce a parallel set of criteria; it amends ADR-001 in place — this PR carries the
ADR-001 edit, and the operative text is ADR-001's new §"Service/concept tie-break". The
amendment replaces step 5's condition with a sub-procedure over two predicates, each decidable
from the record being written (its name, description, `entity_type`, and properties), with no
reference to the writer's state of mind or to the world at write time:

- **Instance evidence (D)**: defined normatively in ADR-001 §"Service/concept tie-break" — the
  record names at least one concrete instance identifier stated in the record. ADR-001's
  statement (including its specific host/region/cluster qualification of deployment surfaces,
  the property-field requirement for operational state, and the exclusion of bare
  deployment/liveness vocabulary) is the single normative text; this ADR deliberately does not
  restate it, so the two documents cannot diverge.
- **Technique identity (T)**, evaluated only when D holds: the record's own text names a
  technique as its referent — its `entity_type` is one of `Concept`'s canonical subtypes, or
  its name or description names the referent with one of ADR-001 step 8's own designators
  (`idea`, `method`, `algorithm`, `theory`, `architecture`, `research gap`, `metric`) or
  with a `Concept` canonical-subtype name. A vocabulary-presence test on the record's
  fields reusing vocabulary ADR-001 already owns, not an inference about the prose.

The arms partition on D, then on T, so they are mutually exclusive by construction: D absent →
step 5 does not fire and the walk continues to step 6 (a codebase classifies `Project`, a pure
technique reaches step 8 and classifies `Concept`); D without T → `Service`; D with T → the
split is **mandatory** — two records, `Concept` for the technique and `Service` for the
deployment, joined by `Service instance_of Concept`, and single-record classification either
way is out of contract. Step 9's uncertainty default is unchanged; the amendment adds a
question-note requirement whose trigger (deployment vocabulary present, instance identifier
absent) is likewise read off the record.

The earlier draft of this section stated the procedure as four ordered first-match rules whose
split arm required two earlier arms to hold simultaneously — unreachable under first-match —
and whose predicates ("meaningful to say", "when deployed would have", "the writer cannot state
which") were not decidable from the record; its broad liveness arm could also claim a static
codebase that ADR-001 step 6 assigns to `Project`. The partition form above closes all three
defects: no arm can be masked by ordering, every predicate is a presence test on the record's
own fields and vocabulary, and codebase identity is routed to step 6 before `Service` can be
reached.

### 3. Kind migration: keep delete-and-recreate, and require a re-anchor plan

We propose **not** adding a kind-migration mechanism, and instead requiring that any
delete-and-recreate carries a written re-anchor plan. The reasoning is priced against the
measured number rather than asserted.

A migration mechanism would have to re-point every inbound and outbound edge, preserve edge ids
so that anything referencing an edge stays valid, and preserve `annotates` targets across the
substrate boundary. That is a substantial amount of runtime surface, and the population it would
serve is small: kind changes are rare, and the store measured 19 services in total.

Against that, the cost of the status quo is bounded and known — 62 edges across a whole-population
hard migration, 42 of them annotations. For a single-record correction the cost is that record's own
edge count, which is typically small.

So the proposal is a **procedure**, not a mechanism. Two things have to be pinned down before the
steps make sense, because the earlier draft of this section left both implicit and the procedure was
unsound without them.

**Deletion mode, and what each mode actually does.** `DeleteMode` (`crates/khive-storage/src/types/mod.rs`)
has two variants and they differ in exactly the way that matters here. `Soft` marks `deleted_at` and
leaves the row queryable under an explicit soft-delete filter. `Hard` physically removes the row **and
cascades incident edges** — `purge_incident_edges_statement` issues
`DELETE FROM graph_edges WHERE source_id = ?1 OR target_id = ?1`, unconditionally and with no soft
counterpart. For entity and note targets, the statement order inside the atomic plan is: the record's
own row delete first (under an exactly-one-row guard), lineage-warning statements second, and the
incident-edge purge last of those three; the plan then appends FTS/vector index-purge statements and
the deletion event after the purge (`crates/khive-runtime/src/operations.rs`,
`crates/khive-runtime/src/atomic_prepare.rs` — both build this order for those targets). Nothing in a
migration plan may therefore rely on the old record row still existing when the purge runs; it is
already gone. The edge-as-node branch orders differently: when the deleted record is itself an edge,
`atomic_prepare.rs` emits the lineage warnings first, purges incident edges second, and hard-deletes
the edge row last of those three (the deletion event is appended after the row delete) — so a
migration step deleting an edge record may still observe the edge row while
its incident purge runs. Because this ADR treats edges as valid provenance endpoints, a migration
plan that touches edge records must be written against the edge branch's order, not the entity/note
order. Either way the order is invisible to callers only because the whole plan commits in a single
synchronous pass — which is one more reason the atomicity requirement below is load-bearing rather
than advisory.

Two consequences, and the first is a correction to this ADR's own framing. **The 62-edge figure is a
HARD-delete figure.** A soft delete destroys no edges at all; it leaves every one of them as a live row
pointing at a record that now reads as deleted. So soft deletion is not the cheap option it appears to
be — it converts edge destruction into a set of edges whose endpoint no longer satisfies
`endpoint_exists_clause`, which is a worse state than either outcome, because nothing reports it. And
hard deletion is genuinely irreversible: the cascade is a `DELETE`, there is no `deleted_at` to
restore from, and edges are not FTS or vector indexed so no secondary copy exists.

**Therefore the whole correction is one atomic unit, or it is not attempted.** The runtime already has
the seam: `atomic_prepare` builds a plain-data `AtomicOpPlan` that `run_atomic_unit` commits in a
single synchronous pass, with a per-statement `AffectedRowGuard`. A kind correction is prepared as one
such plan covering deletion and every recreation, so a failure after the delete rolls the delete back
with it. **A correction executed as a sequence of independent calls is out of contract**, because the
window between the cascade and the last recreation is exactly where the edges are unrecoverable.

**Edges whose triples become ILLEGAL under the new kind.** Recreation is not always available, and this
is not an edge case: `Org contains Service` has no `Org contains Concept` counterpart in the endpoint
matrix, so an org-contained service cannot simply be recreated as a concept. Every incident edge must
therefore be classified against ADR-002's matrix BEFORE anything is deleted, into exactly four
dispositions for a live edge — a tombstoned edge takes the tombstone-only outcomes below
instead — each of which must be written down per edge:

- **PRESERVE** — the edge is live, the resulting triple is legal, and an id-preserving endpoint
  move (ADR-113's `move_edge_endpoint`) is available in the running system and collision-free for
  this edge. A tombstoned edge never qualifies (see the tombstone rule below). Liveness is a
  precondition this ADR imposes on the disposition and checks before the plan is prepared; it is
  not inherited from the move primitive. ADR-113 specifies id preservation, canonicalization,
  collision failure, and endpoint validation, and it does not read `deleted_at`, so a move issued
  against a tombstoned row is not refused downstream — only this check stops it. The
  edge keeps its id while the endpoint fields referencing the moved subject are updated (the
  primitive takes one or both fields): for an incident edge, every endpoint field referencing the
  migrating record moves onto the new record — one field ordinarily, both for a self-loop; for a
  closure member, the target moves to its annotated subject's mapped replacement — a closure
  member is never moved onto the new record. A closure
  member whose subject is itself preserved needs no move at all: its target id is still valid.
  Because the id survives, every annotation targeting a PRESERVEd edge stays valid with no
  re-anchoring. Preferred over RECREATE wherever it qualifies. It exempts nothing: the edge still
  appears in the closure enumeration, its disposition is still written down, and any move still
  executes inside the same atomic plan as the rest of the correction.
- **RECREATE** — the triple is legal under the new kind. Recreate and read back.
- **RE-EXPRESS** — the triple is illegal but the fact survives under a different relation or a
  different endpoint. Name the replacement triple and why it carries the same claim.
- **REFUSE** — the fact has no legal expression under the new kind. This does not mean drop the edge;
  it means **the migration does not proceed** on that record until someone decides, on the record,
  either to accept the loss with a named carrier for the fact or to leave the kind as it is.

The default is REFUSE. A procedure whose failure mode is a silently dropped edge is the thing this
ADR exists to prevent, so an unclassifiable edge stops the migration rather than being absorbed by it.

Two rules sit beside the four dispositions and are written down with the same discipline:

**Annotation dispositions.** The four dispositions above classify an edge's TRIPLE against the
endpoint matrix. Separately, every annotation in the closure — an `annotates` edge together with
the note it anchors — carries its own recorded disposition: **re-anchor** (to the target read off
the closure-substrate map), **delete** (the annotation is removed with its subject), or **carrier**
(the note is re-anchored to a named carrier record that preserves the claim). A disposition names
its outcome for both rows it governs, because they are separate substrates: re-anchor updates the
`annotates` edge's target and retains the note unchanged; delete removes the `annotates` edge and
states explicitly whether the note is deleted with it or retained as an orphaned free-standing
note; carrier retains the note and re-points the `annotates` edge at the named carrier. Each
deletion is written into the plan per row before anything executes. This is not REFUSE's forbidden
"drop the edge": a REFUSE is an unclassifiable fact stopping the migration, while an annotation
deletion is a decided, per-row, recorded outcome.

Two recorded sources, and only these two, produce a **named deletion** entry in the
closure-substrate map (step 3): an annotation whose recorded disposition is delete, and an edge
carrying the NAMED-DELETION tombstone outcome below. A RE-EXPRESSed edge's original is also
deleted, but it appears in the map as the mapping to its replacement, not as a deletion entry.
Nothing else in the procedure deletes a row outside the record's own purge cascade.

**Tombstone rule.** The purge set includes tombstoned edges, so they receive dispositions like
every other row, but no disposition changes their deletedness. A tombstoned edge is never
PRESERVEd — the liveness precondition on that disposition is this ADR's, checked here, not a
property the move primitive enforces — and is never recreated live: clearing or omitting
`deleted_at` on the replacement row would resurrect an edge someone deleted. A tombstoned edge
therefore takes one of two recorded outcomes, or REFUSE if neither applies:

- **RECREATE-TOMBSTONED** — the triple is legal under the new kind, and the replacement row is
  created carrying forward the same `deleted_at` value the original held. The map records the
  original's id mapping to the tombstoned replacement.
- **NAMED-DELETION** — the fact is not carried forward. The map records a named deletion for that
  edge id with the tombstone noted, and the plan carries the explicit `DELETE` statement for it.

NAMED-DELETION is available to tombstoned edges only; a live edge with no legal expression is a
REFUSE, which stops the migration, never a deletion. An annotation targeting a tombstoned edge
follows the map like any other.

With those settled, a kind correction:

1. Enumerates the **complete purge set** before deleting anything: a direct query inside the
   migration's own transaction using the purge's own predicate
   (`source_id = ?1 OR target_id = ?1`), with **no visibility scoping and no live-row filter** —
   every namespace, tombstoned rows included, because the purge deletes across both (see the
   enumeration note in Context). The collected edge IDs are deduplicated (an edge matching on
   both source and target — a self-loop — is one row and is counted once) and reconciled
   against an independently computed `COUNT(*)` under the same predicate in the same
   transaction; a mismatch stops the migration before any destructive plan is prepared. The
   caller-facing cursor walk (`list_edges_after`) may be used for planning reads, but the
   destructive plan is prepared only from the in-transaction enumeration. Offset paging is
   out of contract in any role, in either direction.
2. Enumerates the annotation closure under the same completeness contract: starting from the
   enumerated incident edges, repeatedly collects the `annotates` edges whose TARGET is any
   edge **or note** already in the set, together with the notes those edges anchor, until a
   pass adds nothing — a fixed-point walk with a visited set over the ids of everything the
   set holds, edges and notes alike. Expanding through note targets is not optional: the
   closure contains notes, and ADR-002 rule 1 places no kind restriction on an `annotates`
   target (an entity, a note, or any edge), so a second note may annotate a note already in
   the closure; a walk that expands only through edge ids never finds that annotation, and
   deleting or re-anchoring its subject leaves it dangling. The same rule is why the fixed
   point is required at all: chains of edge- and note-targeting annotations are
   constructible; creation order alone would keep them acyclic, but an endpoint move can
   re-point an edge after creation and nothing in the contract rules a cycle out, so
   termination comes from the visited set, never from assumed acyclicity. One level is not
   enough: a note annotating an edge that itself annotates an incident edge is deleted or
   left dangling by any plan that stops at the first level. The walk carries a work bound,
   `max_closure_rows`, defined as the cardinality of the visited set — every distinct edge id
   and note id the closure holds, counted once each, incident edges included. Its default is
   10,000, a finite value chosen to sit far above any closure a normal correction produces
   and far below a resource hazard; a caller may raise it per call, and no value disables the
   bound. When the visited set would exceed it, the walk stops and the migration is refused
   before any plan is prepared, reporting the bound and the count reached: annotation chains
   are caller-constructible, so an unbounded walk is a resource hazard, and the failure mode
   is a refused migration, never a truncated closure. All queries are
   direct in-transaction reads over the enumerated ids, with no visibility or live-row
   filter. A recreated edge is a new edge id, so an edge-targeting annotation is orphaned
   by recreation even when the record's own annotations were handled correctly. Every edge
   in the closure receives a step-3 disposition; every annotation in the closure receives
   its own recorded re-anchor, delete, or carrier disposition (see the annotation rule
   above).
3. Classifies every enumerated edge — incident edges and every closure member alike — as PRESERVE,
   RECREATE, RE-EXPRESS or REFUSE against the endpoint matrix (a tombstoned edge takes
   RECREATE-TOMBSTONED, NAMED-DELETION or REFUSE instead), records every annotation's re-anchor,
   delete, or carrier disposition, and builds the **closure-substrate map** covering the whole
   set before anything is deleted. The map is keyed by id over BOTH substrates the closure holds,
   because an `annotates` target may be either: every enumerated edge id and every enumerated
   note id appears in it exactly once, and every entry resolves to one of three outcomes —
   an id (the row survives under that id), a planned replacement id (created within the plan),
   or a named deletion.
   - **Edge entries.** A PRESERVEd edge maps to itself. A recreated or re-expressed edge maps to
     its planned replacement. A NAMED-DELETION edge, and an edge whose recorded annotation
     disposition is delete, map to a named deletion.
   - **Note entries.** A note is never recreated: its id is stable across the correction, so a
     retained note — the outcome for re-anchor and for carrier — maps to itself as an identity
     entry. A note deleted under a delete disposition maps to a named deletion. A note retained
     as free-standing under a delete disposition is an identity entry like any other retained
     note. There is no fourth outcome: a note whose disposition leaves it neither retained nor
     deleted is unclassified, which is a REFUSE.

   Every `annotates` edge's re-anchor target is then read off that map, never assumed, whichever
   substrate the target lives in: an annotation of the record itself re-anchors to the new record;
   an annotation of a closure edge re-anchors to that edge's mapped replacement — the edge itself
   when preserved, its recreation when recreated, never the new record; an annotation of a closure
   NOTE re-anchors to that note's map entry, which is the same note id whenever the note is
   retained, so the annotation needs no move; an annotation whose subject maps to a named deletion
   is itself deleted or re-anchored to a named carrier. A note left pointing at a deleted subject
   is not an acceptable outcome. **Any REFUSE stops here.**
4. Prepares deletion and all recreations as ONE atomic plan and commits it, with the plan's
   internal order fixed in five phases. Atomicity gives rollback, not ordering — a plan that
   purged the incident edges before moving them would lose exactly the ids PRESERVE exists to
   keep, inside a transaction that commits clean, and a plan that moved a closure member before
   its mapped target existed would move it onto an id no row carries.
   1. **New record created.**
   2. **Incident PRESERVE moves.** Every PRESERVEd incident edge moves its endpoint fields
      referencing the migrating record onto the new record.
   3. **Deletion.** The purge cascade runs, followed by an explicit `DELETE` for every named
      deletion whose row the cascade does not reach — a closure member is not incident to the
      migrating record, so the purge predicate never matches it and the cascade never touches it;
      only this statement removes it.
   4. **Recreations.** Every RECREATE, RE-EXPRESS and RECREATE-TOMBSTONED replacement is created,
      so that every planned replacement id in the closure-substrate map now names a live row
      (a tombstoned replacement carries its `deleted_at` forward and is a row, not a live edge).
   5. **Post-recreation closure re-anchors.** Every closure member whose subject maps to a
      replacement moves its target onto that replacement. These moves run last because their
      targets do not exist before phase 4. A closure member whose subject maps to itself — a
      PRESERVEd edge, or any retained note — needs no move and none is planned for it.

   Every move statement in phases 2 and 5 carries two postconditions: an `AffectedRowGuard`
   requiring exactly one row changed, and an endpoint read-back asserting the moved field now
   equals the mapped target the plan named for that row. Both apply to closure members, not only
   to incident edges. Between phases 2 and 3 the plan re-runs the purge predicate and requires
   that no PRESERVEd incident edge id still matches it; a match stops the plan (a moved edge still
   referencing the old record is a failed move, not a candidate for deletion). That predicate is
   incident-scoped by construction and says nothing about closure members, so after phase 5 the
   plan verifies the map directly: every entry that resolves to an id or a replacement id names a
   row that exists, every `annotates` edge in the closure targets its subject's mapped outcome,
   and no surviving row references a purged id. A recreated edge is a new edge id, so anything
   that referenced the old id does not follow and must be re-pointed in the same plan.
5. Records the old record's id in the new record's properties, so the discontinuity is traceable.

Relation to ADR-113: ADR-113's `move_edge_endpoint` primitive preserves an edge id when an
endpoint moves between records, and with it every annotation targeting the edge — and its split
recipe composes exactly the shape a kind migration needs: create the new record, then move each
qualifying edge onto it. The PRESERVE disposition above is that composition applied here. Three
conditions gate it per edge, all from ADR-113's own contract: the resulting triple must be legal
under the new kind (the primitive re-validates the endpoint contract), the move must not collide
with an existing natural key (the primitive fails loud rather than dropping the edge), and the
primitive must exist in the running system (ADR-113 is Proposed; a system without the operation
takes the delete-and-recreate path for every edge). An edge failing any of the three falls
through to RECREATE, RE-EXPRESS or REFUSE, and the annotation closure above is what keeps its
annotations from dangling.

If kind changes later become common enough that this procedure is being run routinely, that is the
evidence that would justify the mechanism, and the count of times it has been run is what should
open that decision. This ADR does not pre-commit to it.

## Acceptance criteria

Each decision above is accepted by a test that fails against the pre-image. An implementation of
this ADR is complete when all of the following hold:

**Endpoint pair (Decision 1).**

- `link(source=<service>, relation="introduced_by", target=<document>)` succeeds and the edge
  reads back.
- The neighboring pairs this ADR deliberately did not add are still rejected, each with an error
  naming the valid values: `Service introduced_by Person`, `Service introduced_by Org`,
  `Document introduced_by Service` (the reverse direction), and `Service derived_from Document`.

**Classification tie-break (Decision 2).**

- ADR-001 itself carries the amendment in this PR's diff: its decision tree's step 5 names the
  instance-evidence test and its §"Service/concept tie-break" states the D/T partition. A test
  reading ADR-001 finds the operative text; ADR-167 records the decision and rationale.
- Each arm has a worked fixture stated as exact record fields, with the tree preconditions
  explicit (every fixture is not a person, org, document, or dataset, so steps 1-4 are false
  by construction), and an expected stored outcome decidable from the written rule alone:
  - D without T → `Service`: name `retrieval-gateway-prod`, description `the deployment
    serving vector retrieval; currently down between deployments`, properties
    `{endpoint: "https://retrieval.example/api", operator: "platform"}`, no `entity_type`.
    D holds (endpoint, operator); T does not (no technique designator in any field).
    Classifies `Service`; liveness is not consulted.
  - D absent, codebase identity → `Project` via step 6: name `retrieval-gateway`,
    description `Rust codebase for the retrieval gateway`, properties
    `{repository: "https://example.com/r/gateway", language: "rust"}`. No instance
    identifier, so step 5 does not fire; step 6 matches the repository field. Classifies
    `Project`, never `Service`.
  - D absent, technique identity → `Concept` via step 8: name `HNSW`, description
    `graph-based approximate nearest neighbour search algorithm`, no instance identifier.
    Step 5 does not fire; step 8 classifies `Concept`.
  - D with T → split: name `Vamana index service`, description `deployment of the Vamana
    graph-index algorithm`, properties `{endpoint: "https://ann.example/v1"}`. D holds
    (endpoint); T holds (`algorithm` designator). The split is mandatory: two records
    joined by `Service instance_of Concept`; classifying it as a single record of either
    kind is rejected by the written rule.
  - Step 9 note: name `experimental serving stack`, description `deployed somewhere in the
    lab, details unrecorded`, no properties — deployment vocabulary (`deployed`) appears
    only as bare prose: no endpoint or address, no named host, region, cluster, or
    operator, and no state record in any property field, so D does not hold and step 5
    does not fire; no codebase field (step 6 false), no technique designator (step 8's
    abstract-idea test unresolved), steps 1-8 thereby exhausted without resolving. Lands
    `Concept` per step 9, with the open classification question recorded as a note
    annotating the record — the question-note trigger (deployment vocabulary present, no
    instance identifier) is exactly this case.

**Migration procedure (Decision 3).**

- Enumeration, exactness: multi-namespace listing exactness (every row exactly once across
  pages, stable page order) is owned by the tests of the change that closed #2088 and is not
  re-owned here; this ADR's enumeration criteria assume it and test what the listing surface
  cannot answer.
- Enumeration, destructive-scope completeness: the fixture is this exact matrix of six
  incident edges on the migrating record R. The caller's visible namespaces are `ns_a` and
  `ns_b`; `ns_hidden` is not visible to it. The invisible row (e5) and the tombstoned row
  (e6) are distinct edges, both directions appear in both the included and excluded sets,
  and e3 is a self-loop matching the purge predicate on both source and target:

  | id | source → target   | namespace   | deleted_at | in visible/live walk | in purge set |
  | -- | ----------------- | ----------- | ---------- | -------------------- | ------------ |
  | e1 | R → X1            | `ns_a`      | live       | yes                  | yes          |
  | e2 | X2 → R            | `ns_a`      | live       | yes                  | yes          |
  | e3 | R → R (self-loop) | `ns_a`      | live       | yes                  | yes, once    |
  | e4 | X3 → R            | `ns_b`      | live       | yes                  | yes          |
  | e5 | R → X4            | `ns_hidden` | live       | no (namespace)       | yes          |
  | e6 | X5 → R            | `ns_a`      | tombstoned | no (live filter)     | yes          |

  The visible/live enumeration (cursor walk or listing surface) returns exactly
  {e1, e2, e3, e4}. The in-transaction purge-predicate enumeration returns exactly
  {e1, e2, e3, e4, e5, e6}, each id once — e3 appears once despite matching both the
  source and target arms of the predicate (the predicate is evaluated per row, so the one
  self-loop row matches once) — and reconciles with `COUNT(*)` = 6 under the same predicate
  in the same transaction, the same count Decision 3 step 1 names. All six rows receive dispositions
  before any plan is prepared, because the purge would delete all six. A migration
  prepared from the four-row visible/live enumeration must be refused, and the refusal is
  the asserted outcome, not a warning. When e3's disposition is PRESERVE, the asserted
  outcome is that both of its endpoint fields reference the new record and its edge id is
  unchanged — a self-loop preserved by updating one field is a defect the fixture must catch.
  For e6, the tombstone rule is the asserted outcome, and both of its outcomes are exercised:
  e6 is never PRESERVEd; under RECREATE-TOMBSTONED its replacement row carries the same
  `deleted_at` it had before, and under NAMED-DELETION the map records its named deletion with
  the tombstone noted and the plan carries the explicit `DELETE` for it. A live row where e6's
  fact used to be is a resurrection and a defect the fixture must catch, whichever outcome ran.
  The NAMED-DELETION arm also asserts what produced the map entry: e6 carries no annotation, so
  a procedure whose only named-deletion source is an annotation disposition cannot classify it,
  and a fixture that reaches a map without an entry for e6 has found that defect.
- Edge-as-endpoint coverage: the fixture includes an `annotates` edge whose TARGET is itself an
  edge incident to the migrating record, a second-level chain — a note whose `annotates`
  edge targets that first `annotates` edge — and a note-target chain: a further note whose
  `annotates` edge targets a NOTE already in the closure (the second-level note), which an
  edge-id-only walk never finds. The closure enumeration finds all three levels, and
  after migration every annotation's target is asserted by the closure-substrate map's outcome
  for its subject: a PRESERVEd subject keeps its original id and the annotation still points at
  it; a recreated or re-expressed subject's annotation points at the mapped replacement id;
  a deleted subject's annotation follows its recorded deletion-or-carrier disposition. No
  annotation points at a purged edge id under any disposition mix. A migration
  prepared from a one-level enumeration must be refused, for the same reason the four-row
  visible/live enumeration above is refused.
- Note subjects in the map: the note-target chain above is run three times, once per note
  outcome, and each asserts the map entry as well as the resulting target. Retained under
  re-anchor: the closure note carries an identity entry and the annotation targeting it is
  unchanged, with no move planned for it. Deleted: the note carries a named deletion, and the
  annotation targeting it is itself deleted or re-anchored to its named carrier, with the
  explicit `DELETE` present in the plan because no cascade reaches a non-incident row. Retained
  as free-standing under a delete disposition: the `annotates` edge below it is deleted while
  the note carries an identity entry and reads back intact. A map missing an entry for any
  enumerated note id is the defect this arm exists to catch — a per-edge map passes every other
  assertion here.
- Plan ordering across substrates: the fixture places a closure member whose subject is a
  RECREATEd incident edge, so its mapped target does not exist until the recreation phase. The
  asserted outcome is that its move executes after that recreation and its endpoint read-back
  names the replacement id; a plan that moves it beside the incident PRESERVE moves fails,
  because the target id names no row at that point. Every move in the plan, incident or closure,
  asserts one affected row and its endpoint read-back.
- Closure work bound: a fixture whose annotation chain exceeds `max_closure_rows` refuses the
  migration before any plan is prepared, reporting the bound and the count reached; no row in
  the store changes. The paired arm raises the bound for the same fixture and completes, so the
  refusal is attributable to the bound and not to the chain's shape.
- Disposition gate: a fixture edge with no legal expression under the new kind (for example
  `Org contains <record>` migrating to `concept`) causes the migration to stop with a REFUSE
  before any plan is prepared; no row in the store changes.
- Atomicity and rollback: with the full plan prepared, a forced failure on a late statement (a
  violated `AffectedRowGuard` on one of the recreations) rolls the whole plan back — the old
  record row, every incident edge, and every annotating note read back unchanged afterward.
- Traceability: after a successful migration, the new record's properties carry the old record's
  id.

## Consequences

**Positive.** Service origins become traversable lineage rather than prose in a note. The
classification decision becomes checkable against a written rule, so a disagreement about a
record's kind can be resolved rather than argued. The migration cost is written down with its
number, so the next person deciding whether to build a mechanism has the evidence.

**Negative, stated plainly.** One more endpoint pair is one more row that has to stay consistent
with the validation code, and the endpoint contract is already the part of ADR-002 that has been
amended most often. The tie-break procedure resolves the disagreement case by mandating a split
into two records, which costs a second record and an `instance_of` edge in cases where a single
looser record would have been tolerable; the procedure prefers that cost to unreproducible
classification.

**Not addressed.** This ADR does not make services reachable as targets of provenance edges in
general. An earlier framing of this proposal asked for that, and it was withdrawn: the measured
refusals concern a service recording its own origin, and the evidence licenses one pair, not a
class. If a case for the general form appears, it is a separate amendment with its own evidence.

## Rationale

### Why one pair rather than a symmetric provenance class?

Because the refusals that motivated this are asymmetric. What was blocked in practice was a
service pointing at the document that introduced it. Nothing measured asked for a document to
point at a service. Adding the reverse direction on the grounds that it looks symmetric would put
a pair in the contract that no observed use needs, and ADR-002's closed-set rationale is
explicitly about not doing that.

### Why not fix this by classifying the affected records as concepts instead?

That is the alternative worth taking seriously, since a `Concept introduced_by Document` pair
already exists. It was rejected because it inverts the problem: it asks records to be given the
kind whose endpoint contract is convenient rather than the kind that describes them, which is the
same failure the tie-break procedure in this ADR exists to prevent. It would also cost the 62
edges, since kind is immutable.

### Why is the classification gap worth an ADR at all?

Because the endpoint contract makes kind consequential and irreversible in the same stroke. A
record's kind decides what edges it may ever carry, and it cannot be changed without hard-deleting
the record and cascading its edges. A decision with that shape should be made against a written rule.
