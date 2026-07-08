# ADR-002: Closed Edge Ontology

**Status**: accepted\
**Date**: 2026-05-22\
**Authors**: khive maintainers
**Amended by**: [ADR-076](ADR-076-relation-calculability-and-system-role.md) — `part_of` is a
distinct relation, not the "inverse of `contains`"; the two coincide in some domains and diverge
in others, and neither is derived from the other.
**Amended 2026-07-08**: base endpoint contract gains four pairs — three provenance
(`Document introduced_by Person`, `Document introduced_by Org`, `Concept introduced_by Org`)
and one dependency (`Document depends_on Document`) — closing a gap where a document's own
authorship and a document's normative dependency on another document had no representable
edge. See "Base endpoint contract" below and "Why the 2026-07-08 endpoint amendment?" in
Rationale.

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

| Relation        | Direction                              | When                                                                                       |
| --------------- | -------------------------------------- | ------------------------------------------------------------------------------------------ |
| `extends`       | child → parent                         | Builds on, generalizes (FlashAttention-2 → FlashAttention)                                 |
| `variant_of`    | variant → original                     | Modified version (QLoRA → LoRA)                                                            |
| `introduced_by` | concept/document → document/person/org | First described in (LoRA → Hu et al. 2021); document authorship (paper → author/publisher) |
| `supersedes`    | new → old                              | Replaces entirely; old stops being authoritative                                           |

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
```

`derived_from` is for material/generative provenance. It is NOT for intellectual inspiration
(use `extends` or `introduced_by`) or dependency (use `depends_on`).

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

#### Provenance relation

| Source     | Relation       | Target     |
| ---------- | -------------- | ---------- |
| `Artifact` | `derived_from` | `Dataset`  |
| `Artifact` | `derived_from` | `Document` |
| `Artifact` | `derived_from` | `Project`  |
| `Artifact` | `derived_from` | `Artifact` |

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

| Endpoint pair        | Default `dependency_kind` |
| -------------------- | ------------------------- |
| `Project → Project`  | `build`                   |
| `Service → Service`  | `runtime`                 |
| `Service → Dataset`  | `data`                    |
| `Service → Artifact` | `artifact`                |
| `Artifact → Project` | `tooling`                 |
| `Artifact → Service` | `tooling`                 |

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

Consumer verbs built over `introduced_by` may accept a narrower source set than the full base
contract. `knowledge.cite` intentionally remains a concept-to-document/person convenience
wrapper; edges to org sources (or document authorship edges) use `link` directly.

It also adds one pair to `depends_on`:

- **`Document depends_on Document`** — a document's normative dependency on another document
  (a spec that requires terminology or scope defined in a referenced document). This is
  distinct from `precedes` (temporal ordering, no replacement judgment) and from `supersedes`
  (replacement) — `depends_on` here records that one document cannot be correctly read or
  implemented without the other.

None of these four pairs remove or narrow an existing rule; they are strictly additive to the
base contract, consistent with the "packs extend, never tighten" principle this ADR already
applies to pack-level `EDGE_RULES` ([ADR-017](ADR-017-pack-standard.md)).

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
