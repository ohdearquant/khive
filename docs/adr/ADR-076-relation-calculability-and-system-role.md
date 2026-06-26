# ADR-076: Relation-Set Calculability — System Role and the Non-Redundancy Certificate

**Status**: proposed\
**Date**: 2026-06-26\
**Authors**: Ocean, lambda:khive\
**Amends**: [ADR-002](ADR-002-edge-ontology.md) — corrects the `contains`/`part_of` "inverse"
phrasing (the relations are distinct, not converses).\
**Relates to**: [ADR-055](ADR-055-epistemic-edge-relations.md) (the `supports`/`refutes` case
study), [ADR-001](ADR-001-entity-kind-taxonomy.md), [ADR-013](ADR-013-note-kind-taxonomy.md),
[ADR-017](ADR-017-pack-standard.md).

## Context

khive ships a closed set of 17 edge relations (ADR-002 base 15, ADR-055 +2 epistemic), 9 entity
kinds, and 5 note kinds. A recurring and fair challenge to any closed taxonomy is: the set looks
hand-picked. Why exactly these relations and not others? If the answer is "they felt right," the
taxonomy is ad-hoc, and ad-hoc closed sets accrete cruft and resist principled extension. The
demand is for the set to be **calculable** rather than eyeballed: a mechanical account of why
each relation belongs, and a mechanical gate that any proposed new relation must pass.

This ADR answers that demand. It does so honestly, after validating every claim against a copy
of the production graph rather than against theory. The honest answer has three parts, and the
first thing to state is what calculability **is not**.

**Calculability is not derivation.** There is no closed-form argument that yields "these 17
relations are necessary" from first principles. Any such derivation would smuggle in the very
modeling choices it claims to derive. A relation set is a design artifact answering a bounded
list of query classes that the system's users actually run. The query classes are chosen, not
deduced.

**What is mechanical** is the justification of each relation and the falsification of redundancy:

1. **System role** — each relation must name a concrete query, view, or policy that it uniquely
   serves, witnessed by usage. This is the justifier.
2. **A non-redundancy certificate** — a fixed family of redundancy hypotheses ("eliminators"),
   each of which proposes encoding a relation as something cheaper. A relation is non-redundant
   only if, for every eliminator, a concrete fixture exists where the cheaper encoding returns a
   wrong answer that the standalone relation gets right. This is a falsification harness, not a
   necessity proof.

The two compose: the certificate is a tripwire that forces a redundancy question to be answered
explicitly; the system-role declaration is the arbiter that decides the relation's fate. The
certificate is **necessary but not sufficient**. A relation can fail the certificate (be formally
redundant) and still be kept, when a declared system role demands it. The `supports`/`refutes`
pair is exactly that case, and it is the proof that the arbiter, not the algebra, has the final
word.

### What is actually in the system today

This ADR is grounded in the live graph (8381 edges, 2763 entities), not in the design loop's
prior assumptions. The relevant facts:

- 16 of 17 relations carry live edges; only `refutes` has none. The taxonomy is exercised, not
  speculative. Each heavily-used relation answers a query class agents run.
- khive performs pure breadth-first traversal with relation filters. There is **no**
  composition engine, transitive-closure machinery, derived-edge materialization, or rollup
  inference anywhere in the codebase.
- Edge metadata is populated on 3.3% of edges, and the only keys ever used are `context`,
  `dependency_kind`, `note`, `reason`. There is no `role` or `structural_role` field in use.
- The `entity_type` subtype column (ADR-069) is null on every entity. Typed subtyping is
  unexercised.
- Live data violates the declared endpoint contracts in several places (for example
  `contains` project→concept, `part_of` person→org). The contracts are partly aspirational.

These facts decide the scope. The normative content of this ADR is the calculability account,
the `supports`/`refutes` finding, and the `contains`/`part_of` correction. Everything that would
require building new machinery (composition tables, rollup guards, materialized views, typed
sub-relations, RDF/OWL export) is explicitly **out of normative scope** and recorded as future
work with the empirical reason it is not built today. Building any of it now would be theory
dictating architecture against the grain of how the system is actually used.

## Decision

### D1 — System role is the admission and retention standard

A relation belongs in the closed set if and only if it has a declared **system role**: a
specific query, view, or policy that the relation uniquely serves and that some other relation
or attribute encoding would answer incorrectly. The role is recorded in prose in the governing
ADR (ADR-002 and its amendments) and is witnessed by usage in the live graph.

System role is the justifier of record. Usage corroborates it but does not replace it: a
relation may be provisioned ahead of the workload it serves (see D3). The role declaration is
what a future maintainer reads to understand why the relation exists.

