# ADR-196: The `located_in` Relation

- **Status**: Proposed (2026-09-29)
- **Date**: 2026-09-29
- **Amends**: [ADR-002](ADR-002-edge-ontology.md) (closed edge ontology, 18 → 19 relations; Category 1
  Structure gains one row)
- **Depends on**: [ADR-076](ADR-076-relation-calculability-and-system-role.md) (non-redundancy
  certificate), [ADR-069](ADR-069-subject-model.md) and [ADR-072](ADR-072-subject-ontologyspec-as-data.md)
  (Subjects declare their vocabulary and map it onto the closed set),
  [ADR-075](ADR-075-owl-rdf-interoperability.md) (alignment is semantic, not lexical)
- **Precedent**: [ADR-191](ADR-191-web-pack-ontology-and-operations.md) D2 (`links_to`, the last relation
  admitted because no existing relation expressed it without a false claim)

## Context

Subjects ingest a domain's declared structure and map each native predicate onto the closed relation
set by meaning (ADR-069, ADR-075 D5). A predicate with no honest mapping becomes an entity property,
per ADR-002's rule: "If a relationship doesn't fit, it's either an entity property or it doesn't belong
in the graph."

Clinical terminologies expose a predicate that fails that mapping and is too central to leave as a
property: **location**. In SNOMED CT, `Finding site (attribute)` places a disorder or finding in a body
structure (pneumonia in the lung, diabetic nephropathy in the kidney). It is the most frequent defining
attribute after is-a: the US edition carries over 100,000 inferred finding-site relationships. The
OBO Relation Ontology defines the same relation as `RO:0001025 located in`, and the Biolink Model as
`located_in`.

Every existing relation either asserts something false about that pair or loses a query:

| Candidate                 | Why it fails                                                                                                                                                                                                 |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `part_of`                 | Member/constitution (ADR-076). Pneumonia is not a constituent of the lung; a mereological closure over `part_of` would list diseases among the lung's parts.                                                 |
| `contains` (reversed)     | Composition, parent → child. "The lung contains pneumonia" puts a disorder into the anatomy's composition tree, the same error from the other side.                                                          |
| `links_to`                | A document's reference to another document (ADR-191). A disorder does not reference its site.                                                                                                                |
| `depends_on`              | A hard requirement. A disorder does not require an organ in the dependency sense, and the runtime stamps dependency qualifiers on some pairs.                                                                |
| property (`finding_site`) | Honest, but opaque to every generic surface: `neighbors`, graph context, traversal, export, and query cannot see it, so a domain pack must rebuild a private reverse index to answer "what is located in X". |

Location is not specific to medicine: a device located in an anatomical region, a facility in a region,
an event at a site. Those domains currently face the same choice between a false edge and an opaque
property.

## Decision

### D1: One new base relation, `located_in`

| relation     | category  | direction          | coherence class                                                 | cascade |
| ------------ | --------- | ------------------ | --------------------------------------------------------------- | ------- |
| `located_in` | Structure | located → location | order-like; a reciprocal pair (A in B and B in A) is incoherent | none    |

Definition: the source occupies, or is manifested in, the target; the source is not a constituent of the
target. Where constitution also holds, assert `part_of` separately; the two are orthogonal assertions,
as `derived_from` and `supersedes` are (ADR-002).

**Base endpoint contract**: one row, `Concept located_in Concept`. Packs and Subjects narrow the
meaning with `EntityOfType` rules for their subtypes (for example, a clinical Subject's
`clinical_concept located_in body_site` and `body_site located_in body_site`). Rows for other base kinds
(artifact, service, resource) are left to the first pack that emits them, through the same additive
mechanism.

### D2: Query semantics

`located_in` is not transitive on its own, and the runtime materializes nothing. The chain a reader
needs is the Relation Ontology's: `X located_in Y` and `Y part_of Z` imply that X is located in Z. A
reader that wants "everything located in the respiratory system" composes `located_in` with the
`part_of` closure of the target; it never walks `located_in` twice.

### D3: What changes

