# Knowledge Graph Modeling

This guide covers how to think about modeling in khive — when to use each entity
kind, which edge relation fits, when something belongs as a note versus an
entity, and common modeling patterns for research work.

## The two substrates

khive has two kinds of records:

- **Entities** are things in the world: algorithms, papers, people, projects,
  datasets, organizations, binaries, APIs. They are graph nodes with typed edges
  between them.
- **Notes** are your observations about the world: what you noticed, concluded,
  decided, asked, or cited. They are temporal records with salience, optional
  decay, and can annotate entities via `annotates` edges.

The rule of thumb: if it has a name and exists independently of your session,
it is an entity. If it is something you thought or recorded during a session,
it is a note.

## Entity kinds

khive has 8 entity kinds. This is a closed set — you cannot add new kinds
without an ADR.

### concept

Algorithms, techniques, architectures, theories, models. This is the most
common kind and the default.

```
create(kind="entity", entity_kind="concept", name="LoRA",
       description="Low-Rank Adaptation of LLMs",
       properties={domain: "fine-tuning", type: "technique", year: 2021})
```

Use `concept` for anything that is an idea, method, or approach. Use
`properties.type` for finer classification: `algorithm`, `technique`,
`architecture`, `model`, `theory`.

### document

Papers, preprints, technical reports, blog posts, books.

```
create(kind="entity", entity_kind="document",
       name="Attention Is All You Need",
       properties={authors: "Vaswani et al.", year: 2017,
                   source: "arxiv:1706.03762"})
```

Name the entity with its short title. Put full title, authors, year, and
citation pointer in `properties`.

### dataset

Benchmarks, corpora, evaluation sets.

```
create(kind="entity", entity_kind="dataset", name="MMLU",
       description="Massive Multitask Language Understanding benchmark",
       properties={type: "benchmark", year: 2021})
```

### project

Codebases, libraries, tools, frameworks.

```
create(kind="entity", entity_kind="project", name="lattice-inference",
       description="Pure-Rust transformer inference engine",
       properties={status: "implemented"})
```

### person

Researchers, engineers, authors.

```
create(kind="entity", entity_kind="person", name="Edward Hu",
       properties={affiliation: "Microsoft"})
```

### org

Labs, companies, institutions.

```
create(kind="entity", entity_kind="org", name="Anthropic",
       description="AI safety company")
```

### artifact

Binaries, model checkpoints, Docker images, packages.

```
create(kind="entity", entity_kind="artifact", name="Llama-3-70B",
       properties={type: "checkpoint", source: "meta-llama"})
```

### service

APIs, hosted endpoints, SaaS products.

```
create(kind="entity", entity_kind="service", name="OpenAI API",
       properties={type: "api"})
```

## Note kinds

khive has 5 base note kinds (also a closed set):

| Kind          | What it records        | Example                                                             |
| ------------- | ---------------------- | ------------------------------------------------------------------- |
| `observation` | An empirical capture   | "FlashAttention reduces memory from O(N^2) to O(N)"                 |
| `insight`     | A synthetic conclusion | "Tiling is the key technique across all IO-aware attention methods" |
| `question`    | An open inquiry        | "Does FlashAttention-3 support GQA natively?"                       |
| `decision`    | A committed choice     | "We will use FlashAttention-2 for the inference engine"             |
| `reference`   | An external pointer    | "See arxiv:2205.14135 Section 3.2 for the tiling algorithm"         |

`observation` is the default if you omit `note_kind`.

Packs add their own note kinds: `task` (GTD pack), `memory` (Memory pack),
`message` (Comm pack), `scheduled_event` (Schedule pack). These are created
through their respective pack verbs, not through `create(kind="note")`.

## Edge relations

khive has 15 edge relations. This is a closed set enforced at compile time.

### When to use each relation

**Structure** — parent/child and classification:

| Relation      | Direction           | When to use                                            |
| ------------- | ------------------- | ------------------------------------------------------ |
| `contains`    | parent to child     | A system contains a module. An org contains a project. |
| `part_of`     | child to parent     | Inverse of contains. A module is part of a system.     |
| `instance_of` | specific to general | GQA is an instance of multi-query attention.           |

**Derivation** — how ideas build on each other:

| Relation        | Direction           | When to use                                   |
| --------------- | ------------------- | --------------------------------------------- |
| `extends`       | child to parent     | FlashAttention-2 extends FlashAttention.      |
| `variant_of`    | variant to original | QLoRA is a variant of LoRA.                   |
| `introduced_by` | concept to source   | LoRA was introduced by the LoRA paper.        |
| `supersedes`    | new to old          | FlashAttention-3 supersedes FlashAttention-2. |

**Provenance** — where things come from:

| Relation       | Direction       | When to use                                |
| -------------- | --------------- | ------------------------------------------ |
| `derived_from` | output to input | A model checkpoint derived from a dataset. |

**Temporal** — ordering:

| Relation   | Direction        | When to use                           |
| ---------- | ---------------- | ------------------------------------- |
| `precedes` | earlier to later | Paper A was published before Paper B. |

**Dependency** — runtime/build relationships:

| Relation     | Direction               | When to use                                       |
| ------------ | ----------------------- | ------------------------------------------------- |
| `depends_on` | consumer to dependency  | Project A depends on Project B at runtime.        |
| `enables`    | prerequisite to outcome | BPE tokenization enables subword-level attention. |

**Implementation** — code realizes concept:

| Relation     | Direction       | When to use                                  |
| ------------ | --------------- | -------------------------------------------- |
| `implements` | code to concept | lattice-inference implements FlashAttention. |

**Lateral** — peer relationships:

