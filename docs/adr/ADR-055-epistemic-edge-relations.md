# ADR-055: Epistemic Edge Relations — `supports` and `refutes`

**Status**: Accepted
**Date**: 2026-06-14
**Authors**: khive maintainers
**Amends**: [ADR-002](./ADR-002-edge-ontology.md), expanding the closed set from
15 to 17 relations and adding an epistemic category.

## Context

The original ontology cannot answer a basic evidence query with polarity and strength:
which records support a claim, which refute it, and how strong is each link?

`introduced_by` records origin, `derived_from` records material provenance,
`enables` records dependency, and `annotates` records commentary. None distinguishes
evidence for a claim from evidence against it.

## Decision

Add two directional relations:

| Relation   | Direction        | Meaning                                               |
| ---------- | ---------------- | ----------------------------------------------------- |
| `supports` | evidence → claim | The source corroborates the target claim.             |
| `refutes`  | evidence → claim | The source contradicts or falsifies the target claim. |

The relation carries polarity. The existing non-negative edge weight carries evidential
strength:

```text
1.0      definitional or direct replication
0.7-0.9  strong evidence
0.4-0.6  plausible or suggestive evidence
<0.4     weak or speculative evidence
```

A weight of 0.9 on `refutes` means strong evidence against the claim. Negative weights are
invalid.

### Direction

Both relations are asymmetric. Inverse queries use incoming-edge traversal. No write-time
endpoint canonicalization is applied.

### Same-substrate rule

`supports` and `refutes` allow:

- note → note; or
- entity → entity.

They never cross substrates. `annotates` remains the only base relation that crosses
between note and entity substrates.

### Note rail

Any note kind may be the evidence or claim endpoint. This supports claims represented as
questions, insights, decisions, or observations without requiring entity promotion.

### Entity rail

The base entity endpoint rules are:

| Source kind | Relation               | Target kind |
| ----------- | ---------------------- | ----------- |
| `concept`   | `supports` / `refutes` | `concept`   |
| `document`  | `supports` / `refutes` | `concept`   |
| `dataset`   | `supports` / `refutes` | `concept`   |
| `artifact`  | `supports` / `refutes` | `concept`   |

The target is always `concept` in the base entity contract. Additive pack
`EDGE_RULES` may admit other entity endpoints without changing these base rules.

Event and edge records are invalid endpoints.

### Claim representation

A claim may be:

- a note, linked through the note rail; or
- a `concept` entity, linked through the entity rail.

Consumers choose the representation appropriate to their data. A note cannot directly
`support` or `refute` a concept entity; either keep both endpoints as notes or promote
the evidence to an allowed entity kind.

### Deletion

Hard deletion of either endpoint cascades its incident epistemic edges and emits an
evidential-link-loss event. Deletion is not blocked.

## Limitations

- There is no neutral, inconclusive, or “tests” relation.
- Person and organization entities are not evidence sources in the base contract.
- Methodological distinctions such as citation, replication, and consistency remain
  represented through ordinary provenance plus weight, not new epistemic relations.
- Weight captures confidence in the link, not sample count or a posterior distribution.

Each limitation may be addressed by an additive amendment after a concrete query contract
requires it.

## Invariants

- The persistent edge ontology has exactly 17 relations after this amendment.
- Polarity is never encoded in the sign of weight.
- `supports` and `refutes` are directional.
- Endpoints are same-substrate.
- Entity targets are `concept` unless an additive pack rule admits more.
- Parsers, formatters, schema output, and relation enumerations are exhaustive.

## Migration

No database migration is required. Edge relation values are stored in a text column without
a SQL relation-enum constraint. The binary's closed enum and runtime validation remain the
authority.

## Implementation

`EdgeCategory` gains `Epistemic`. `EdgeRelation` gains `Supports` and
`Refutes`. The following surfaces must change together:

- enum declaration and `ALL` constant;
- string parser and formatter;
- category mapping;
- symmetry classification;
- base endpoint rules;
- schema and help introspection;
- import/export validation; and
- ontology certificate tests.

## Verification

Tests must cover:

- all allowed note and entity endpoint combinations;
- cross-substrate and invalid-kind rejection;
- incoming and outgoing traversal;
- non-negative weight behavior;
- parser/formatter round trips;
- synthetic import/export round trips;
- hard-delete cascade and warning event; and
- the relation-count certificate reporting 17.

## Rationale

The pair is the smallest addition that answers evidence-for and evidence-against queries.
Overloading `annotates` would discard polarity. Adding narrower methodological relations
would expand the ontology before separate query requirements exist.

## Consequences

### Positive

- Evidence polarity and strength become directly queryable.
- Notes can model claims without entity promotion.
- The existing edge storage and query machinery requires no schema change.

### Negative

- Neutral and method-specific evidence remains lossy.
- Mixed note/entity evidence requires representation alignment.
- Every closed-set consumer must add two enum values.

## References

- [ADR-002](./ADR-002-edge-ontology.md): closed edge ontology and weight semantics
- [ADR-017](./ADR-017-pack-standard.md): additive endpoint rules
- [ADR-076](./ADR-076-relation-calculability-and-system-role.md): relation certificate