The relation enum and its name list; the `ALL` length assertion (18 → 19); the certificate coverage walk
(a disposition entry for `located_in`, with the fixtures below); the endpoint-signature tripwire; the
ADR-002 relation, category, endpoint-contract, reciprocal-pair and cascade tables; and every in-tree
statement of the relation count. The vocabulary ADR-075 D3 publishes carries `located_in` with
`owl:equivalentProperty` to `RO:0001025` (`http://purl.obolibrary.org/obo/RO_0001025`); no vocabulary
export ships in the tree yet, so this ADR records the alignment until one does.

The base row gives `located_in` the same endpoint signature as `extends` (`Concept -> Concept`). The
endpoint-signature tripwire (ADR-076 D2) treats an identical signature as an Er signal, not as proof of
redundancy. This collision is resolved by the Er fixture below and recorded as a closed, separately
guarded exemption beside the ratified `supports`/`refutes` pair. In ADR-002's reciprocal-pair
classification `located_in` is order-like: A in B and B in A contradict each other.

## Non-redundancy certificate (ADR-076 D2)

Each eliminator is defeated by a fixture of four entities: `lung` and `respiratory system`
(anatomy), `right lung` (anatomy), `pneumonia` (disorder), with `right lung part_of lung`,
`lung part_of respiratory system`, and `pneumonia located_in lung`.

| Family | Hypothesis                                           | Defeating fixture and query                                                                                                                                                                                                                                                                            |
| ------ | ---------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Cv     | converse of `contains`                               | "Parts of the lung" via `contains` from lung returns right lung and, under the converse encoding, pneumonia. The standalone relation returns right lung only.                                                                                                                                          |
| Er     | `part_of` restricted to disorder → anatomy endpoints | Add `thoracic cavity` and `lung located_in thoracic cavity` (the Relation Ontology's own example: an organ occupies a space it does not constitute). The pair is anatomy → anatomy, which the restriction excludes, and encoding it as unrestricted `part_of` makes the lung a constituent of a space. |
| At     | `part_of` with `metadata.role = location`            | Any reader of `part_of` that does not filter on the attribute (every generic closure today) lists pneumonia among the parts of the respiratory system.                                                                                                                                                 |
| Po     | an existing relation with a polarity attribute       | Not applicable: no existing relation is the negation of location. Defeated vacuously, recorded as such.                                                                                                                                                                                                |
| Ch     | a chain of existing relations                        | No composition of the existing relations derives `pneumonia → lung`; the fact exists only as asserted.                                                                                                                                                                                                 |
| Mv     | a reachability view over existing relations          | Same as Ch: nothing to materialize from.                                                                                                                                                                                                                                                               |
| Sr     | a typed sub-relation of `links_to`                   | The parent's meaning (a document references a document) is false for pneumonia → lung; a reader of the parent draws a false conclusion.                                                                                                                                                                |

## Consequences

- A clinical Subject maps SNOMED CT `Finding site` to an edge instead of a property, and generic
  graph tools see the anatomy of every disorder. `Procedure site` does not map to `located_in`: a
  procedure occurs at its site rather than being located in it (the Relation Ontology's separate
  `occurs in`, RO:0002231), so it stays a property until a process-location relation passes its own
  certificate.
- Queries such as "which conditions in this record affect the same organ" and "which coded conditions
  are located in the left shoulder" become ordinary traversals.
- The closed set grows by one, under the certificate. The cost is the usual one for a new relation:
  enum, tables, vocabulary export, and every consumer that enumerates relations.

## Alternatives considered

- **Keep location as a property.** Rejected for the reason in the table: a property is invisible to the
  graph surfaces the closed ontology exists to serve, and each domain pack rebuilds the same reverse
  index privately.
- **A generic `related_to`.** Rejected by ADR-002's own rationale (synonym pollution, no semantics).
- **`has_location` in the other direction (location → located).** Rejected: the Relation Ontology,
  Biolink, and SNOMED CT all state the relation from the located thing, and ingestion should not invert
  every row.
