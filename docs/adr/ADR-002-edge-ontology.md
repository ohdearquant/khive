# ADR-002: Closed Edge Ontology

**Status**: accepted\
**Date**: 2026-05-22\
**Authors**: khive maintainers
**Amended by**: [ADR-076](ADR-076-relation-calculability-and-system-role.md) — `part_of` is a
distinct relation, not the "inverse of `contains`"; the two coincide in some domains and diverge
in others, and neither is derived from the other; [ADR-055](ADR-055-epistemic-edge-relations.md)
adds the `supports` and `refutes` epistemic relations.
**Amended 2026-07-08**: base endpoint contract gains four pairs — three provenance
(`Document introduced_by Person`, `Document introduced_by Org`, `Concept introduced_by Org`)
and one dependency (`Document depends_on Document`) — closing a gap where a document's own
authorship and a document's normative dependency on another document had no representable
edge. See "Base endpoint contract" below and "Why the 2026-07-08 endpoint amendment?" in
Rationale.
**Amended 2026-07-27**: base endpoint contract gains one provenance pair —
`Document derived_from Document` — for publication provenance: a curated or filtered
publication copy of a document points at the canonical source it was produced from. See
"Base endpoint contract" below and "Why the 2026-07-27 provenance amendment?" in Rationale.
**Amended 2026-07-31**: hard-delete cascade warnings use the existing `audit` event kind and
carry a relation-specific payload atomically with the delete, as specified in "Cascade Behavior."
**Amended 2026-08-03**: specifies reciprocal same-relation pairs (legal at the substrate,
with a per-relation coherence classification for curation), records the existing self-loop
rejection, and states the delete-then-relink direction rule. See "Reciprocal pairs,
self-loops, and repricing" below. Motivated by issue #1667.
**Amended 2026-08-21 ([ADR-167](ADR-167-service-provenance-and-kind-classification.md))**:
base endpoint contract gains one derivation pair — `Service introduced_by Document` — so a
service can record the specification, ADR, or paper that introduced it. See "Base endpoint
contract" below.

## Context

A knowledge graph is only useful if its edges have consistent semantics. Allowing free-form
relation strings ("uses", "related_to", "contains_module", "loaded_by") leads to:

1. Synonym pollution — `uses` vs `requires` vs `depends_on` all mean the same thing.
2. Ambiguity — `related_to` carries no semantic information.
3. Query brittleness — "find all dependencies of X" must enumerate dozens of synonyms.
4. Agent drift — different agents invent different vocabularies.

A closed ontology — a fixed set of allowed relations — solves these by forcing every edge
into a canonical bucket.

The entity kind taxonomy (ADR-001) defines 8 entity kinds. The edge ontology must define
which `(source_kind, relation, target_kind)` triples are legal for each relation, and provide
enough relations to cover the query classes agents actually need without creating
classification ambiguity.

## Decision

**17 canonical relations, grouped into 9 categories. No others.**

> **Amended 2026-06-14 ([ADR-055](ADR-055-epistemic-edge-relations.md))**: added Category 9
> (Epistemic / Evidential) with `supports` and `refutes`, expanding the closed set from 15 → 17.

### Category 1: Structure (composition and classification)

| Relation      | Direction          | When                                             |
| ------------- | ------------------ | ------------------------------------------------ |
| `contains`    | parent → child     | Crate contains module, system contains component |
| `part_of`     | child → parent     | Member/constitution; distinct from `contains`    |
| `instance_of` | specific → general | One is a case of the other (GPT-4 → Transformer) |

### Category 2: Derivation (intellectual lineage)

| Relation        | Direction                                               | When                                                                                       |
| --------------- | ------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `extends`       | child → parent                                          | Builds on, generalizes (FlashAttention-2 → FlashAttention)                                 |
| `variant_of`    | variant → original                                      | Modified version (QLoRA → LoRA)                                                            |
| `introduced_by` | concept/document/artifact/service → document/person/org | First described in (LoRA → Hu et al. 2021); document authorship (paper → author/publisher) |
| `supersedes`    | new → old                                               | Replaces entirely; old stops being authoritative                                           |

### Category 3: Provenance (material/generative source lineage)

| Relation       | Direction      | When                                                  |
| -------------- | -------------- | ----------------------------------------------------- |
| `derived_from` | output → input | Generated, trained, exported, transformed from source |

### Category 4: Temporal (chronological ordering)

| Relation   | Direction       | When                                  |
| ---------- | --------------- | ------------------------------------- |
| `precedes` | earlier → later | Temporal sequence without replacement |

### Category 5: Dependency (runtime/build needs)

| Relation     | Direction              | When                                              |
| ------------ | ---------------------- | ------------------------------------------------- |
| `depends_on` | consumer → dependency  | Hard requirement                                  |
| `enables`    | prerequisite → outcome | Makes possible (Sinkhorn → Wasserstein attention) |

### Category 6: Implementation (code/service ↔ concept)

| Relation     | Direction          | When                                                   |
| ------------ | ------------------ | ------------------------------------------------------ |
| `implements` | code/svc → concept | Code or service realizes algorithm (Solver → Sinkhorn) |

### Category 7: Lateral (peer relationships)

