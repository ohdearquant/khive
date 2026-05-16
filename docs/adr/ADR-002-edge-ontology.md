# ADR-002: Closed Edge Ontology (13 Canonical Relations)

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

A knowledge graph is only useful if its edges have consistent semantics. Allowing free-form relation
strings ("uses", "related_to", "contains_module", "loaded_by") leads to:

1. Synonym pollution — `uses` vs `requires` vs `depends_on` all mean the same thing.
2. Ambiguity — `related_to` carries no semantic information.
3. Query brittleness — "find all dependencies of X" must enumerate dozens of synonyms.
4. Agent drift — different agents invent different vocabularies.

A closed ontology — a fixed set of allowed relations — solves these by forcing every edge into a
canonical bucket.

## Decision

**13 canonical relations, grouped into 6 categories. No others.**

### Category 1: Structure (how things compose)

| Relation      | Direction          | When                                             |
| ------------- | ------------------ | ------------------------------------------------ |
| `contains`    | parent → child     | Crate contains module, system contains component |
| `part_of`     | child → parent     | Inverse of `contains`                            |
| `instance_of` | specific → general | One is a case of the other (GPT-4 → Transformer) |

### Category 2: Derivation (intellectual lineage)

| Relation        | Direction                 | When                                                       |
| --------------- | ------------------------- | ---------------------------------------------------------- |
| `extends`       | child → parent            | Builds on, generalizes (FlashAttention-2 → FlashAttention) |
| `variant_of`    | variant → original        | Modified version (QLoRA → LoRA)                            |
| `introduced_by` | concept → document/person | First described in (LoRA → Hu et al. 2021)                 |
| `supersedes`    | new → old                 | Replaces entirely; same-substrate (rare)                   |

`supersedes` is the **same-substrate** replacement relation: a note supersedes a note, or an
entity supersedes an entity — it never crosses substrate kinds (so it does not contradict
"`annotates` is the only relation that crosses substrate kinds", stated below). Note supersession
(ADR-019) and entity supersession use the identical `supersedes` edge and mechanism
(`new --supersedes--> old`). Both endpoints must resolve to the **same** substrate kind in the
caller's namespace; `note→entity` and `entity→note` are rejected, as is any endpoint that is not a
note or entity (event, edge).

### Category 3: Dependency (runtime/build needs)

| Relation     | Direction              | When                                              |
| ------------ | ---------------------- | ------------------------------------------------- |
| `depends_on` | consumer → dependency  | Hard requirement                                  |
| `enables`    | prerequisite → outcome | Makes possible (Sinkhorn → Wasserstein attention) |

### Category 4: Implementation (code ↔ concept)

| Relation     | Direction      | When                                                |
| ------------ | -------------- | --------------------------------------------------- |
| `implements` | code → concept | Code realizes algorithm (SinkhornSolver → Sinkhorn) |

### Category 5: Lateral (peer relationships)

| Relation        | Direction | When                                             |
| --------------- | --------- | ------------------------------------------------ |
| `competes_with` | A ↔ B     | Alternative approaches (softmax attn ↔ OT attn)  |
| `composed_with` | A ↔ B     | Used together in a system (GDN ↔ GQA in Qwen3.5) |

### Category 6: Annotation (commentary about other substrate items)

| Relation    | Direction       | When                                                                                                                              |
| ----------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `annotates` | note → anything | A note observes/comments on an entity, edge, event, or other note. The source is always a note; the target is any substrate UUID. |

`annotates` is what makes notes first-class graph nodes. A `create(kind="note", annotates=[X], ...)`
call that captures an observation about entity X is conceptually the same operation as creating a
note plus an edge `note --annotates--> X`. The runtime materializes both. `search` + graph
traversal then unify: `neighbors(entity_id, relations=[annotates], direction=in)` returns every note
that comments on that entity.

