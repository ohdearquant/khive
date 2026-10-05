# ADR-197: The `owns` Relation

- **Status**: Proposed (2026-10-05)
- **Date**: 2026-10-05
- **Amends**: [ADR-002](ADR-002-edge-ontology.md) (closed edge ontology, 19 → 20 relations; a tenth
  category, Ownership)
- **Depends on**: [ADR-076](ADR-076-relation-calculability-and-system-role.md) (non-redundancy
  certificate), [ADR-075](ADR-075-owl-rdf-interoperability.md) (alignment is semantic, not lexical)
- **Precedent**: [ADR-196](ADR-196-located-in-relation.md) (`located_in`) and
  [ADR-191](ADR-191-web-pack-ontology-and-operations.md) D2 (`links_to`), the last relations admitted
  because no existing relation expressed them without a false claim

## Context

An organization graph built from public filings records who holds a stake in whom. A beneficial
owner of more than five percent of a class of a company's registered equity securities files a
statement that names the holder, the company, the percentage of the class, and the date. Funds,
parent companies and individuals hold stakes; many holders own a few percent of one company, and one
holder owns stakes in many companies. The questions asked of such a graph are "who holds a stake in X",
"what does Y hold", and "which holders appear in two companies' ownership at once".

The base contract and the `kg` pack's endpoint rules allow organization → organization only as
`depends_on`, `enables`, `contains`, `part_of`, `precedes` and `competes_with`, and person → organization
only as `part_of` and `instance_of`. Every candidate asserts something false about a stake or loses a query:

| Candidate                           | Why it fails                                                                                                                                                                                                                                                                                                                                      |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `part_of`                           | Member/constitution (ADR-076). `Person part_of Org` already means a member or employee and `Org part_of Org` a subsidiary (ADR-002, kg pack extensions). A fund holding seven percent of a company is neither, and a reader that takes the `part_of` closure of a company (ADR-196 D2 composes with one) lists every shareholder among its parts. |
| `contains` (reversed)               | Composition, parent → child. "The company contains its shareholder" puts the holder into the company's organizational tree.                                                                                                                                                                                                                       |
| `depends_on` / `enables`            | A hard requirement and an enabling relationship. A stake is neither, and `depends_on` carries governed qualifiers for build, runtime and data dependencies.                                                                                                                                                                                       |
| `instance_of`                       | Classification. A holder is not a kind of the company.                                                                                                                                                                                                                                                                                            |
| property (`holders` on the company) | Honest, but invisible to `neighbors`, graph context, traversal, export and query, and a list of holders on the company cannot answer "what does Y hold" without scanning every organization.                                                                                                                                                      |

Ownership is not specific to filings: a parent holding a joint venture, a person holding shares in a
private company, and an investment fund's portfolio are the same fact.

## Decision

### D1: One new base relation, `owns`, in a new Ownership category

| relation | category  | direction     | coherence class                                                                   | cascade |
| -------- | --------- | ------------- | --------------------------------------------------------------------------------- | ------- |
| `owns`   | Ownership | owner → owned | state-like; a reciprocal pair (A owns part of B and B owns part of A) is coherent | none    |

Definition: the source holds an ownership interest, whole or partial, in the target. The interest may
be direct or beneficial. Where the source also controls or contains the target, assert `contains` or
`part_of` separately; ownership and organizational structure are orthogonal assertions, as `part_of`
and `located_in` are (ADR-196).

A reciprocal pair is coherent because cross-shareholdings exist: two companies can each hold a stake
in the other. A self-loop stays rejected at the substrate (ADR-002), so a company's holding of its own
shares is not represented as an edge.