| Relation        | Direction | When                                             |
| --------------- | --------- | ------------------------------------------------ |
| `competes_with` | A ↔ B     | Alternative approaches (softmax attn ↔ OT attn)  |
| `composed_with` | A ↔ B     | Used together in a system (GDN ↔ GQA in Qwen3.5) |

### Category 8: Annotation (cross-substrate commentary)

| Relation    | Direction       | When                                                     |
| ----------- | --------------- | -------------------------------------------------------- |
| `annotates` | note → anything | A note comments on an entity, edge, event, or other note |

### Category 9: Epistemic / Evidential (evidence for/against a claim)

Added by [ADR-055](ADR-055-epistemic-edge-relations.md). The relation carries the **polarity**
(for vs. against); the edge **weight** carries the **strength** of the evidential link on the
standard scale. Directional (evidence → claim), **not** symmetric.

| Relation   | Direction        | When                                                            |
| ---------- | ---------------- | --------------------------------------------------------------- |
| `supports` | evidence → claim | Evidence **for** the claim (corroborates, confirms, replicates) |
| `refutes`  | evidence → claim | Evidence **against** the claim (contradicts, falsifies)         |

### `supersedes` vs `precedes`

These are the two temporal-adjacent relations. The decision rule:

```text
Does the old record stop being the authoritative reference?

Yes → supersedes (new replaces old)
No  → precedes (order only, both remain valid)
```

If both seem applicable, prefer `supersedes`. Do not create both edges for the same pair
unless there is a strong reason.

| Case                                     | Relation                             |
| ---------------------------------------- | ------------------------------------ |
| Training run 1 before run 2, both valid  | `run_1 -[precedes]-> run_2`          |
| Checkpoint v2 replaces v1                | `v2 -[supersedes]-> v1`              |
| Preprint replaced by published version   | `published -[supersedes]-> preprint` |
| Ablation A before ablation B, both valid | `A -[precedes]-> B`                  |
| Deployment green replaces blue           | `green -[supersedes]-> blue`         |

### `derived_from` semantics

Direction is output → input: the generated thing points to what it was made from.

```text
checkpoint -[derived_from]-> training_set
embedding_index -[derived_from]-> corpus
brain_profile_v2 -[derived_from]-> brain_profile_v1
snapshot -[derived_from]-> project
published_copy -[derived_from]-> canonical_document
```

`derived_from` is for material/generative provenance. It is NOT for intellectual inspiration
(use `extends` or `introduced_by`) or dependency (use `depends_on`).

For documents, `derived_from` records publication provenance: a copy produced from a
canonical source by a content transformation (filtering, subsetting, redaction, format
conversion) is a distinct document entity pointing at its source. The boundary is content
identity. A document that moves keeps one entity — relocation is location history recorded
in properties, not a new document. A document whose content is transformed for a different
audience or authority is a new entity with a `derived_from` edge; it does not share an
entity with its canonical source.

`derived_from` and `supersedes` are orthogonal assertions and may coexist on the same
document pair: production is not replacement. A publication copy points at its canonical
with `derived_from` and carries no `supersedes` — the canonical remains authoritative. A
published version produced from a preprint legitimately carries both: `derived_from`
records the production, `supersedes` records the transfer of authority. Collapsing the two
into one edge loses one of two distinct facts.

### Rules

- Relations not in this list are forbidden.
- If a relationship doesn't fit, it's either an entity property or it doesn't belong in
  the graph.
- Inverse relations are NOT created automatically. `part_of` is a distinct relation, not the
  converse of `contains` (see [ADR-076](ADR-076-relation-calculability-and-system-role.md));
  assert it explicitly when constitution holds. Query with `direction=in` for direction-aware
  traversal of a single relation.
- Edge weight: `1.0` = definitional, `0.7-0.9` = strong, `0.4-0.6` = plausible,
  `<0.4` = speculative.

### Symmetric relation handling

`competes_with` and `composed_with` are semantically bidirectional. Storage is directed.

**Write-time canonicalization**: for symmetric relations, the runtime normalizes direction
so that `source_uuid < target_uuid` lexicographically. This prevents duplicate edges.

**Uniqueness invariant**: `(namespace, relation, canonical_source, canonical_target)` is
unique for symmetric relations.

**Query behavior**: `direction` is ignored for symmetric relations and treated as `both`.
Physical canonical direction is never exposed as semantic direction.

### Reciprocal pairs, self-loops, and repricing (2026-08-03 amendment)

The base contract above addresses inverse _relations_ (`contains` vs `part_of`) and
symmetric canonicalization, but never stated whether two live opposite-direction edges of
the _same_ non-symmetric relation between one pair of nodes is a legal state, nor what a
delete-then-relink workflow may assume. Issue #1667 forced the question: the edge natural
key `(namespace, source_id, target_id, relation)` is direction-sensitive for non-symmetric
relations (canonicalization applies only to `competes_with`/`composed_with`), so a
same-direction re-link revives a soft-deleted row (id and `created_at` preserved) while a
direction-flipped re-link inserts a fresh row — and both rows can then be live at once,
reading as two independent assertions.

**Substrate legality.** A reciprocal pair — `A -[r]-> B` and `B -[r]-> A` both live, `r`
non-symmetric — is legal at the storage layer for every relation. Three reasons:

