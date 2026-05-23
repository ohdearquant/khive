# ADR-001: Entity Kind Taxonomy

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

khive is a research knowledge graph platform. Entities are the nodes in the graph — papers,
algorithms, people, tools. Until now, `Entity.entity_type` was a free-form `String`, which:

1. Provided no compile-time guarantees about valid types.
2. Led to inconsistent naming (e.g., "paper" vs "article" vs "publication").
3. Made it impossible to enforce per-kind property schemas.
4. Gave agents no clear classification guidance — any string was valid.

With the scope locked to research KG, we need a proper taxonomy. The tension: too granular and
agents misclassify (wasting graph quality); too generic and the kind field has no discriminative
power.

The 13-relation edge ontology (ADR-002) is already defined and closed:

| Category       | Relations                                              |
| -------------- | ------------------------------------------------------ |
| Structure      | `contains`, `part_of`, `instance_of`                   |
| Derivation     | `extends`, `variant_of`, `introduced_by`, `supersedes` |
| Dependency     | `depends_on`, `enables`                                |
| Implementation | `implements`                                           |
| Lateral        | `competes_with`, `composed_with`                       |
| Annotation     | `annotates`                                            |

The kind taxonomy must complement — not duplicate — edge semantics.

## Decision

6 entity kinds, defined as `EntityKind` enum in `khive-types`:

| Kind         | What it covers                                                                | Unique properties                                         |
| ------------ | ----------------------------------------------------------------------------- | --------------------------------------------------------- |
| **Concept**  | Algorithms, techniques, architectures, theories, models, research gaps, ideas | `type` (algorithm/technique/model/architecture), `domain` |
| **Document** | Papers, preprints, technical reports, blog posts, books                       | `title`, `authors`, `year`, `venue`, `doi`, `url`         |
| **Dataset**  | Benchmarks, corpora, evaluation sets                                          | `task`, `size`, `metrics`, `license`                      |
| **Project**  | Codebases, libraries, tools, frameworks                                       | `language`, `repo`, `license`                             |
| **Person**   | Researchers, engineers, authors                                               | `affiliation`, `orcid`                                    |
| **Org**      | Labs, companies, institutions                                                 | `url`, `type` (academic/industry)                         |

`Concept` is the default/residual bucket. Agents should only escalate to the other 5 kinds when
signals are unambiguous.

The `entity_type: String` field is replaced by `kind: EntityKind`.

## Rationale

### Why separate Document from Concept?

This is the highest-ROI separation:

1. **Agents classify reliably** — papers have unambiguous signals (DOI, authors, year, venue,
   abstract).
2. **Cleans up `introduced_by` semantics** — the edge goes from `Concept → Document` or
   `Concept → Person`, not `Concept → Concept`. This makes the edge queryable without inspecting
   node properties.
3. **Enables the most common query pattern** — "find all papers about topic X" is a kind filter, not
   a property filter.
4. **Epistemically stable** — once published, a paper doesn't change kind. No reclassification
   churn.

### Why separate Dataset?

1. **Benchmark queries are primary** — "what datasets evaluate technique X" is a core research KG
   query.
2. **Unique property schema** — task type, size, metrics, splits don't fit the concept schema.
3. **Agents classify reliably** — a collection of examples with a defined task is unambiguously a
   dataset.

### Why NOT separate Model?

Models are concepts with `instance_of` an architecture, `introduced_by` a paper. The graph edges
already express everything interesting about a model. Separating Model would:

- Create a rapidly-growing kind (GPT-4/4o/4o-mini/4.1...) without new query power.
- Add classification burden — "is GPT-4 a model or an architecture?" depends on context.
- Not enable new edge types (the ontology is closed at 13 relations).

Use `properties.type = "model"` for filtering when needed.

### Why NOT separate Algorithm from Technique?

"Is LoRA an algorithm or a technique?" — ask 10 ML researchers, get 10 answers. Agent
misclassification rate would be 20-30%. The distinction doesn't enable useful queries with the
existing edge ontology. Both have identical edge patterns and identical property schemas.

Keep both in `Concept`, use `properties.type` for finer-grained filtering.

## Alternatives Considered

| Alternative                             | Pros                                | Cons                                                                                      | Why rejected                             |
| --------------------------------------- | ----------------------------------- | ----------------------------------------------------------------------------------------- | ---------------------------------------- |
| 4 kinds (concept, project, person, org) | Minimum classification errors       | "Show all papers" requires property filter; `introduced_by` semantics muddy               | Insufficient query power for research KG |
| 7 kinds (+ model)                       | Models queryable by kind            | Model explosion, classification ambiguity (model vs architecture?), no new edge semantics | Cost exceeds benefit                     |
| 8 kinds (+ algorithm, technique)        | Fine-grained concept discrimination | 20-30% misclassification rate, identical edge patterns, no useful queries enabled         | The split doesn't pay for itself         |
| Free-form string (status quo)           | Maximum flexibility                 | Inconsistent naming, no compile-time guarantees, no agent guidance                        | Failed in practice                       |

## Consequences

### Positive

- Type-safe entity classification at compile time.
- Clean `introduced_by` edge semantics (concept → document/person).
- Agents have clear, reliable classification heuristics.
- "Show all papers/datasets/projects" are clean kind-level queries.

### Negative

- Adding a new kind requires a code change (new enum variant). Mitigated: the research domain has
  stable entity types; this is unlikely to be needed often.
- `Concept` bucket is broad. Mitigated: `properties.type` provides finer filtering without the
  classification error cost.

### Neutral

- When importing data from another KG that uses `kind: "concept"` with `properties.type: "paper"`,
  prefer `kind: "document"` so the closed taxonomy stays clean.

## Implementation

- `crates/khive-types/src/entity.rs`: `EntityKind` enum with 6 variants, `Entity.kind: EntityKind`
  field.
- Default is `EntityKind::Concept`.
- Serde: `#[serde(rename_all = "snake_case")]` for JSON interop.
- `Display` and `name()` for string conversion.
- Agent classification heuristics should be documented in `AGENTS.md`.

## Agent Classification Heuristics

For agent prompts and documentation:

- **Document**: Has a title, authors, year. Was published in a venue or on arxiv. Use for preprints
  too.
- **Dataset**: Is a downloadable collection of examples. Has a defined task type and size.
  Benchmarks are datasets.
- **Project**: Has a codebase, repository, or package listing. Has a programming language.
- **Person**: A human researcher, engineer, or author.
- **Org**: A lab, company, institution, or research collective.
- **Concept**: Everything else. When in doubt, use Concept. Use `properties.type` for finer
  distinctions (algorithm, technique, model, architecture, theory, research_gap).

## References

- Edge ontology: ADR-002 (13 canonical relations)
- Implementation: `crates/khive-types/src/entity.rs`