The target can be any substrate UUID — entity, edge, event, or another note (e.g., one note cites
another's observation). This is the only relation in the ontology that crosses substrate kinds.

### Rules

- Relations not in this list are forbidden.
- If a relationship doesn't fit, it's either an entity property or it doesn't belong in the graph.
- Inverse relations are NOT created automatically. Use `part_of` explicitly if you need the inverse
  of `contains`.
- Edge weight: `1.0` = definitional, `0.7-0.9` = strong, `0.4-0.6` = plausible, `<0.4` =
  speculative.

## Rationale

### Why closed (not open)?

Open ontologies sound flexible but fail in practice. Real-world KGs accumulate hundreds of
near-synonym relations, making queries impossible to write. The cost of "rejection at write time" is
far lower than the cost of "untangling synonyms at query time."

### Why these 13 specifically?

The set was derived from observing what queries actually need to be answered in a research KG:

- **Structural queries** ("what's inside this system") need `contains`/`part_of`/`instance_of`.
- **Lineage queries** ("where did this idea come from") need
  `extends`/`variant_of`/`introduced_by`/`supersedes`.
- **Reachability queries** ("what does X depend on") need `depends_on`/`enables`.
- **Code ↔ concept mapping** ("what implements algorithm Y") needs `implements`.
- **Alternative comparison** ("what competes with this") needs `competes_with`/`composed_with`.
- **Cross-substrate annotation** ("what notes talk about this entity") needs `annotates`.

Every category serves a distinct query class. Removing any category loses a class of queries.

### Why no inverse auto-creation?

Auto-inverses double the graph size for redundant information. They also create maintenance traps:
if you delete `A contains B` does the `B part_of A` edge get deleted too? Better to require explicit
inverse edges when needed — and most queries can use direction-agnostic traversal.

## Alternatives Considered

| Alternative                  | Pros                   | Cons                                        | Why rejected                     |
| ---------------------------- | ---------------------- | ------------------------------------------- | -------------------------------- |
| Open vocabulary (any string) | Maximum flexibility    | Synonym pollution, query brittleness        | Failed in every KG that tried it |
| RDF/OWL-style URIs           | Standard, federated    | Massive complexity for single-instance KGs  | Overkill for our use case        |
| Smaller set (5 relations)    | Even simpler           | Loses derivation/dependency distinction     | Insufficient query power         |
| Larger set (~30 relations)   | Fine-grained semantics | Most relations rarely used, agent confusion | Cost > benefit                   |

## Consequences

### Positive

- Queries are writable without enumerating synonyms.
- Agents have a finite menu — easier to classify edges correctly.
- The graph schema is stable; no relation explosion over time.
- Edge meaning is queryable without inspecting properties.

### Negative

- Some real-world relationships don't fit perfectly. Mitigated: use the closest category or store as
  entity property.
- Inverse edges require explicit creation. Mitigated: traversal queries can be direction-agnostic.

### Neutral

- Adding a new relation requires an ADR (changing the closed set is a design decision, not a code
  change).

## Edge Density Rules

A sparse graph is a useless graph. Per-kind minimums:

| Entity Kind | Min Edges | Required Relations                                                                                  |
| ----------- | --------- | --------------------------------------------------------------------------------------------------- |
| Concept     | 4         | `instance_of` OR `extends`, `introduced_by` (if document exists), `competes_with` (if alternatives) |
| Document    | 2         | `introduced_by` connecting concepts                                                                 |
| Project     | 3         | `contains`/`part_of`, `implements`, `depends_on`                                                    |
| Person      | 1         | `introduced_by` from their work                                                                     |
| Dataset     | 2         | `depends_on` from features needing it, `enables` to outcomes                                        |

Density target: **5+ edges per entity average**. Below 3 → polish needed.

## Implementation

The relation is stored as `Edge.relation: EdgeRelation` in `khive-storage` (migrated from `String`
by ADR-021). SQL TEXT column for persistence; serialized via `Display`, deserialized via `FromStr`.
Validation at compile time — invalid relations cannot be constructed. Agents must consult this ADR
before creating edges.

## References

- ADR-001: Entity Kind Taxonomy (defines node kinds that edges connect)
- ADR-021: EdgeRelation Enum (closes the type at compile time; the 13th relation `annotates` was
  added when notes became first-class graph nodes)
- `crates/khive-types/src/edge.rs`: `EdgeRelation`, `EdgeCategory` enum implementations
- `crates/khive-storage/src/types.rs`: `Edge`, `EdgeFilter` types
- Edge metadata schema: open JSON in `Edge.metadata` for relation-specific annotations
