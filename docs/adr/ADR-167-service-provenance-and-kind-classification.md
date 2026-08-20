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
specification, ADR, or paper that introduced it. Every sibling kind can: `Concept introduced_by
Document`, `Artifact introduced_by Document`, `Document introduced_by Person`. `Service` is the
gap in that row, and the gap is not deliberate — the 2026-07-08 amendment that added three
`introduced_by` pairs was reasoning about documents and orgs and did not consider services.

The practical consequence is that a service's origin gets recorded as an annotation on a note
instead of as an edge, where it is invisible to lineage traversal.

**2. The service-versus-concept split is undocumented, and as a result it is not reproducible.**
ADR-002 fixes the relation set and the endpoint contract, but no ADR states what makes something
a `service` rather than a `concept`. The two kinds have materially different endpoint contracts,
so the choice at creation time silently decides what edges the record will ever be allowed to
carry. Made without written criteria, that choice is made on the name.

**3. Kind is immutable, so a misclassification is only correctable by delete-and-recreate, and
that path destroys edges.** Measured against the 19-service population: 16 carry at least one
edge and 3 carry none. Delete-and-recreate across the population would destroy **62 edges**, of
which **42 are `annotates`**. Because `annotates` runs note → entity, each destroyed annotates
edge leaves a note whose subject no longer exists — the note survives, saying something about
nothing, which is worse than either deleting it or keeping it attached.

There is a further complication in enumerating that blast radius, filed separately as issue
#2085: `list(kind="edge", target_id=…)` cannot match an edge whose target is itself an edge, and
returns an empty result rather than an error. So the affected-edge count for a migration cannot
be obtained from that filter alone, and an empty result there must not be read as "nothing points
at this."

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
migration, 42 of them annotations. For a single-record correction the cost is that record's own
edge count, which is typically small.

So the proposal is a **procedure**, not a mechanism. Any kind correction:

1. Enumerates the record's edges in **both directions** before deleting anything, and does not
   rely on `list(kind="edge", target_id=…)` alone for the inbound side (issue #2085).
2. Names, for each `annotates` edge, whether the annotating note is re-anchored to the new record
   or deleted with it. A note left pointing at a deleted subject is not an acceptable outcome.
3. Enumerates the notes annotating the record's **edges**, not only those annotating the record
   itself, and gives each the same re-anchor-or-delete disposition. This population is easy to
   miss twice over: a re-created edge is a new edge id, so an edge-targeting annotation is
   orphaned by the recreation in step 4 even though the record itself was handled correctly; and
   it is exactly the population issue #2085 makes invisible, since the target-side filter returns
   an empty result rather than an error. Enumerate it from the source side.
4. Re-creates the edges against the new record and reads them back, since a re-created edge is a
   new edge id and anything that referenced the old id does not follow.
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
record's kind decides what edges it may ever carry, and it cannot be changed without destroying
edges. A decision with that shape should be made against a written rule.
