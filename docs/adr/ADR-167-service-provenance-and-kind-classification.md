# ADR-167: Service Provenance and Service/Concept Classification

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
`annotates`**. The figure is stated for `DeleteMode::Hard` specifically, because soft deletion
destroys none of them and instead strands all 62 on a deleted endpoint. Because each of those 42
runs note → entity in the measured population, each destroyed `annotates` edge leaves a note whose
subject no longer exists — the note survives, saying something about nothing, which is worse than
either deleting it or keeping it attached.

Enumerating that blast radius has a complication of its own, because an edge may itself be an
endpoint. `endpoint_exists_clause` in `crates/khive-db/src/stores/graph.rs` admits an undeleted
`graph_edges` row as a valid endpoint alongside entities, notes and events, so an `annotates` edge
may point AT another edge, and those rows are part of any migration's affected set. The inbound
filter `list(kind="edge", target_id=…)` does reach them: the earlier report that it returns empty
(issue #2085) has been retracted, and an integration test covering the edge-as-target case matches.

What remains open is exhaustive ENUMERATION, which is a different question from whether the filter
matches, and it is why this ADR does not decide the migration question. The listing surface answers
a narrower question than the purge asks. `list_edges_after`
(`crates/khive-runtime/src/operations.rs`) walks the durable insertion-sequence ledger correctly,
but it is visibility-scoped to the caller's namespaces and, like every listing path, filters to
live rows (`deleted_at IS NULL`). The purge is
`DELETE FROM graph_edges WHERE source_id = ?1 OR target_id = ?1` — no namespace predicate, no
tombstone predicate. An incident edge in a namespace the migrating caller cannot see, or one
already soft-deleted, passes a visible/live enumeration and its matching count while the purge
deletes it anyway. A completeness check scoped narrower than the destructive statement it guards
is not a completeness check.

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
not introduce a parallel set of criteria; it amends ADR-001 in place, and the operative text is
ADR-001's §"Service/concept tie-break", which ADR-001 already carries. The
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

### 3. Kind migration: deferred to a successor ADR

**This ADR specifies no kind-migration mechanism and proposes none.** The measured consequences in
Context item 3 stand as the record of why the question is open. The closure procedure that would
answer it is deliberately absent rather than pending.

The reason is a shape argument, not a scheduling one. A correct closure rule for a
delete-and-recreate has to be a total function over the product of five independent dimensions: the
substrate of each incident record (entity, note, event, or edge), the relation kind, the endpoint
role the migrating record occupies in that relation, the liveness of the incident row (live or
already tombstoned), and the namespace it lives in relative to the migrating caller's visibility.
Context item 3 above demonstrates four of those five changing the correct answer — an edge that is
itself an endpoint, an `annotates` edge whose destruction orphans a note, an already-tombstoned
incident row, and a row outside the caller's visible namespaces — and the purge's own predicate
ignores the last two entirely. A rule stated for the cases that have come up is not a specification
of that function. It is a set of points on it, and the distance between the two is exactly where an
edge gets destroyed without appearing in any enumeration that preceded it.

**A successor ADR specifying that function is REQUIRED before any implementation may depend on kind
migration.** Until such an ADR is accepted, kind remains immutable in practice: a misclassified
record is corrected by delete-and-recreate at the operator's own risk, with the blast radius in
Context item 3 as the standing estimate of what that costs. Nothing accepted here creates or
implies a supported migration path.

## Acceptance criteria

Decisions 1 and 2 are each accepted by a test that fails against the pre-image. Decision 3
specifies no mechanism and therefore carries no criteria. An implementation of this ADR is complete
when all of the following hold:

**Endpoint pair (Decision 1).**

- `link(source=<service>, relation="introduced_by", target=<document>)` succeeds and the edge
  reads back.
- The neighboring pairs this ADR deliberately did not add are still rejected, each with an error
  naming the valid values: `Service introduced_by Person`, `Service introduced_by Org`,
  `Document introduced_by Service` (the reverse direction), and `Service derived_from Document`.

**Classification tie-break (Decision 2).**

- ADR-001 itself carries the amendment: its decision tree's step 5 names the instance-evidence
  test and its §"Service/concept tie-break" states the D/T partition. A test reading ADR-001
  finds the operative text; ADR-167 records the decision and rationale.
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