### D2 — The non-redundancy certificate is the regression gate on new relations

Any proposal to add a relation to the closed set must pass a non-redundancy certificate. The
certificate runs a fixed family of **eliminators**, each a hypothesis that the proposed relation
R is redundant because it can be encoded more cheaply. R earns admission only if every eliminator
is **defeated** by a fixture: a small concrete graph plus a query where the cheaper encoding
returns a wrong answer and R-as-its-own-relation returns the right one.

The eliminator families:

| Family | Hypothesis: R is redundant because it is …                                    | Defeated by a fixture where …                                             |
| ------ | ----------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| Cv     | the converse of an existing relation (use `direction=in`)                     | R and the converse disagree on some pair                                  |
| Er     | an existing relation restricted to specific endpoint kinds                    | R holds for a pair the endpoint restriction would exclude, or vice versa  |
| At     | an existing relation plus a metadata attribute value                          | the attribute encoding loses a query the standalone relation answers      |
| Po     | an existing relation plus a polarity/sign attribute                           | a polarity-blind reader returns a wrong answer the signed relation avoids |
| Ch     | a fixed composition (property chain) of existing relations, `R = S ∘ T`       | the chain admits a pair R rejects, or rejects a pair R admits             |
| Mv     | a query-free materialized view (reachability) over existing relations         | the view and the asserted relation diverge on some pair                   |
| Sr     | a sub-relation of a broader parent (model it as a typed sub-relation instead) | the parent cannot carry a distinction R needs at top level                |

A relation that survives every eliminator is non-redundant and admissible. A relation for which
some eliminator has **no** defeating fixture is formally redundant under that eliminator, and the
default disposition is to encode it the cheaper way (demote to an attribute, a sub-relation, a
direction-aware query, or a view) rather than mint a top-level relation.

This is the operational meaning of "calculable": the gate is a mechanical, repeatable check that
a build can run, not a matter of taste applied case by case. It is a **falsification harness**.
It cannot prove a relation is necessary. It can only fail to refute its non-redundancy, which is
the strongest mechanical statement available and the right discipline for a closed set.

### D3 — `supports`/`refutes`: redundant under the certificate, kept by system role

The `supports`/`refutes` pair (ADR-055) fails the Po eliminator. A single relation `assesses`
carrying a polarity attribute (`polarity ∈ {supports, refutes}`) answers every query the pair
answers, with provably identical results: every "what supports X" query becomes "what assesses X
with polarity=supports." No fixture defeats the Po eliminator for this pair. By the D2 default,
the pair would be demoted to one relation plus an attribute.

The pair is kept as two top-level relations anyway. The justification is the declared system role
in ADR-055: the epistemic layer khive is building toward requires polarity to be a first-class,
relation-level distinction that planners, indexes, federation, and the public API can branch on
directly, not a value buried in an open metadata blob that 3.3% of edges populate. That is a
design decision about where the distinction must live, and it is the designer's to make.

This is the load-bearing case for the whole ADR. The certificate did its job: it flagged the one
pair in the set that is formally collapsible, and it forced the keep-or-demote question to be
answered explicitly. The answer came from the system-role declaration, not from the algebra. The
certificate is necessary and not sufficient; the declaration is the arbiter. The live data
sharpens the point: `supports`=2 edges, `refutes`=0. The pair is both the algebra-flagged pair
and the near-unused pair, kept purely on forward-looking design grounds. A taxonomy that admitted
relations only by algebra would have dropped it; a taxonomy that admitted them only by usage
would also have dropped it. It is kept because the design says the epistemic layer matters.

### D4 — `contains` and `part_of` are distinct relations, not converses

ADR-002 describes `part_of` as the "inverse of `contains`" in three places. That phrasing is
incorrect and is struck by this ADR. `contains` denotes housing or scope; `part_of` denotes
constitution or membership. The two **coincide** in some domains (an organization contains a
subsidiary and the subsidiary is part of the parent) and **diverge** in others (an ocean
contains fish, but a fish is not part of the ocean). Because they diverge, neither may be derived
from the other.

The live graph confirms this is the correct model. 70 pairs carry both `contains(a,b)` and
`part_of(b,a)`. That co-occurrence is the fingerprint of a system that does **not** auto-infer
the converse: agents assert both directions by hand exactly when both housing and constitution
hold. If khive derived `part_of` from `contains`, those 70 doubles would not exist as asserted
edges. The mechanical "no auto-inverse" rule (ADR-002 §"Why no auto-inverse?") is correct and is
confirmed by code: no code couples the two relations. Only the **semantic** phrasing — calling
one the inverse of the other — is wrong, and only that phrasing is corrected here.

## Empirical grounding

