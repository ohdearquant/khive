# ADR-075: OWL/RDF Interoperability -- Publishing the khive Vocabulary and Aligning with External Ontologies

**Status**: Draft\
**Date**: 2026-06-26\
**Authors**: khive maintainers
**Depends on**: ADR-002 (Edge Ontology), ADR-008 (Query Layer Separation -- SPARQL subset),
ADR-017 (Pack Standard)

> **Draft.** This ADR sets direction; it does not yet fix the concrete IRI scheme, the
> vocabulary serialization, or the export wire shape. Those are specified before it moves to
> Proposed. It is recorded now so the interoperability surface is designed deliberately rather
> than accreted.

## Context

khive stores a typed property graph: closed entity kinds (ADR-001), closed edge relations
(ADR-002, 17 in 9 categories), and pack-declared subtypes and additive endpoint rules (ADR-017).
Its query surface is GQL plus a SPARQL subset compiled to SQL
(ADR-008). The model is closed-world and optimized for ingestion and serving at scale.

A large body of knowledge-graph and ontology work lives in the W3C semantic-web stack: RDF as
the data model, OWL/RDFS/SKOS for vocabularies, SPARQL for query, and description-logic
reasoners (for example ELK, HermiT, RDFox) for automated inference over axioms. That stack is
open-world and optimized for reasoning. For khive's maps to be useful to practitioners and tools
in that ecosystem, and for khive to consume external ontologies where they map cleanly, the two
models have to interoperate.

The two traditions are genuinely different. khive is a closed-world property graph; the OWL/DL
tradition is an open-world description-logic model. Interoperability does **not** mean adopting
OWL-DL semantics inside khive. It means publishing khive's structure in RDF so external tools can
consume it, and consuming external RDF where a declared mapping makes it sound. The internal
model is unchanged; an RDF projection is added alongside it.

The motivation is adoption. The semantic-web ecosystem is where much of the established ontology
and knowledge-graph tooling and practice lives. Speaking RDF lets khive's typed graphs be
consumed by that ecosystem without giving up the property-graph architecture that makes khive's
ingestion and serving fast and auditable.

## Decision (directional -- Draft)

### D1: RDF is the interchange format; the internal model is unchanged

khive does not adopt OWL-DL as its internal semantics and does not run a reasoner internally. It
gains an RDF projection: entities and edges are exportable as RDF triples. The property-graph
model, the closed taxonomies, and the storage layer are untouched. RDF is a boundary format, not
a replacement model.

### D2: A stable IRI scheme with explicit identity bridging

Every khive entity, edge relation, and entity-kind token is assigned a dereferenceable IRI under
a khive namespace. Cross-system identity is expressed with `owl:sameAs` (or `skos:exactMatch`
for concept-scheme alignment): a khive IRI is linked to an external IRI for the same thing. This
bridges identity without merging schemas: the two graphs stay distinct and the link records that
two IRIs denote the same entity.

### D3: Publish the khive vocabulary as OWL/RDFS/SKOS

The closed 17 relations and the entity kinds are published as a machine-readable vocabulary:
RDFS/OWL classes and properties for kinds and relations, SKOS for taxonomy labels. This
published vocabulary is the contract external tools align against. It is generated from the same
closed enums and pack declarations that define the internal model, so it cannot drift
from what khive actually stores.

### D4: RDF export first (MVP), Turtle and JSON-LD

The first deliverable is read-only export: serialize a namespace's entities and edges to Turtle
and JSON-LD. Exporting a closed-world graph into RDF is always sound (every stored triple is a
true assertion), so export carries no semantic risk and is immediately useful. External SPARQL
endpoints and reasoners can consume the export. Export ships before import.

### D5: Cross-ontology alignment lives with the pack vocabulary declaration

The mapping from pack-declared subtypes and relations to external ontology IRIs is declared
alongside the pack's internal validation rules. The alignment table extends that vocabulary
declaration with the external IRI each token corresponds to and the mapping predicate (`sameAs`,
`exactMatch`, `broader`, `narrower`). One artifact serves internal validation and external
alignment.

Alignment is **semantic, not lexical**. A relation is mapped by meaning, never by matching
names. A worked caution: an external ontology's `supports` may mean "underpins" -- which
corresponds to khive's `enables`, not to khive's epistemic `supports` (ADR-055). Mapping by name
would assert a falsehood. Every alignment entry is a deliberate semantic claim.

### D6 (phase 2, deferred): RDF import and federated SPARQL