| Relation        | Direction        | When to use                                          |
| --------------- | ---------------- | ---------------------------------------------------- |
| `competes_with` | either direction | LoRA competes with full fine-tuning.                 |
| `composed_with` | either direction | FlashAttention composed with GQA in a serving stack. |

**Annotation** — notes observing entities:

| Relation    | Direction        | When to use                                                 |
| ----------- | ---------------- | ----------------------------------------------------------- |
| `annotates` | note to anything | An observation about a concept, a decision about a project. |

### Edge endpoint rules

Not every `(source_kind, relation, target_kind)` triple is valid. The base
contract in ADR-002 defines which entity kinds can appear as source and target
for each relation. Key rules:

- `annotates` is the only cross-substrate relation. Source must be a note;
  target can be anything (entity, note, edge, event).
- `supersedes` is same-substrate only: entity to entity, or note to note.
- All other 13 relations require entity-to-entity endpoints.
- `competes_with` and `composed_with` are symmetric — the system canonicalizes
  direction internally.

Packs can add endpoint pairs through the `EDGE_RULES` mechanism (ADR-017). The
KG pack adds person-to-org and org-to-org pairs. The GTD pack allows task-to-task
`depends_on` edges. These are additive — packs cannot tighten the base contract.

### Why a closed ontology

A sparse, fixed set of relations keeps the graph queryable. Ad-hoc relations
like `uses`, `related_to`, or `loaded_by` fragment the graph and make traversal
meaningless. If your relationship does not fit one of the 15, it is probably a
property on the entity rather than an edge.

## Modeling patterns

### Research papers

A paper typically produces: one `document` entity (the paper itself), one or
more `concept` entities (the ideas it introduces), and `introduced_by` edges
from concepts to the paper.

```
create(kind="entity", entity_kind="document", name="LoRA Paper",
       properties={title: "LoRA: Low-Rank Adaptation of Large Language Models",
                   authors: "Hu et al.", year: 2021, source: "arxiv:2106.09685"})

create(kind="entity", entity_kind="concept", name="LoRA",
       properties={domain: "fine-tuning", type: "technique"})

link(source_id="<lora_id>", target_id="<paper_id>", relation="introduced_by")
```

For citation chains between papers, use `precedes` (temporal ordering):

```
link(source_id="<earlier_paper>", target_id="<later_paper>", relation="precedes")
```

### Software projects

Model a project with `contains` for internal structure, `implements` for the
concepts it realizes, and `depends_on` for external dependencies:

```
create(kind="entity", entity_kind="project", name="lattice-inference",
       properties={status: "implemented"})

link(source_id="<lattice_id>", target_id="<flash_id>", relation="implements")
link(source_id="<lattice_id>", target_id="<tokio_id>", relation="depends_on")
```

### People and organizations

```
create(kind="entity", entity_kind="person", name="Tri Dao")
create(kind="entity", entity_kind="org", name="Princeton")

link(source_id="<person_id>", target_id="<org_id>", relation="part_of")
```

### Decision records

Use `decision` notes that annotate the entities they concern:

```
create(kind="note", note_kind="decision",
       content="We will use FlashAttention-2 over vanilla attention because memory reduction is critical for 70B inference",
       annotates=["<flash2_id>", "<project_id>"])
```

### Temporal chains

For versioned artifacts or sequential papers:

```
link(source_id="<flash1_id>", target_id="<flash2_id>", relation="precedes")
link(source_id="<flash2_id>", target_id="<flash3_id>", relation="precedes")
link(source_id="<flash3_id>", target_id="<flash2_id>", relation="supersedes")
```

## Anti-patterns

| Pattern                        | Problem                                                                                         | Fix                                                                      |
| ------------------------------ | ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| Storing findings only as notes | Notes are temporal context; entities are structural. A concept worth naming deserves an entity. | Create the entity, then annotate it with notes.                          |
| Creating duplicate entities    | Fragments the graph, splits edges.                                                              | Always `search` before `create`. If found, `link` to it.                 |
| Using ad-hoc relation names    | `link(relation="uses")` will be rejected.                                                       | Map to the 15 closed relations. If none fit, use a property.             |
| Reversed `introduced_by`       | `paper → concept` is wrong.                                                                     | Direction is `concept → paper` (the paper introduces the concept).       |
| Over-noting                    | 20 observations but zero entities.                                                              | Extract the structural content into entities first.                      |
| Under-linking                  | Entities with 0-1 edges are orphans.                                                            | Target 5+ edges per entity. Below 3 means the entity needs more context. |
| Version numbers in names       | "LoRA v2" instead of "QLoRA".                                                                   | Version info goes in properties. Names are canonical short forms.        |

## Edge density

Sparse graphs are useless for traversal. Target minimums:

| Entity kind         | Min edges | What to link                                                                                 |
| ------------------- | --------- | -------------------------------------------------------------------------------------------- |
| concept (algorithm) | 4         | `extends` or `instance_of` (parent), `introduced_by` (paper), `competes_with` (alternatives) |
| concept (paper)     | 2         | `introduced_by` edges from concepts it introduced                                            |
| project             | 3         | `implements` (concepts), `depends_on` (deps), `contains`/`part_of` (structure)               |
| person              | 1         | `introduced_by` edges from their work                                                        |

Overall target: 5+ edges per entity average. Check with `stats()` — if
`total_edges / total_entities` is below 4, the graph needs polish.

## See also

- [Prompt Cookbook](prompt-cookbook.md) — concrete verb patterns for all the
  operations described here
- [Search and Retrieval](search.md) — how to find things in the graph
- [AGENTS.md](../../AGENTS.md) — the full agent reference with GQL/SPARQL
  examples
