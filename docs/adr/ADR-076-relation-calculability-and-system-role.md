# ADR-076: Relation-Set Calculability and System Role

**Status**: Accepted\
**Date**: 2026-06-26\
**Authors**: khive maintainers\
**Amends**: [ADR-002](./ADR-002-edge-ontology.md), correcting the description of
`contains` and `part_of` as inverses\
**Relates to**: [ADR-055](./ADR-055-epistemic-edge-relations.md),
[ADR-001](./ADR-001-entity-kind-taxonomy.md),
[ADR-013](./ADR-013-note-kind-taxonomy.md),
[ADR-017](./ADR-017-pack-standard.md)

---

## Context

khive ships a closed set of 17 edge relations, 9 entity kinds, and 5 note kinds. A closed relation
set needs a repeatable admission test. Without one, additions depend on intuition and overlapping
relations accumulate over time.

The set cannot be derived uniquely from first principles. It is a design artifact chosen to answer
supported query classes. What can be made mechanical is the test for redundancy and the requirement
that every relation declare a system role.

## Decision

### 1. System role is the admission and retention standard

A relation belongs in the closed set only when its governing ADR names a specific query, view, or
policy that requires the relation as a first-class distinction. The declaration must explain why an
existing relation, direction-aware query, attribute, subtype, composition, or materialized view
would answer that use case incorrectly or make a required public distinction unavailable.

Usage data may corroborate a role, but it is neither required nor sufficient. This avoids exposing
repository-specific graph contents and allows a relation to be introduced before a public fixture
or feature begins using it.

### 2. New relations require a non-redundancy certificate

A proposal to add a relation `R` must evaluate every eliminator below. Each eliminator proposes a
less expensive representation. To defeat it, the proposal supplies a small checked-in synthetic
graph and a query for which the replacement gives the wrong result while `R` gives the intended
result.

| Family | Redundancy hypothesis                                    | Required counterexample                                                      |
| ------ | -------------------------------------------------------- | ---------------------------------------------------------------------------- |
| `Cv`   | `R` is the converse of an existing relation              | A pair on which `R` and the converse differ                                  |
| `Er`   | `R` is an existing relation restricted by endpoint kinds | A valid pair for which the restriction and `R` differ                        |
| `At`   | `R` is an existing relation plus a metadata value        | A query whose required result is lost by the attribute encoding              |
| `Po`   | `R` is an existing relation plus a polarity value        | A polarity-sensitive query answered incorrectly without a top-level relation |
| `Ch`   | `R` is a fixed composition of existing relations         | A graph where the property chain admits or rejects the wrong pair            |
| `Mv`   | `R` is a reachability view over existing relations       | A graph where asserted `R` and reachability differ                           |
| `Sr`   | `R` is a typed sub-relation of a broader relation        | A query that requires `R` at the public relation level                       |

If any eliminator has no counterexample, `R` is redundant under that representation. The default is
to use the less expensive representation. An ADR may override that default only with an explicit
system-role justification.

The certificate is a falsification harness, not a proof that the relation is uniquely necessary.
It makes the redundancy review repeatable and reviewable.

### 3. Certificate fixtures are public and deterministic

Certificate fixtures contain only synthetic entities and edges. Each fixture records:

- the relation under review;
- the eliminator family;
- the graph records and endpoint kinds;
- the exact query;
- the intended result and the replacement's incorrect result.

The test runner loads each fixture into an isolated store, executes both query forms, and compares
stable identifier sets. Fixtures must not depend on record names, counts, or metadata from an
operational graph.

### 4. `supports` and `refutes` use a system-role override

The `supports` and `refutes` pair from ADR-055 does not defeat the `Po` eliminator. A single
`assesses` relation with `polarity = supports | refutes` can represent the same edge facts.

The pair remains in the closed set because ADR-055 makes epistemic polarity a first-class public
query and indexing distinction. A caller can select either relation through the same relation
filter used for every other edge, without interpreting an open metadata object. This is an explicit
system-role override, not a claim that the pair is algebraically irreducible.

### 5. `contains` and `part_of` are distinct, not converses

`contains` denotes housing or scope. `part_of` denotes constitution or membership. They may both
apply to one pair, but neither follows from the other.

A synthetic counterexample is an aquarium that contains a fish. The fish is located within the
aquarium but is not a constituent part of the aquarium. Deriving `part_of(fish, aquarium)` from
`contains(aquarium, fish)` would therefore add a false edge.

Conversely, a component can be part of an assembly whose storage or administrative scope is
modeled elsewhere. The relations remain separately asserted, and the runtime does not generate
one from the other.

ADR-002's phrase "inverse of `contains`" is replaced by this distinct-relation rule. No schema or
runtime inference change is required.

## Certificate evaluation rules

1. A counterexample must use the public entity kinds and relation enum.
2. Expected results compare identifier sets, not incidental row order.
3. The fixture must test both the accepted relation and the proposed replacement.
4. An endpoint-kind validation failure is a failed fixture, not a counterexample.
5. An override must identify the undefeated eliminator and the public system role that prevails.
6. Adding an eliminator family requires an amendment to this ADR.
7. Removing a relation requires running its existing certificate against the proposed replacement.

## Out of scope

This ADR does not introduce:

- automatic relation composition or transitive closure;
- materialized derived-edge views;
- relation subtyping;
- a structural-role metadata field;
- RDF, OWL, or SHACL entailment rules.

Those features require separate decisions about provenance, invalidation, and query semantics. The
certificate may evaluate them as cheaper representations, but it does not implement them.

## Consequences

### Positive

- Every relation addition follows the same reviewable test.
- Synthetic fixtures avoid dependence on private or mutable graph contents.
- Explicit overrides keep design choices visible when algebra alone is insufficient.
- `contains` and `part_of` now have precise independent semantics.

### Tradeoffs

- The certificate cannot prove that the 17-member set is uniquely correct.
- Maintaining fixtures adds work to relation-set amendments.
- A first-class system role may outweigh a formally cheaper encoding.

## Testing requirements

- Every new relation ADR includes fixtures for all seven eliminator families.
- The fixture runner is deterministic across repeated runs.
- The `contains` and `part_of` counterexample prevents converse inference.
- Relation filters treat `supports` and `refutes` as independent enum values.
- No certificate fixture reads an external or operational database.

## References

- [ADR-001](./ADR-001-entity-kind-taxonomy.md): entity kind taxonomy
- [ADR-002](./ADR-002-edge-ontology.md): edge ontology amended here
- [ADR-013](./ADR-013-note-kind-taxonomy.md): note kind taxonomy
- [ADR-017](./ADR-017-pack-standard.md): pack standard and endpoint declarations
- [ADR-055](./ADR-055-epistemic-edge-relations.md): epistemic relations