Consuming external RDF into the typed graph, and answering federated SPARQL across a khive
endpoint and an external one, is deferred to a later ADR. Import crosses the closed-world /
open-world boundary: external RDF may assert things khive's closed taxonomies cannot represent
without an explicit mapping, and the absence of a triple in an open-world source does not mean
the corresponding fact is false. Import is gated on the alignment table (D5) being populated for
the source ontology and on a policy for assertions that have no khive mapping.

### Non-goal: OWL-DL reasoning inside khive

khive does not run a description-logic reasoner and does not adopt open-world entailment
internally. External reasoners consume khive's RDF export; khive does not become one. The
division of labor is explicit: external tooling reasons over axioms; khive ingests, stores, and
serves typed graphs.

## Rationale

### Why interoperate at all

The established ontology and knowledge-graph ecosystem speaks RDF/OWL/SPARQL. Interoperating
makes khive's maps consumable there, and lets khive consume external ontologies, without forcing
khive to abandon the closed-world property-graph model that gives it fast, auditable ingestion
and serving.

### Why RDF as a boundary, not a model

OWL-DL's open-world semantics conflict with khive's closed taxonomies and ingestion-optimized
storage. Adopting it internally would mean giving up the guarantees the closed model provides and
taking on reasoning khive is not built for. Projecting to RDF at the boundary gets the
interchange benefit while keeping the internal model intact.

### Why export first, import later

Export of a closed-world graph into RDF is unconditionally sound and immediately useful, so it is
the right MVP. Import is where the world-assumption mismatch bites: open-world sources, missing
mappings, and the meaning of absence all have to be resolved first. Shipping export early
delivers value while the harder import questions are worked out.

### Why alignment belongs with the vocabulary declaration

The pack declaration is already the vocabulary source of truth. External alignment is the same
vocabulary, mapped outward. Keeping it in one artifact avoids a second registry that must be kept
in sync with the first.

## Alternatives Considered

**A1: Adopt OWL-DL as the internal model.** Rejected. Open-world DL semantics conflict with
khive's closed taxonomies and property-graph storage; reasoning is not khive's role. This would
trade away the architecture's core strengths for a model khive is not optimized to run.

**A2: No interoperability; stay property-graph-only.** Rejected. This forecloses adoption by the
semantic-web ecosystem and prevents consuming external ontologies. The cost of an RDF projection
is small relative to the reach it buys.

**A3: Lexical alignment (map relations by name match).** Rejected. Name collisions make lexical
mapping unsound -- the `supports` false-friend is a concrete example where matching names would
assert the wrong relation. Alignment must be by meaning.

**A4: A standalone alignment registry separate from the pack declaration.** Rejected. This splits
the vocabulary contract across two artifacts that must agree. Alignment belongs with the
vocabulary source of truth.

## Consequences

### Positive

- khive maps become consumable by RDF/SPARQL/OWL tooling, broadening adoption.
- Cross-system identity is expressed explicitly via `owl:sameAs`/`skos:exactMatch`.
- The vocabulary is published as a stable, machine-readable contract, generated from the internal
  source so it cannot drift.
- Export is sound and ships early; the risky surface (import) is deferred deliberately.

### Negative / Open

- Import (D6) is unsolved and crosses the world-assumption boundary; it needs its own ADR.
- Alignment tables are manual semantic work, one per external ontology.
- The published vocabulary must track the closed enums as they evolve (mitigated by generating it
  from the same source).
- The concrete IRI minting rules and serialization details are not yet fixed (this is a Draft).

### Neutral

- The internal model, query layer, and storage are unchanged; this is additive surface.

## Implementation (sketch -- Draft)

1. Define the khive IRI namespace and the minting rules for entity, relation, and kind IRIs.
2. Generate the khive OWL/RDFS/SKOS vocabulary from the closed enums and pack
   declarations.
3. Build the RDF exporter (Turtle and JSON-LD) over a namespace's entities and edges.
4. Extend the pack vocabulary declaration with an optional alignment table: khive token ->
   external IRI plus mapping predicate.
5. Phase 2 (separate ADR): RDF import and federated SPARQL, gated on alignment coverage and an
   unmappable-assertion policy.

## References

- ADR-002: Edge Ontology -- the closed 17 relations published as the OWL/RDFS property set
- ADR-008: Query Layer Separation -- the existing SPARQL subset over SQL
- ADR-055: Epistemic Edge Relations -- `supports`/`refutes`; the `supports` false-friend in D5
- ADR-017: Pack Standard -- the vocabulary declaration the alignment table extends
- W3C RDF 1.1, OWL 2, RDFS, SKOS, SPARQL 1.1 -- the interchange standards targeted