The category is new because ownership answers a query class no existing category covers ("who holds
X", "what does Y hold"). It is not Structure: a stake does not place the holder inside the owned
organization's composition. ADR-002's rule that categories follow query semantics, not relation
counts, admits a single-relation category, as it admits Implementation and Annotation.

**Base endpoint contract**: two rows, `Person owns Org` and `Org owns Org`. Both are base kinds. Rows
for other owned kinds (a project, an artifact, a resource) are left to the first pack that emits them,
through the additive `EDGE_RULES` mechanism, as ADR-196 D1 left `located_in` rows.

### D2: Weight and metadata

`weight` keeps its ADR-002 meaning, the confidence of the assertion (`1.0` definitional to `<0.4`
speculative). It never carries the size of the stake. Every reader that filters by weight would
otherwise drop small holders as if they were uncertain.

The size and the date of the stake go in edge metadata, with conventional keys:

| key          | meaning                                                            |
| ------------ | ------------------------------------------------------------------ |
| `pct`        | percentage of the class or of the owned entity, `0 < pct <= 100`   |
| `as_of`      | ISO-8601 date the percentage was reported for                      |
| `valid_from` | ISO-8601 date the interest began, when known                       |
| `valid_to`   | ISO-8601 date the interest ended; an ended interest keeps its edge |

The keys are conventional, not governed: ADR-002 governs metadata only where one relation carries
several meanings that need different traversals (`depends_on`), and `owns` carries one. A producer may
add its own source citations beside them.

The edge is unique per `(namespace, source, target, relation)`. A holder's changing stake in one
company is one edge whose metadata states the latest report; a producer that keeps the history of
reports keeps it in that edge's metadata.

### D3: Query semantics

`owns` is not transitive, and the runtime materializes nothing. Indirect ownership (A holds 50% of B,
B holds 40% of C) is a computation over `pct` along a path, done by the reader that needs it; walking
`owns` twice without that arithmetic would claim that a holder of a holder owns the company outright.
Control (a majority of votes, a right to appoint the board) is not asserted by `owns` and is not
derived from it by the runtime.

### D4: What changes

The relation enum and its name list; the `ALL` length assertion (19 → 20); the certificate coverage
walk (a disposition entry for `owns`, with the fixtures below); the endpoint-signature tripwire; the
ADR-002 relation, category, endpoint-contract, reciprocal-pair and cascade tables; the ADR-002 category
count (9 → 10); and every in-tree statement of the relation count. The vocabulary ADR-075 D3 publishes
aligns `owns` with Wikidata `P1830` ("owner of", stated from the owner), whose inverse is `P127`
("owned by"); Wikidata records the size of a stake with the qualifier `P1107` ("proportion", a value
between 0 and 1), which maps to `pct / 100`. No vocabulary export ships in the tree yet, so this ADR
records the alignment until one does.

## Non-redundancy certificate (ADR-076 D2)

Each eliminator is defeated by a fixture of five entities: `Parent Co` and `Target Co` (organizations),
`Subsidiary Co` (organization), `Index Fund` (organization) and `Jane Doe` (person), with
`Subsidiary Co part_of Parent Co`, `Jane Doe part_of Target Co` (an employee), `Index Fund owns
Target Co` (`pct` 7), `Jane Doe owns Target Co` (`pct` 0.5) and `Parent Co owns Subsidiary Co`
(`pct` 100).

| Family | Hypothesis                                             | Defeating fixture and query                                                                                                                                                                                                                                                                                            |
| ------ | ------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cv     | converse of `contains`                                 | "The organizational tree under Target Co" via `contains` from Target Co returns, under the converse encoding, Index Fund and Jane Doe as its children. The standalone relation leaves the tree empty.                                                                                                                  |
| Er     | `part_of` restricted to person/org → org endpoints     | `Jane Doe part_of Target Co` (employee) and `Jane Doe owns Target Co` (shareholder) have the same endpoints. The restriction cannot tell them apart; under it, "employees of Target Co" returns every shareholder and "shareholders of Target Co" returns every employee.                                              |
| At     | `part_of` with `metadata.predicate = beneficial_owner` | `neighbors`, `traverse` and `context` filter edges by relation and weight, never by metadata. "Members of Target Co" via `part_of` in returns Index Fund; a `part_of` closure over Parent Co's subsidiaries lists every holder of every subsidiary as a part of Parent Co.                                             |
| Po     | an existing relation with a polarity attribute         | Not applicable: no existing relation is the negation of ownership. Defeated vacuously, recorded as such.                                                                                                                                                                                                               |
| Ch     | a chain of existing relations                          | No composition of the existing relations derives `Index Fund → Target Co`; the fact exists only as asserted.                                                                                                                                                                                                           |
| Mv     | a reachability view over existing relations            | Same as Ch: nothing to materialize from.                                                                                                                                                                                                                                                                               |
| Sr     | a typed sub-relation of `part_of`                      | The parent's meaning (member or constituent) is false for Index Fund → Target Co, and a reader of the parent that takes its closure draws a false conclusion. `Parent Co owns Subsidiary Co` at 100 alongside `Subsidiary Co part_of Parent Co` shows the two hold independently, which a sub-relation cannot express. |

## Consequences

- A filings graph stores beneficial ownership as `owns` with `pct` and `as_of`, and "who holds X" and
  "what does Y hold" become one-hop traversals in either direction. Readers that use `part_of` for
  membership and subsidiaries stop seeing shareholders.
- A producer that encoded stakes as `part_of` with a predicate attribute migrates those edges to `owns`
  after the relation ships; the encoding has no other consumer to break.
- The closed set grows by one, under the certificate, and the category count by one. The cost is the
  usual one for a new relation: enum, tables, vocabulary export, and every consumer that enumerates
  relations.

## Alternatives considered

- **Keep `part_of` with a predicate attribute.** Rejected by the At row: the graph verbs cannot filter
  on it, so every reader has to fetch the edges and filter them itself.
- **`owned_by` in the other direction (owned → owner).** Rejected: filings, Wikidata `P1830` and
  ordinary usage state ownership from the holder, and the queries run in both directions anyway.
- **A `stake` entity between holder and company.** Reifies a two-place fact with one quantity into an
  entity and two edges, and the two edges would still need a relation that means "holds". Rejected
  while a stake carries no facts beyond its size and dates.
- **Put `owns` in Structure.** Rejected: Structure is composition, classification and reference, and a
  stake is none of them.
