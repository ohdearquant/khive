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

This ADR serves a producer's request of 2026-10-05: a holder recorded as `part_of` a company it holds
a stake in is wrong, and the graph needs a relation that states the holding itself.

An organization graph built from public filings records who holds a stake in whom. A beneficial
owner of more than five percent of a class of a company's registered equity securities files a
statement that names the holder, the company, the percentage of the class, and the date. Funds,
parent companies and individuals hold stakes; many holders own a few percent of one company, and one
holder owns stakes in many companies. The questions asked of such a graph are "who holds a stake in X",
"what does Y hold", and "which holders hold stakes in both X and Z".

The base contract and the `kg` pack's endpoint rules allow organization → organization only as
`depends_on`, `enables`, `contains`, `part_of`, `precedes` and `competes_with`, and person → organization
only as `part_of` and `instance_of`. Every candidate asserts something false about a stake or loses a
query:

| Candidate                           | Why it fails                                                                                                                                                                                                                                                                                                                                   |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `part_of`                           | Member/constitution (ADR-076). `Person part_of Org` already means a member or employee and `Org part_of Org` a subsidiary (ADR-002, kg pack extensions). A fund holding seven percent of a company is neither. A person who is both an employee and a shareholder needs two facts, and the unique edge index holds one `part_of` row per pair. |
| `contains` (reversed)               | Composition, parent → child. "The company contains its shareholder" puts the holder into the company's organizational tree.                                                                                                                                                                                                                    |
| `depends_on` / `enables`            | A hard requirement and an enabling relationship. A stake is neither, and `depends_on` carries governed qualifiers for build, runtime and data dependencies.                                                                                                                                                                                    |
| `instance_of`                       | Classification. A holder is not a kind of the company.                                                                                                                                                                                                                                                                                         |
| property (`holders` on the company) | `neighbors`, `traverse` and `context` do not see it. `query` can read it, but a holder list stored on each company answers "what does Y hold" only by scanning every organization's property.                                                                                                                                                  |

Ownership is not specific to filings: a parent holding a joint venture, a person holding shares in a
private company, and an investment fund's portfolio are the same fact.

## Decision

### D1: One new base relation, `owns`, in a new Ownership category

| relation | category  | direction     | coherence class                                                                   | cascade                           |
| -------- | --------- | ------------- | --------------------------------------------------------------------------------- | --------------------------------- |
| `owns`   | Ownership | owner → owned | state-like; a reciprocal pair (A owns part of B and B owns part of A) is coherent | cascade normally (no audit event) |

Definition: the source holds an ownership interest, whole or partial, in the target, now. The interest
may be direct or beneficial. Where the source also controls or contains the target, assert `contains`
or `part_of` separately; ownership and organizational structure are orthogonal assertions, as
`part_of` and `located_in` are (ADR-196).

A reciprocal pair is coherent because cross-shareholdings exist: two companies can each hold a stake
in the other. Mutual whole ownership is not, and a curation pass can find it from `pct` on both edges.
A self-loop stays rejected at the endpoint-validation seam (ADR-002), so a company's holding of its
own shares stays a property of the company. Hard delete of either endpoint removes the edge with the
other incident edges, as ADR-002's cascade rule does for every relation without a provenance warning.

