# ADR-167: Service Provenance, Service/Concept Classification, and Kind Migration

**Status**: proposed\
**Date**: 2026-08-20\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-002](ADR-002-edge-ontology.md) — the closed 17-relation set and the base endpoint contract
- [ADR-055](ADR-055-epistemic-edge-relations.md) — same-substrate restriction on `supports` / `refutes`

**Amends**:

- [ADR-002](ADR-002-edge-ontology.md) — add one derivation pair, `Service introduced_by Document`,
  and record written classification criteria for `service` versus `concept`

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

**2. The service-versus-concept split is undocumented, and as a result it is not reproducible.**
ADR-002 fixes the relation set and the endpoint contract, but no ADR states what makes something
a `service` rather than a `concept`. The two kinds have materially different endpoint contracts,
so the choice at creation time silently decides what edges the record will ever be allowed to
carry. Made without written criteria, that choice is made on the name.

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
may point AT another edge, and those rows are part of any migration's affected set. Whether the
inbound filter `list(kind="edge", target_id=…)` reaches them is disputed and under verification at
issue #2085 — one measurement says it returns empty, a later integration test says it matches. This
ADR takes no position on that, and deliberately does not depend on it: a procedure that can lose
edges must not have its completeness rest on a filter whose behaviour is in question, so the
enumeration below is specified from the source side, which is unaffected either way.

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

### 2. Write down the classification criteria

Add to ADR-002 a subsection stating the test, so that the decision is made against a written rule
rather than against a name:

> A record is a `service` when it names **a running or deployable system that has an operator, a
> deployment surface, and a lifecycle independent of any document describing it** — something
> that can be up or down. A record is a `concept` when it names **an idea, technique, pattern, or
> named result** whose existence does not depend on anything running.
>
> Two tests that discriminate where the name does not. **The liveness test**: ask whether it is
> meaningful to say the thing is currently down. If yes, `service`. **The independence test**:
> ask whether the thing would still exist if every deployment of it were removed. If yes,
> `concept`.
>
> Where both tests point the same way, that is the kind. Where they disagree, there are usually
> two records and not one — a concept naming the technique and a service naming the deployment —
> and they are joined by `Service instance_of Concept`, which the contract already admits.

The criteria are stated as tests rather than as a category list because the failure mode observed
is a borderline record being classified on its name, and a list of examples does not help a
borderline record.

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
counterpart, and it runs BEFORE the record's own row is removed.

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

1. Enumerates the record's edges in **both directions** before deleting anything, from the source side,
   and does not rest completeness on a single inbound filter (see the enumeration note in Context).
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

## Consequences

**Positive.** Service origins become traversable lineage rather than prose in a note. The
classification decision becomes checkable against a written rule, so a disagreement about a
record's kind can be resolved rather than argued. The migration cost is written down with its
number, so the next person deciding whether to build a mechanism has the evidence.

**Negative, stated plainly.** One more endpoint pair is one more row that has to stay consistent
with the validation code, and the endpoint contract is already the part of ADR-002 that has been
amended most often. The classification criteria are a test and not a decision procedure: two
readers can still disagree on a genuinely borderline record, and the ADR's answer in that case is
that there are probably two records.

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
same failure the classification criteria in this ADR exist to prevent. It would also cost the 62
edges, since kind is immutable.

### Why is the classification gap worth an ADR at all?

Because the endpoint contract makes kind consequential and irreversible in the same stroke. A
record's kind decides what edges it may ever carry, and it cannot be changed without hard-deleting
the record and cascading its edges. A decision with that shape should be made against a written rule.