1. For dependency-category relations a reciprocal pair records a real state: mutual
   dependency and mutual enablement occur in practice, and the reference production store
   carries live `depends_on` reciprocal pairs that are factually correct.
2. A write-time pair rejection would be a length-2 special case of cycle prevention. A
   `supersedes` cycle of length 3 is exactly as incoherent as a reciprocal pair, and no
   pair check can see it. Enforcing only the length-2 case is arbitrary enforcement, not a
   contract; whole-graph coherence is an audit concern (ADR-034 validation pipelines), not
   a per-write concern.
3. Data records assertions; coherence between assertions is judged at the curation layer
   (ADR-014). Refusing the write would conflate the two layers.

**Per-relation coherence classification.** Legal-to-store is not the same as
semantically coherent. For curation and validation-pipeline use (advisory — never
write-time enforcement):

| Class                                   | Relations                                                                                                                              | Reciprocal pair means                                                                                                                                           |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Order-like — reciprocal pair INCOHERENT | `contains`, `part_of`, `instance_of`, `extends`, `variant_of`, `introduced_by`, `supersedes`, `derived_from`, `precedes`, `implements` | The two edges contradict: each claims a directional subordination or ordering the other denies. Surface as curation-review candidates.                          |
| State-like — reciprocal pair COHERENT   | `depends_on`, `enables`, `supports`, `refutes`                                                                                         | The two edges are independent assertions that can both hold (mutual dependency, mutual enablement, claims that each support or refute the other). Not findings. |

`annotates` is note-sourced, so it cannot form an entity-endpoint reciprocal pair and
sits outside the entity census below. Note→note reciprocal `annotates` pairs (note A
annotates note B and note B annotates note A) are endpoint-legal — the contract admits
any substrate UUID as target — and are classified state-like: two annotations are
independent assertions, not findings. Symmetric relations cannot form reciprocal pairs
by canonicalization.

**Self-loops.** `source_id == target_id` is rejected at the endpoint-validation seam
(`validate_edge_relation_endpoints`) for every relation — this amendment records an
existing behavior in the contract rather than introducing it. The rejection covers the
verb surface; writers below the seam (pack-private map databases, direct store accessors)
are outside it by construction, and ADR-034's `no-self-loops` pipeline is the audit that
catches legacy or below-seam rows.