The category is new because ownership answers a query class no existing category covers ("who holds
X", "what does Y hold"). It is not Structure: Structure is composition, classification and reference,
and ADR-196 already widened it to carry location; a stake is none of these, and placing it there would
widen the category again. `EdgeCategory` is descriptive and no runtime behavior is keyed on it, so the
cost of the tenth category is its enum variant and the ADR-002 tables. ADR-002's rule that categories
follow query semantics, not relation counts, admits a single-relation category, as it admits
Implementation and Annotation.

**Base endpoint contract**: two rows, `Person owns Org` and `Org owns Org`. Ownership of other base
kinds (a project, an artifact, a dataset) enters the base contract by amendment when a producer needs
it, as the 2026-10-05 organization rows did, because the additive `EDGE_RULES` route needs a pack linked
into the binary. A pack that defines its own subtypes narrows the relation with `EntityOfType` endpoint rules, as
ADR-196 D1 describes.

### D2: Weight, metadata, and change over time

`weight` keeps its ADR-002 meaning, the confidence of the assertion (`1.0` definitional to `<0.4`
speculative). It never carries the size of the stake. Every reader that filters by weight would
otherwise drop small holders as if they were uncertain.

The size and the date of the stake go in edge metadata, with conventional keys:

| key          | meaning                                                                                                                                                      |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `pct`        | percentage held, `0 < pct <= 100`, of the class named by `class`, or of the whole owned entity when `class` is absent                                        |
| `class`      | the class of securities `pct` refers to, as the report names it                                                                                              |
| `as_of`      | ISO-8601 date the percentage was reported for                                                                                                                |
| `valid_from` | ISO-8601 date the interest began, when known                                                                                                                 |
| `by_class`   | for a holder with stakes in more than one class of one owned entity: a list of `{class, pct, as_of}`, one entry per class; `pct` and `class` are then absent |

A reader therefore knows from the edge alone what a percentage is a percentage of. Stakes in several
classes stay on the one edge the unique index allows for the pair, in `by_class`.

The keys are conventional, not governed: ADR-002 governs metadata only where one relation carries
several meanings that need different traversals (`depends_on`), and `owns` carries one. A producer may
add its own source citations beside them.

An `owns` edge states a holding that exists now. The edge is unique per
`(namespace, source_id, target_id, relation)`, so a holder's changing stake in one company is one edge
whose metadata the producer updates to the latest report. When the holding ends, the producer deletes
the edge; the graph verbs then stop returning a former holder, and "which holders hold stakes in both X
and Z" stays a question about current holdings. If the holder acquires a stake again, `link` with
`resurrect: true` restores the edge with the new metadata. The history of reports is not the edge's: a
producer that keeps it records each report as its own record, for example a note per filing.

### D3: Query semantics

`owns` is not transitive, and the runtime materializes nothing. Indirect ownership (A holds 50% of B,
B holds 40% of C) is a computation over `pct` along a path, done by the reader that needs it; walking
`owns` twice without that arithmetic would claim that a holder of a holder owns the company outright.
`neighbors` returns each edge's id, relation and weight, and `traverse` returns paths whose nodes carry
the id of the edge that reached them (`via_edge`); neither returns the edge's metadata, so a reader
that needs `pct` reads the edges by those ids, or through `query`. Control (a majority of
votes, a right to appoint the board) is not asserted by `owns` and is not derived from it by the
runtime.

### D4: What changes

The relation enum and its name list; the `ALL` length assertion (19 → 20); the `EdgeCategory` enum
(9 → 10) and the relation-to-category map; the certificate coverage walk (a disposition entry for
`owns`, with the fixtures below); the endpoint-signature tripwire; the ADR-002 relation, category,
endpoint-contract, reciprocal-pair and cascade tables; and every in-tree statement of the relation and
category counts. The vocabulary ADR-075 D3 publishes aligns `owns` with Wikidata `P1830` ("owner of",
stated from the owner), whose inverse is `P127` ("owned by"); Wikidata records the size of a stake with
the qualifier `P1107` ("proportion", a value between 0 and 1), which maps to `pct / 100`. No vocabulary
export ships in the tree yet, so this ADR records the alignment until one does.

## Non-redundancy certificate (ADR-076 D2)

The fixture has four organizations (`Parent Co`, `Subsidiary Co`, `Target Co`, `Index Fund`) and two
people (`Jane Doe`, `John Roe`), with these edges:

- `Subsidiary Co part_of Parent Co` (a subsidiary)
- `Parent Co owns Subsidiary Co` (`pct` 80) and `Index Fund owns Subsidiary Co` (`pct` 6)
- `Index Fund owns Target Co` (`pct` 7) and `Jane Doe owns Target Co` (`pct` 0.5)
- `Jane Doe part_of Target Co` and `John Roe part_of Target Co` (employees; John Roe holds no stake)

Under each cheaper encoding, every `owns` edge above is written in that encoding instead.

| Family | Hypothesis                                             | Defeating fixture and query                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ------ | ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cv     | converse of `contains`                                 | Under the converse encoding `Target Co contains Jane Doe` is refused at write time: the endpoint contract has no `Org contains Person` row, so a person's stake cannot be stored at all. The organization stakes it can store enter the organizational tree: `neighbors(Target Co, relations=[contains], direction=outgoing)` ("the organizational tree under Target Co") returns Index Fund. With `owns` it returns nothing, and Jane Doe's stake is stored.                                     |
| Er     | `part_of` restricted to person/org → org endpoints     | `neighbors(Target Co, relations=[part_of], direction=incoming)` as "shareholders of Target Co" returns John Roe, who holds no stake. With `owns` it returns Index Fund and Jane Doe. The encoding also cannot store Jane Doe's two facts: the unique index holds one `part_of` row for Jane Doe → Target Co, so deleting her employment when she leaves deletes her stake.                                                                                                                        |
| At     | `part_of` with `metadata.predicate = beneficial_owner` | `traverse(roots=[Parent Co], relations=[part_of], direction=incoming)`, the parts of Parent Co, returns Subsidiary Co and, through it, Index Fund. `traverse` filters by relation and weight only, so no call to it can exclude the stake. With `owns`, the same call returns Subsidiary Co only. Jane Doe's employment and stake again collide on one row.                                                                                                                                       |
| Po     | an existing relation with a polarity attribute         | Instantiated on the nearest host, `part_of` with a sign attribute (`sign = holding`, read as "is held by" rather than "is a member of"), every stake written as holder `part_of` company. A sign-blind reader asks for the members of Target Co with `neighbors(Target Co, relations=[part_of], direction=incoming)` and gets Index Fund beside Jane Doe and John Roe. With `owns` the same call returns Jane Doe and John Roe only. Jane Doe's employment and stake collide on one row here too. |
| Ch     | a chain of existing relations                          | "What does Index Fund hold": with the `owns` edges removed, Index Fund has no incident edge, so every chain of existing relations from it returns nothing. `neighbors(Index Fund, relations=[owns], direction=outgoing)` returns Subsidiary Co and Target Co.                                                                                                                                                                                                                                     |
| Mv     | a reachability view over existing relations            | The same query: a reachability view over the existing relations from Index Fund is empty, because Index Fund has no other edge.                                                                                                                                                                                                                                                                                                                                                                   |
| Sr     | a typed sub-relation of `part_of`                      | A reader of the parent relation that does not know the sub-type runs the At query and gets Index Fund as a part of Parent Co. The sub-relation also points the wrong way for the parent: `part_of` runs part → whole, and `Parent Co owns Subsidiary Co` holds beside `Subsidiary Co part_of Parent Co` with both directions true at once, which a sub-relation of `part_of` cannot state.                                                                                                        |

## Consequences

- A filings graph stores current beneficial ownership as `owns` with `pct` and `as_of`, and "who holds
  X" and "what does Y hold" become one-hop traversals in either direction. Readers that use `part_of`
  for membership and subsidiaries stop seeing shareholders.
- A producer that encoded stakes as `part_of` with a predicate attribute migrates those edges to `owns`
  after the relation ships; the encoding has no other consumer to break.
- The closed set grows by one, under the certificate, and the category count by one. The cost is the
  usual one for a new relation: enum, tables, vocabulary export, and every consumer that enumerates
  relations or categories.

## Alternatives considered

- **Keep `part_of` with a predicate attribute.** Rejected by the At row: the graph verbs cannot filter
  on it, so every reader has to fetch the edges and filter them itself.
- **`owned_by` in the other direction (owned → owner).** Rejected: filings, Wikidata `P1830` and
  ordinary usage state ownership from the holder, and the queries run in both directions anyway.
- **A `stake` entity between holder and company.** Reifies a two-place fact with one quantity into an
  entity and two edges, and the two edges would still need a relation that means "holds". Rejected
  while a stake carries no facts beyond its size and dates.
- **Keep ended holdings as edges with a `valid_to` date.** Rejected: `neighbors`, `traverse` and
  `context` cannot filter on metadata, so every traversal would return former holders as current ones.
- **Put `owns` in Structure.** Rejected: Structure is composition, classification and reference (and,
  since ADR-196, location), and a stake is none of them.