Validated on a disposable copy of the production graph. Full detail in the design workspace; the
load-bearing numbers:

- **Relation distribution** (live edges): introduced_by 1435, enables 1380, annotates 1269,
  implements 896, composed_with 659, part_of 539, instance_of 519, extends 452, competes_with
  446, depends_on 294, contains 285, precedes 120, variant_of 50, supersedes 25, derived_from 10,
  supports 2, refutes 0. Evidence for D1 (system role witnessed by usage) and D3 (the flagged
  pair is also the unused pair).
- **`contains(a,b) ∧ part_of(b,a)`**: 70 pairs. Evidence for D4 (no auto-inverse, distinct
  relations co-asserted where they coincide).
- **Metadata population**: 3.3% of edges; keys `context, dependency_kind, note, reason` only.
  Evidence that an attribute-encoded distinction (the At/Po default of D2) is cheap in schema but
  expensive in practice, because agents rarely populate metadata. This is why D3 keeps polarity
  at relation level.
- **`entity_type` null on all entities**: typed subtyping is unexercised. Evidence that the Sr
  eliminator's "model it as a typed sub-relation" alternative is forward-looking, not a reuse.
- **Endpoint-contract violations**: the declared contracts do not fully match live data. The
  certificate operates over the declared contracts, so its soundness is bounded by contract
  fidelity. A reconciliation audit is recorded as future work.

## Out of normative scope (future work)

Each item below was considered and is **not** adopted. The empirical reason is recorded so the
deferral is a decision, not an omission.

- **Composition / rollup tables and a `structural_role` edge field.** A guard that decides when
  `part_of` reachability rolls a functional fact up a hierarchy would need agents to mark each
  edge's role. Metadata is populated on 3.3% of edges and no `role` field is in use, so such a
  guard would be unreliable in exactly the cases it must cover. No rollup is computed today
  (pure BFS), so there is nothing to guard. Revisit only if a workload begins computing closures
  over `part_of` and the role signal is reliably populated.
- **Materialized derived-edge views and pin-on-annotation.** No materialization machinery exists;
  traversal is on-demand. Building a view catalog, derivation provenance, and invalidation before
  any measured need is premature optimization. Revisit behind a measured reuse-and-pain threshold.
- **Typed sub-relations (`proves ⊑ supports`) for opaque-label resolution (#293).** The subtype
  column is unused, so this would be the first subtyping in the system, not a reuse. It is a
  coherent forward direction for resolving overloaded relations (for example `extends` as both
  subtype and "builds on"), to be specified when a pack needs it, under closed-world,
  validator-enforced entailment.
- **RDF / OWL / SHACL export of the relation set.** External-interop concern, tracked separately.
  The relevant invariant for that work is that core relations must carry no `rdfs:domain` /
  `rdfs:range` that would let a reasoner infer entity kinds from edges; this ADR's endpoint
  contracts are validation rules, not logical axioms.
- **Endpoint-contract reconciliation.** Declared contracts diverge from observed endpoint pairs.
  A reconciliation audit (compare declared vs observed, then either tighten enforcement or widen
  the contract per relation) is concrete future work and a precondition for the certificate's
  soundness over real data.

## Consequences

**Changes now (documentation only).**

- ADR-002's "inverse of `contains`" phrasing is struck in favor of the distinct-relations model
  (D4). No code or schema changes; the no-auto-inverse behavior is already correct.
- The non-redundancy certificate (D2) is the documented gate any future relation proposal must
  pass. New-relation ADRs must include the eliminator fixtures or an explicit system-role
  override for any undefeated eliminator, on the `supports`/`refutes` pattern.

**Does not change.**

- No relation is added or removed. The set stays at 17.
- No schema change, no new edge field, no new machinery. khive continues to do pure BFS.
- The closed enum stays closed and compile-time. The certificate governs proposals; it does not
  open the set.

**Trade-off accepted.** The certificate is a falsification harness, not a necessity proof. It
cannot certify that the 17 relations are the uniquely correct set, only that each survives the
registered eliminators or is kept by an explicit, recorded system-role override. This is the
honest ceiling for a designed closed set, and it is stronger than the prior state, where the set
was justified only by prose intuition.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md) — entity kind taxonomy
- [ADR-002](ADR-002-edge-ontology.md) — edge ontology (amended here)
- [ADR-013](ADR-013-note-kind-taxonomy.md) — note kind taxonomy
- [ADR-017](ADR-017-pack-standard.md) — pack standard, pack-extensible endpoints
- [ADR-055](ADR-055-epistemic-edge-relations.md) — `supports`/`refutes` epistemic relations
- [ADR-069](ADR-069-subject-model.md) — subject model, typed subtypes