**Delete-then-relink direction rule (normative for callers; non-symmetric relations).**
Repricing an edge means: `get(edge_id)` → read the stored `source_id`/`target_id` →
`delete` → `link` in the SAME stored direction. The re-link revives the original row —
same id, `created_at` preserved as first-assertion provenance, `updated_at` carrying the
act date. A direction-flipped `link` is a NEW assertion, never a reprice: the flip
addresses the other direction's natural key, so it creates (or revives) that key's row
while the deleted original stays deleted — one live edge after the sequence, not two.
Two live rows exist only once both directional natural keys have been linked live. For
symmetric relations the question does not arise: either endpoint order canonicalizes to
the same natural key, so a re-link in either order revives the same row. Callers must
not infer stored direction from adjacency output (`neighbors` echoes the traversal
origin, issue #1670); `get(edge_id)` is the supported direction read.

**Migration consequence.** The legality ruling strands no existing rows. Reciprocal
pairs in state-like relations stay as-is. Reciprocal pairs in order-like relations and
any surviving self-loop rows become curation-review candidates, resolved one by one with
ADR-014 verbs (typically: delete the direction that the pair's history shows was an
accidental flip, or supersede one side) — never mass-deleted. Measured on the reference
production store (2026-08-03), over **entity-endpoint edges**: for 5 of 14 non-symmetric
relations the enumeration's distinct edge-id count equals the `stats()` row count
exactly (including `precedes`, the largest at 9,379 rows); the other 9 relations carry
a residual of 163 edge rows total that entity-pattern matching cannot see. Sampled
diagnosis of the `contains` residual: edges whose endpoint entities are soft-deleted
(edges retained by design, view-filtered). For relations whose contract permits
note-substrate endpoints (`supersedes`, `supports`, `refutes` note→note; pack-extended
`contains`→note), live note-endpoint edges are a second cause the entity-pattern census
cannot see — inferred from the endpoint contract, not sampled. Note-endpoint reciprocal
pairs are therefore **not counted below**; the SQL queries that follow are
substrate-blind and settle them for store operators.

| Relation                  | Reciprocal pairs | Class               | Residual rows outside census |
| ------------------------- | ---------------- | ------------------- | ---------------------------- |
| `precedes`                | 66               | order-like — review | 0                            |
| `enables`                 | 15               | state-like — keep   | 16                           |
| `depends_on`              | 11               | state-like — keep   | 11                           |
| `extends`                 | 3                | order-like — review | 17                           |
| `instance_of`             | 2                | order-like — review | 0                            |
| `introduced_by`           | 2                | order-like — review | 37                           |
| `variant_of`              | 1                | order-like — review | 0                            |
| `supersedes`              | 1                | order-like — review | 56                           |
| `contains`                | 0                | order-like — review | 10                           |
| `part_of`                 | 0                | order-like — review | 5                            |
| `implements`              | 0                | order-like — review | 10                           |
| `supports`                | 0                | state-like — keep   | 1                            |
| `derived_from`, `refutes` | 0                | —                   | 0                            |

Total: 101 reciprocal pairs (75 order-like review candidates, 26 state-like keeps).
Self-loops: 1 (`instance_of`, predating the seam rejection) — curation candidate.

**Count queries.** For store operators with SQL access, read-only:

```sql
-- Reciprocal pairs per relation (each pair counted once per direction; halve)
SELECT e1.relation, COUNT(*) / 2 AS reciprocal_pairs
FROM graph_edges e1
JOIN graph_edges e2
  ON e2.namespace = e1.namespace
 AND e2.relation  = e1.relation
 AND e2.source_id = e1.target_id
 AND e2.target_id = e1.source_id
WHERE e1.deleted_at IS NULL AND e2.deleted_at IS NULL
  AND e1.source_id <> e1.target_id
  AND e1.relation NOT IN ('competes_with', 'composed_with')
GROUP BY e1.relation;

-- Self-loops per relation
SELECT relation, COUNT(*) AS self_loops
FROM graph_edges
WHERE deleted_at IS NULL AND source_id = target_id
GROUP BY relation;
```

On the MCP surface the same census is expressible with per-relation edge enumeration
(`query` GQL `MATCH (a)-[e:REL]->(b) RETURN a.id, b.id, e.id`, partitioned by id prefix
under the 500-row result cap) followed by client-side pairing — with two caveats.
First, completeness is checked against `stats()` per relation, but exact parity is only
reachable when every edge row has two live entity endpoints: `stats()` counts raw live
edge rows, while entity-pattern matching excludes edges with soft-deleted or
note-substrate endpoints, so a residual short of `stats()` is expected on relations
carrying such rows and must be diagnosed (fetch the residual edge ids, inspect their
endpoints) rather than assumed benign. Second, `list`-verb offset pagination
(2026-08-07 amendment, issue #1671) now gives every store's default list order a
deterministic `(created_at, id)` total order — equal-`created_at` rows tiebreak on `id`
in a fixed direction — so a sweep over a table with no concurrent writes returns each
row exactly once, no duplicates or misses. That total order does not make offset
pagination a sound enumeration substitute under concurrent change, though: offset counts
positions in a result set that can shift while the sweep is in flight, so a sweep that
spans a concurrent insert, delete, or sort-key update to the paged rows can still
duplicate or skip rows. For a census that must be sound under concurrent writes, use the
keyset-cursor path below, not offset pagination. The edge list surface also carries a
keyset cursor (`after`/`next_after`) that reads raw edge rows — substrate-blind, so it
covers the note-endpoint and soft-deleted-endpoint edges the entity pattern misses. Where a
deployment supports it end-to-end, per-relation cursor traversal (until the cursor is
exhausted) followed by client-side pairing is the preferred MCP census path. Measured
caveats on the reference deployment: traversal fails loudly on edges created before the
cursor's insertion-sequence ledger, and an empty-string starting cursor returned no
cursor envelope at all — verify the first page round-trips (rows plus `next_after`)
before relying on the cursor, and otherwise fall back to the SQL queries above as the
reliable substrate-blind census.

## Endpoint Validation

Every `(source_kind, relation, target_kind)` triple must be explicitly allowed. Unlisted
triples are rejected at write time. Pack endpoint rules (via `EDGE_RULES`) add rows to the
allowlist but cannot remove base rules.

> **Centralization note (2026-07-05)**: this base contract and every loaded pack's
> `EDGE_RULES` additions are composed and enforced at one site,
> `validate_edge_relation_endpoints` (`crates/khive-runtime/src/operations.rs`) — there is no
> duplicate per-verb endpoint check elsewhere in the runtime. This centralization shipped and
> is recorded as complete in [ADR-095](ADR-095-verb-surface-consolidation.md); it is not open
> for re-litigation. See also [ADR-017](ADR-017-pack-standard.md) §"Pack-extensible edge
> endpoints" for how packs declare their `EDGE_RULES` additions.

### Validation algorithm

```text
1. Resolve source and target substrate (Entity | Note | Edge | Event)
2. Apply substrate-level contract:
   - annotates: source must be Note, target may be any substrate UUID
   - supersedes, supports, refutes: same substrate (Note→Note or Entity→Entity)
   - all other base relations: Entity→Entity unless explicitly stated
3. If both endpoints are entities, resolve EntityKind for both
4. Check base allowlist
5. Check loaded pack EDGE_RULES
6. No matching rule → reject
```

### Base endpoint contract

#### Structure relations

| Source     | Relation      | Target     |
| ---------- | ------------- | ---------- |
| `Concept`  | `contains`    | `Concept`  |
| `Project`  | `contains`    | `Project`  |
| `Project`  | `contains`    | `Artifact` |
| `Org`      | `contains`    | `Project`  |
| `Org`      | `contains`    | `Service`  |
| `Concept`  | `part_of`     | `Concept`  |
| `Project`  | `part_of`     | `Project`  |
| `Project`  | `part_of`     | `Org`      |
| any entity | `instance_of` | `Concept`  |
| `Service`  | `instance_of` | `Project`  |

#### Derivation relations

| Source     | Relation        | Target     |
| ---------- | --------------- | ---------- |
| `Concept`  | `extends`       | `Concept`  |
| `Concept`  | `variant_of`    | `Concept`  |
| `Artifact` | `variant_of`    | `Artifact` |
| `Concept`  | `introduced_by` | `Document` |
| `Concept`  | `introduced_by` | `Person`   |
| `Artifact` | `introduced_by` | `Document` |
| `Document` | `introduced_by` | `Person`   |
| `Document` | `introduced_by` | `Org`      |
| `Concept`  | `introduced_by` | `Org`      |
| `Service`  | `introduced_by` | `Document` |
| `Concept`  | `supersedes`    | `Concept`  |
| `Document` | `supersedes`    | `Document` |
| `Artifact` | `supersedes`    | `Artifact` |
| `Service`  | `supersedes`    | `Service`  |
| `Dataset`  | `supersedes`    | `Dataset`  |

> **Amended 2026-07-08**: added `Document introduced_by Person`, `Document introduced_by Org`,
> and `Concept introduced_by Org`. `introduced_by` previously covered only how a _concept_ or
> _artifact_ was first described; it had no pair for a _document's own authorship_ (who wrote
> or published it) or for a _concept originating from an org_ rather than a paper or person
> (e.g. an architecture originated by a company). Direction is unchanged: source is the thing
> whose origin is being recorded, target is the origin.

> **Amended 2026-08-21 ([ADR-167](ADR-167-service-provenance-and-kind-classification.md))**:
> added `Service introduced_by Document`. A service could supersede another service and be
> contained by an org, but could not point at the specification or paper that introduced it;
> its origin was being recorded as prose in notes, invisible to lineage traversal. This is
> deliberately one pair: the measured refusals concern a service recording its own origin,
> and the wider forms (`Service introduced_by Person`/`Org`, any `derived_from` pair
> involving a service) were considered and rejected for lack of evidence. Direction is
> unchanged: source is the thing whose origin is being recorded, target is the origin.

#### Provenance relation

| Source     | Relation       | Target     |
| ---------- | -------------- | ---------- |
| `Artifact` | `derived_from` | `Dataset`  |
| `Artifact` | `derived_from` | `Document` |
| `Artifact` | `derived_from` | `Project`  |
| `Artifact` | `derived_from` | `Artifact` |
| `Document` | `derived_from` | `Document` |

> **Amended 2026-07-27**: added `Document derived_from Document` for publication provenance —
> a curated or filtered publication copy pointing at the canonical document it was produced
> from. Direction is unchanged: output → input, copy → canonical. See "Why the 2026-07-27
> provenance amendment?" in Rationale for the move-vs-copy boundary this pair depends on.

#### Temporal relation

| Source     | Relation   | Target     |
| ---------- | ---------- | ---------- |
| `Document` | `precedes` | `Document` |
| `Dataset`  | `precedes` | `Dataset`  |
| `Artifact` | `precedes` | `Artifact` |
| `Service`  | `precedes` | `Service`  |
| `Project`  | `precedes` | `Project`  |

Not allowed in base contract: `Concept→Concept`, `Person→Person`, `Org→Org` for `precedes`.
Those are better modeled with `extends`, `variant_of`, `supersedes`, or metadata.

#### Dependency relations

| Source     | Relation     | Target     |
| ---------- | ------------ | ---------- |
| `Project`  | `depends_on` | `Project`  |
| `Service`  | `depends_on` | `Project`  |
| `Service`  | `depends_on` | `Service`  |
| `Service`  | `depends_on` | `Artifact` |
| `Service`  | `depends_on` | `Dataset`  |
| `Artifact` | `depends_on` | `Project`  |
| `Artifact` | `depends_on` | `Service`  |
| `Document` | `depends_on` | `Document` |
| `Concept`  | `enables`    | `Concept`  |
| `Service`  | `enables`    | `Concept`  |
| `Dataset`  | `enables`    | `Concept`  |

> **Amended 2026-07-08**: added `Document depends_on Document` — a document's normative
> dependency on another document (e.g. a spec that requires the terminology or scope defined
> in a referenced RFC). Previously `depends_on` covered only project/service/artifact
> dependency chains; document-to-document normative dependency had no representable pair.

#### Implementation relation

| Source    | Relation     | Target    |
| --------- | ------------ | --------- |
| `Project` | `implements` | `Concept` |
| `Service` | `implements` | `Concept` |

#### Lateral relations

| Source    | Relation        | Target    |
| --------- | --------------- | --------- |
| `Concept` | `competes_with` | `Concept` |
| `Project` | `competes_with` | `Project` |
| `Service` | `competes_with` | `Service` |
| `Concept` | `composed_with` | `Concept` |
| `Project` | `composed_with` | `Project` |

#### Annotation relation

| Source | Relation    | Target             |
| ------ | ----------- | ------------------ |
| `Note` | `annotates` | any substrate UUID |

`annotates` is the only relation that crosses substrate kinds. Source is always a note.
Target may be any existing UUID (entity, note, event, edge) in the caller's namespace.

#### Epistemic relations (added by ADR-055)

`supports` and `refutes` are **same-substrate** (Note→Note or Entity→Entity), like
`supersedes`. They do **not** cross substrates — `annotates` remains the only relation that does.

Entity form (kind-restricted). The claim is a `concept`; evidence may be a concept, document,
dataset, or artifact:

| Source     | Relation   | Target    |
| ---------- | ---------- | --------- |
| `Concept`  | `supports` | `Concept` |
| `Document` | `supports` | `Concept` |
| `Dataset`  | `supports` | `Concept` |
| `Artifact` | `supports` | `Concept` |
| `Concept`  | `refutes`  | `Concept` |
| `Document` | `refutes`  | `Concept` |
| `Dataset`  | `refutes`  | `Concept` |
| `Artifact` | `refutes`  | `Concept` |

Note form (substrate-level, any note kind → any note kind): a finding-note `supports`/`refutes`
a hypothesis-note. Enforced at the substrate level like `supersedes`, not in the kind allowlist.

Event and edge endpoints are invalid for `supports`/`refutes`.

#### KG pack extensions (added v0.2.4)

The KG pack extends the base endpoint contract via `EDGE_RULES` to cover
person→org, person→project, and org→org relationships common in research KGs:

| Source   | Relation      | Target    | Added      |
| -------- | ------------- | --------- | ---------- |
| `Person` | `part_of`     | `Org`     | v0.2.4     |
| `Person` | `instance_of` | `Org`     | v0.2.4     |
| `Person` | `part_of`     | `Project` | unreleased |
| `Person` | `instance_of` | `Project` | unreleased |
| `Org`    | `depends_on`  | `Org`     | v0.2.4     |
| `Org`    | `enables`     | `Org`     | v0.2.4     |
| `Org`    | `contains`    | `Org`     | v0.2.4     |
| `Org`    | `part_of`     | `Org`     | v0.2.4     |
| `Org`    | `precedes`    | `Org`     | v0.2.4     |

These are additive — the base contract is unchanged. Semantics:

- `Person part_of Org` — a person is a member or employee of an org
- `Person instance_of Org` — a person represents or embodies an org (e.g. a founder)
- `Person part_of Project` — a person is a member or contributor of a project (issue #60);
  the same member-not-component semantic stretch accepted for `Person part_of Org` is extended
  here — a person is not a structural component of a project, but the closest base relation is
  `part_of`, mirroring the org row.
- `Person instance_of Project` — a person represents or embodies a project (e.g. a founder or
  maintainer), mirroring `Person instance_of Org`.
- `Org depends_on Org` — one org depends on another (e.g. subsidiary dependency)
- `Org enables Org` — one org enables another (e.g. incubator → startup)
- `Org contains Org` — org hierarchy (e.g. parent company contains subsidiary)
- `Org part_of Org` — subsidiary is part of parent (here it coincides with `contains`; the two
  remain distinct relations, not converses — see ADR-076)
- `Org precedes Org` — temporal ordering without replacement (predecessor org)

## Edge Metadata

`Edge.metadata` remains open JSON for relation-specific annotations. ADR-governed metadata
keys are validated at write time. Ungoverned keys are accepted but not part of the ontology
contract.

### `depends_on` governed metadata

`depends_on` requires a `dependency_kind` qualifier because the relation covers semantically
distinct dependency types:

```json
{
  "dependency_kind": "build",
  "optional": false
}
```

| `dependency_kind` | Meaning                                        | Typical endpoint pair        |
| ----------------- | ---------------------------------------------- | ---------------------------- |
| `build`           | Needed to build, compile, package              | `Project → Project`          |
| `runtime`         | Needed while executing or serving              | `Service → Service/Project`  |
| `data`            | Dataset/corpus dependency                      | `Service → Dataset`          |
| `artifact`        | Generated state dependency (checkpoint, index) | `Service → Artifact`         |
| `tooling`         | Required for generation or reproduction        | `Artifact → Project/Service` |
| `normative`       | Referenced document required to read/implement | `Document → Document`        |

`optional` is a separate boolean (default `false`), not a `dependency_kind` value.

**Runtime inference defaults**: if `dependency_kind` is omitted, the runtime infers from
endpoint kinds:

| Endpoint pair         | Default `dependency_kind` |
| --------------------- | ------------------------- |
| `Project → Project`   | `build`                   |
| `Service → Service`   | `runtime`                 |
| `Service → Dataset`   | `data`                    |
| `Service → Artifact`  | `artifact`                |
| `Artifact → Project`  | `tooling`                 |
| `Artifact → Service`  | `tooling`                 |
| `Document → Document` | `normative`               |

Unknown `dependency_kind` values are rejected. `dependency_kind` is only valid on
`depends_on` edges.

## Edge Density Rules

A sparse graph is a useless graph. Per-kind minimums (polish guidance, not write-time gates):

| Entity Kind  | Min Edges | Required / Preferred Relations                                                                                   |
| ------------ | --------: | ---------------------------------------------------------------------------------------------------------------- |
| **Concept**  |         4 | `instance_of` OR `extends`; `introduced_by` if document exists; `competes_with` if alternatives                  |
| **Document** |         2 | `introduced_by` connecting concepts                                                                              |
| **Dataset**  |         2 | `depends_on` from consumers; `enables` to outcomes                                                               |
| **Project**  |         3 | `contains`/`part_of`; `implements`; `depends_on`                                                                 |
| **Person**   |         1 | `introduced_by` from their work                                                                                  |
| **Org**      |         1 | `contains` to projects or services                                                                               |
| **Artifact** |         2 | `derived_from`; plus one of `instance_of`, `introduced_by`, `depends_on`, `supersedes`, `precedes`               |
| **Service**  |         2 | one identity anchor (`instance_of Project` OR `Org contains` OR `implements Concept`); plus one operational edge |

Density target: **5+ edges per entity average**. Below 3 → polish needed.

## Cascade Behavior

**Hard-delete cascades all incident edges synchronously** in the same SQLite transaction.
No dangling references.

**Soft-delete leaves edges in place.** Queries filter by `deleted_at IS NULL`.

For provenance/lineage-sensitive relations, hard-delete cascade emits a warning event:

| Relation               | Cascade behavior                                    |
| ---------------------- | --------------------------------------------------- |
| `derived_from`         | cascade edge; emit provenance-loss warning          |
| `supersedes`           | cascade edge; emit replacement-lineage-loss warning |
| `precedes`             | cascade edge; emit temporal-sequence-loss warning   |
| `supports` / `refutes` | cascade edge; emit evidential-link-loss warning     |
| `annotates`            | cascade as documented                               |
| others                 | cascade normally                                    |

Warnings use the existing `audit` event kind and target the hard-deleted record. The runtime
emits one warning per affected protected relation, in the same transaction as the delete and
edge cascade, with payload fields `severity="warning"`, `warning`, `deleted_id`, `relation`,
`edge_count`, and `edges`. `warning` is `provenance_loss`, `replacement_lineage_loss`,
`temporal_sequence_loss`, or `evidential_link_loss` as named by the table above; each `edges`
entry preserves the removed edge's id, namespace, endpoints, and prior `deleted_at` value.
Relations absent from the incident-edge set emit no warning.

No hard blocks on delete. If stronger provenance guarantees are needed later, add tombstones
or immutable lineage records in a separate ADR.

## Rationale

### Why closed (not open)?

Open ontologies fail in practice. Real-world KGs accumulate hundreds of near-synonym
relations, making queries impossible. The cost of "rejection at write time" is far lower
than "untangling synonyms at query time."

### Why 17 specifically?

The original 13 covered 6 query classes. The first expansion (→ 15) added two:

- **Provenance queries** ("what was this artifact generated from") need `derived_from`.
  Previously approximated by `depends_on` or `extends`, both semantically wrong.
- **Temporal queries** ("what came before this, without implying replacement") need
  `precedes`. Previously approximated by `supersedes`, which carries a replacement judgment.

The second expansion (→ 17, [ADR-055](ADR-055-epistemic-edge-relations.md)) adds the
**Epistemic** category:

- **Evidential queries** ("what is the evidence for and against claim X, and how strong") need
  `supports` and `refutes`. Previously approximated by `annotates`, which is polarity-blind and
  does not connect two entities. The relation choice carries polarity; the weight carries
  strength. This is the signal a confidence model consumes.

### How is the closed set calculable and auditable?

[ADR-076](ADR-076-relation-calculability-and-system-role.md) records why there is no
first-principles relation algebra whose closure uniquely derives these 17 relations: khive's
query classes are design inputs, not deductions. The calculable result is instead a repeatable
falsification audit. Each relation names a system role, and a proposed relation is tested against
seven cheaper encodings: converse (Cv), endpoint restriction (Er), attribute (At), polarity
(Po), fixed property chain (Ch), materialized reachability view (Mv), and typed sub-relation
(Sr).

The audit disposition for the current set is:

- Exactly the 15 base relations in this ADR are registered as the grandfathered
  `SurvivesAll` set. No base relation is treated as a converse, fixed composition, or
  materialized reachability view of another. In particular, `contains`/`part_of` do not collapse:
  housing or scope and constitution or membership can diverge. `depends_on`/`enables` also do not
  collapse: a hard requirement is not the converse of making an outcome possible.
- `supports`/`refutes` are the one algebraically collapsible pair. They fail Po because a single
  `assesses` relation plus a polarity value can answer the same queries. ADR-055 nevertheless
  keeps both as top-level relations under an explicit system-role exception so polarity remains
  visible to planners, indexes, federation, and the public API rather than residing in open
  metadata.
- The live base-plus-KG-pack endpoint-signature audit finds only the ratified
  `supports`/`refutes` collision. An identical signature is a signal to run Er analysis, not by
  itself proof of redundancy; relations with the same legal endpoint kinds can still answer
  different questions.

Accordingly, the audit does not replace the stored vocabulary with a smaller inferred generating
set. The only demonstrated algebraic collapse is retained by a declared system role, and khive
does not create converse, chained, or materialized derived edges automatically.

The decision is executable, not prose-only. The seven-eliminator harness and closed-set coverage
gate live in `crates/khive-types/tests/certificate/`; the live endpoint-signature tripwire lives
in `crates/khive-pack-kg/tests/certificate/endpoint_signatures.rs`. A new Tier-1 relation must
provide non-vacuous fixtures that defeat all seven eliminators, or record the eliminator it fails
and the explicit system role that justifies keeping it. A proposed Tier-2 native label (#293)
must clear the same irreducibility and system-role analysis before its separate storage mechanism
is designed; an honest mapping to an existing core relation does not qualify as a new label.

### Why the 2026-07-08 endpoint amendment?

The base contract did not distinguish "who first described this concept" from "who authored
this document." `introduced_by` covered concept/artifact origin but had no pair for a document
pointing at its own author or publisher. Knowledge graphs built over real research corpora
surface this gap immediately: a document entity for a paper, blog post, or standard needs an
authorship edge to the person or org that wrote it, independent of any concept the document
introduces.

The amendment adds three pairs to close it:

- **`Document introduced_by Person`** — a document authored by a person.
- **`Document introduced_by Org`** — a document authored or published by an org.
- **`Concept introduced_by Org`** — a concept originated by an org rather than a specific paper
  or person (e.g. an architecture or protocol originated by a company). This pattern recurs
  often enough in production knowledge graphs to warrant a first-class base pair rather than a
  per-consumer workaround.

Consumer verbs built over `introduced_by` follow the base contract. `knowledge.cite`
delegates to the same runtime link validation, so a concept may be cited to a document,
person, or org source.

It also adds one pair to `depends_on`:

- **`Document depends_on Document`** — a document's normative dependency on another document
  (a spec that requires terminology or scope defined in a referenced document). This is
  distinct from `precedes` (temporal ordering, no replacement judgment) and from `supersedes`
  (replacement) — `depends_on` here records that one document cannot be correctly read or
  implemented without the other.

None of these four pairs remove or narrow an existing rule; they are strictly additive to the
base contract, consistent with the "packs extend, never tighten" principle this ADR already
applies to pack-level `EDGE_RULES` ([ADR-017](ADR-017-pack-standard.md)).

### Why the 2026-07-27 provenance amendment?

`derived_from` covered artifact provenance (checkpoints, indexes, exports) but had no pair
for document-to-document production. Real corpora surface this as soon as a document exists
in more than one authoritative form: a public reference copy produced from an internal
canonical by a filtering sync, a redacted release of a full report, an abridged or translated
edition. These copies are not the same document at a second address — their content differs
by construction and they serve a different audience and authority — and no existing relation
fits:

- `supersedes` is wrong: the canonical remains authoritative; nothing is replaced.
- `precedes` is wrong: the relationship is production, not chronology.
- `variant_of` is wrong twice over: its endpoints are concept/artifact, and it records a
  modified version relative to an original, not material production from a source.
- `annotates` is substrate-wrong: the copy is a document entity, not a note.

The amendment adds one pair — `Document derived_from Document` — with the standard
`derived_from` direction (output → input: the copy points at the canonical).

Two modelling rules keep the pair from being misused:

1. **Move vs copy.** Relocation of a document (a repository migration, a path change) is
   location history on ONE entity, recorded in properties. A `derived_from` edge between two
   document entities asserts that two documents exist with a production relationship between
   them. If only one content-bearing thing exists, there is nothing to link.
2. **Mint at publication.** The copy's entity is created when the copy is produced (or first
   modelled), pointing at the already-existing canonical entity. Canonical records are never
   split retroactively to manufacture a second endpoint.

Weight and annotation follow existing rules: a definitional production relationship carries
weight 1.0, and an annotating note on the edge records what transformation produced the copy
(what was filtered, when, for which audience) — two `derived_from` edges are otherwise
indistinguishable.

This pair is strictly additive; no existing rule is removed or narrowed.

### Why 9 categories?

Each category serves a distinct query class. Single- or two-relation categories (Implementation,
Annotation, Provenance, Temporal, Epistemic) are justified because the relation(s) within each
answer a question no other category covers. Category count is driven by query semantics, not by
balancing relation counts.

### Why no auto-inverse?

Auto-inverses double the graph size for redundant information and create maintenance traps.
Direction-aware traversal (`direction=in`) handles logical inverses. `precedes` and
`derived_from` follow the same no-auto-inverse rule as all other relations.

### Why governed metadata only for `depends_on`?

`depends_on` is the one relation where semantic overloading materially harms query utility
(build vs runtime vs data vs artifact dependencies are different traversal questions). Other
relations carry their primary meaning without metadata qualifiers. Full per-relation metadata
schemas would overfit too early.

## Implementation

```rust
pub enum EdgeCategory {
    Structure,
    Derivation,
    Provenance,
    Temporal,
    Dependency,
    Implementation,
    Lateral,
    Annotation,
    Epistemic,
}

pub enum EdgeRelation {
    // Structure
    Contains, PartOf, InstanceOf,
    // Derivation
    Extends, VariantOf, IntroducedBy, Supersedes,
    // Provenance
    DerivedFrom,
    // Temporal
    Precedes,
    // Dependency
    DependsOn, Enables,
    // Implementation
    Implements,
    // Lateral
    CompetesWith, ComposedWith,
    // Annotation
    Annotates,
    // Epistemic
    Supports, Refutes,
}
```

`EdgeRelation` is defined once in `khive-types/src/edge.rs`. Stored as SQL TEXT column.
Serialized via `Display` (snake_case), deserialized via `FromStr`. Unknown relation strings
are rejected with the valid list in the error message.

Endpoint validation lives in `khive-runtime` (not the type layer). The base contract tables
above are the default. Packs extend via `EDGE_RULES` (additive only, cannot tighten).
