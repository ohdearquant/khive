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
destroys none of them and instead strands all 62 on a deleted endpoint (Decision 3). Because `annotates` runs note → entity, each destroyed annotates
edge leaves a note whose subject no longer exists — the note survives, saying something about
nothing, which is worse than either deleting it or keeping it attached.

Enumerating that blast radius has a complication of its own, because an edge may itself be an
endpoint. `endpoint_exists_clause` in `crates/khive-db/src/stores/graph.rs` admits an undeleted
`graph_edges` row as a valid endpoint alongside entities, notes and events, so an `annotates` edge
may point AT another edge, and those rows are part of any migration's affected set. The inbound
filter `list(kind="edge", target_id=…)` does reach them: the earlier report that it returns empty
(issue #2085) has been retracted, and an integration test covering the edge-as-target case matches.

What remains open is exhaustive ENUMERATION, which is a different question from whether the filter
matches. Issue #2088 records that the multi-namespace visibility path fetches each namespace's rows
ordered by `created_at`, re-sorts the union by UUID, and then slices `[offset, offset+limit)`: the
window floats as the prefix grows, so successive pages both duplicate and skip rows, and a paged
walk terminates having seen a fraction of the population while reporting nothing wrong. That defect
is not confined to inbound reads: `source_id` and `target_id` are filter predicates on the same
listing path (`build_edge_filter_sql` in `crates/khive-db/src/stores/graph.rs`), so a source-filtered
walk pages through the same floating window. No direction of offset-paged enumeration is safe while
the defect stands, and a procedure that can lose edges must not rest its completeness on offset
paging in either direction. The runtime already carries the sound primitive: `list_edges_after`
(`crates/khive-runtime/src/operations.rs`) walks the durable insertion-sequence ledger with an
explicit cursor, merges every visible namespace at one sequence boundary, and fails loudly on a
hard-deleted or out-of-scope cursor instead of hiding an incomplete traversal. The enumeration
contract in Decision 3 is therefore stated against that cursor walk (or a transaction-scoped direct
query executed inside the migration's own transaction), never against offset paging.

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
not introduce a parallel set of criteria; it amends step 5 of ADR-001's decision tree so that the
one under-determined boundary resolves the same way for every writer. The amendment replaces
step 5's condition with an ordered sub-procedure:

> **5. Evaluate, in order, stopping at the first rule whose condition holds:**
>
> 5a. It is meaningful to say the thing is currently up or down (it has, or when deployed would
> have, an endpoint, health state, deployment surface, and an operator) → `Service`. Current
> downtime does not disqualify: a deployable system between deployments is still a `Service`.
>
> 5b. The name refers to an idea, technique, pattern, or named result that would still exist if
> every deployment of it were removed, AND no record-specific deployment is being named →
> `Concept` (continue to step 8's signals to confirm).
>
> 5c. Both 5a and 5b hold — the name is being used for a technique AND for a deployment of it —
> → **the split is mandatory**: create two records, a `Concept` naming the technique and a
> `Service` naming the deployment, joined by `Service instance_of Concept`. Classifying the
> single record either way is out of contract; the disagreement is the evidence that there are
> two things.
>
> 5d. Neither holds cleanly and the writer cannot state which — record it as `Concept` per
> ADR-001 step 9's uncertainty default, and record the open question as a note annotating the
> record, so the classification is revisitable instead of silently settled.

The rules are ordered, each condition is decidable from the record being written (no "usually",
no evidence threshold left to taste), and the disagreement case has exactly one outcome. The
earlier draft of this section stated the liveness and independence tests without an order or a
mandatory outcome for the disagreement case; two writers could reach different kinds for a
currently-down deployable system, which is the drift this decision exists to stop.

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
counterpart. The statement order inside the atomic plan is: the record's own row delete first (under
an exactly-one-row guard), lineage-warning statements second, and the incident-edge purge LAST
(`crates/khive-runtime/src/operations.rs`, `crates/khive-runtime/src/atomic_prepare.rs` — both build
this order). Nothing in a migration plan may therefore rely on the old record row still existing when
the purge runs; it is already gone. The order is invisible to callers only because the whole plan
commits in a single synchronous pass — which is one more reason the atomicity requirement below is
load-bearing rather than advisory.

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
therefore be classified against ADR-002's matrix BEFORE anything is deleted, into exactly three
dispositions, each of which must be written down per edge:

- **RECREATE** — the triple is legal under the new kind. Recreate and read back.
- **RE-EXPRESS** — the triple is illegal but the fact survives under a different relation or a
  different endpoint. Name the replacement triple and why it carries the same claim.
- **REFUSE** — the fact has no legal expression under the new kind. This does not mean drop the edge;
  it means **the migration does not proceed** on that record until someone decides, on the record,
  either to accept the loss with a named carrier for the fact or to leave the kind as it is.

The default is REFUSE. A procedure whose failure mode is a silently dropped edge is the thing this
ADR exists to prevent, so an unclassifiable edge stops the migration rather than being absorbed by it.

With those settled, a kind correction:

1. Enumerates the record's edges in **both directions and across every namespace visible to the
   migrating caller** before deleting anything, using the insertion-sequence cursor walk
   (`list_edges_after`) or a direct query inside the migration's own transaction — never offset
   paging, in either direction (see the enumeration note in Context). The walk runs to cursor
   exhaustion, and the collected edge IDs are reconciled against an independently computed count
   of the record's incident edges; a mismatch stops the migration before any destructive plan is
   prepared.
2. Enumerates the notes annotating the record's **edges**, not only those annotating the record itself.
   A recreated edge is a new edge id, so an edge-targeting annotation is orphaned by recreation even
   when the record's own annotations were handled correctly, and a hard delete's cascade removes those
   rows outright. Each gets the same re-anchor-or-delete disposition as step 3.
3. Classifies every enumerated edge as RECREATE, RE-EXPRESS or REFUSE against the endpoint matrix, and
   names for each `annotates` edge whether the annotating note is re-anchored to the new record or
   deleted with it. A note left pointing at a deleted subject is not an acceptable outcome. **Any
   REFUSE stops here.**
4. Prepares deletion and all recreations as ONE atomic plan, commits it, and reads back every recreated
   edge. A recreated edge is a new edge id, so anything that referenced the old id does not follow and
   must be re-pointed in the same plan.
5. Records the old record's id in the new record's properties, so the discontinuity is traceable.

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

- The ADR-001 amendment text contains the ordered 5a–5d sub-procedure, and each branch has a
  worked fixture: a currently-down deployable system classifies `Service` (5a); a technique with
  no named deployment classifies `Concept` (5b); a name used for both yields two records joined
  by `Service instance_of Concept`, and classifying it as a single record of either kind is
  rejected by the written rule (5c); an undecidable record lands `Concept` with an annotating
  question note (5d).

**Migration procedure (Decision 3).**

- Enumeration: against a fixture population large enough to exercise the paging defect in issue
  #2088 (rows spread across at least two namespaces, with interleaved creation timestamps and
  UUID order that disagrees with `created_at` order), the cursor walk returns every incident edge
  in both directions exactly once, and its result reconciles against the independent count. An
  offset-paged walk over the same fixture demonstrably misses or duplicates rows — that fixture
  is what makes this criterion a real discriminator rather than a restatement.
- Edge-as-endpoint coverage: the fixture includes an `annotates` edge whose TARGET is itself an
  edge incident to the migrating record; the enumeration finds it, and after migration the
  annotation is re-anchored to the recreated edge's new id (or deleted with a recorded
  disposition) — never left pointing at a purged edge id.
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
