# khive — Agent Usage Guide

This file is for AI agents (and the humans configuring them) using khive as the research runtime.

khive gives your agent three things:

1. **A knowledge graph** — typed entities + edges you build as you work
2. **Notes** — observations, insights, questions, decisions, references that persist across sessions
3. **Pattern matching queries** — GQL/SPARQL traverse over the graph

If you're working on khive itself (writing code in this repo), see `CLAUDE.md` instead.

---

## Core verbs

All verbs are available via MCP ([ADR-023](docs/adr/ADR-023-verb-consolidated-mcp-surface.md)).

| Verb       | What it does                                       | When to use                                              |
| ---------- | -------------------------------------------------- | -------------------------------------------------------- |
| `create`   | Add an entity or note                              | New concept, paper, observation, decision worth tracking |
| `get`      | Fetch a record by ID                               | When you have a UUID and need the full record            |
| `search`   | Text + semantic search over entities or notes      | Finding things by content similarity                     |
| `list`     | Structured filtering (by kind, tags, etc.)         | Browsing a category or namespace                         |
| `update`   | Patch properties, tags, or content                 | Correcting or enriching an existing record               |
| `delete`   | Soft-delete (or hard-delete) a record              | Removing stale or incorrect data                         |
| `link`     | Connect two nodes with a typed relation            | When relationships emerge from research                  |
| `traverse` | Multi-hop graph query (GQL or SPARQL)              | Structural context — lineages, paths, clusters           |
| `neighbors`| Immediate neighbors of a node                      | "What connects to this entity?"                          |
| `merge`    | Deduplicate two records into one                   | "LoRA" and "Low-Rank Adaptation" are the same concept    |
| `supersede`| Mark a newer record as replacing an older one      | Revised decision, refined observation                    |
| `resolve`  | Look up a UUID and return its substrate kind + data| "Is this UUID a note or an entity?"                      |
| `request`  | Batch multiple verbs in one call                   | Parallel creates, chained operations                     |

### Notes vs entities

- **Entities** = things in the world: concepts, papers, people, projects, datasets, orgs.
  Graph nodes with typed edges between them.
- **Notes** = your observations about the world: what you noticed, concluded, decided, asked, cited.
  Temporal records with salience and optional graph edges (via `annotates`).

Use `create(kind="entity", entity_kind="concept", ...)` for entities.
Use `create(kind="note", note_kind="observation", ...)` for notes.

---

## The 6 entity kinds (closed set — [ADR-001](docs/adr/ADR-001-entity-kind-taxonomy.md))

| Kind        | What it represents                                           |
| ----------- | ------------------------------------------------------------ |
| `concept`   | Algorithms, techniques, architectures, theories, models      |
| `document`  | Papers, preprints, technical reports, blog posts, books      |
| `dataset`   | Benchmarks, corpora, evaluation sets                         |
| `project`   | Codebases, libraries, tools, frameworks                      |
| `person`    | Researchers, engineers, authors                              |
| `org`       | Labs, companies, institutions                                |

`concept` is the default. Use `properties` for finer distinctions (`type: "paper"`,
`domain: "attention"`, `status: "implemented"`).

---

## The 5 note kinds (closed set — [ADR-019](docs/adr/ADR-019-note-kind-taxonomy.md))

| Kind            | What it records                                 |
| --------------- | ----------------------------------------------- |
| `observation`   | An empirical capture — what you noticed         |
| `insight`       | A synthetic conclusion from observations        |
| `question`      | An open inquiry or research direction           |
| `decision`      | A committed choice with rationale               |
| `reference`     | An external pointer with context (paper, URL)   |

`observation` is the default. Notes can annotate entities via `create(kind="note",
annotates=[entity_id], ...)`.

---

## The 13-relation ontology (closed set — [ADR-002](docs/adr/ADR-002-edge-ontology.md))

When you `link` nodes, use ONLY these relations:

### Structure

- `contains` — parent → child (system contains module)
- `part_of` — inverse of contains
- `instance_of` — specific is a case of general

### Derivation

- `extends` — child builds on parent (Flash Causal extends Flash Tiled)
- `variant_of` — A is a modified version of B (QLoRA variant_of LoRA)
- `introduced_by` — concept first described in paper/by person
- `supersedes` — new replaces old entirely

### Dependency

- `depends_on` — consumer needs dependency at runtime/build
- `enables` — prerequisite makes outcome possible

### Implementation

- `implements` — code realizes algorithm/concept

### Lateral

- `competes_with` — alternative approaches
- `composed_with` — used together in a system

### Annotation

- `annotates` — a note observes/comments on an entity (or another note)

**Why closed**: a sparse ontology stays queryable. Ad-hoc relations (`uses`, `related_to`,
`loaded_by`) fragment the graph and make traversal useless. If your relationship doesn't fit, it's
probably a property on the entity, not an edge.

---

## Research workflow pattern

```
1. search(kind="note", query="topic I'm investigating")
   → see what you already know

2. search(kind="entity", query="FlashAttention")
   → check what's already in the graph

3. For new concepts:
   create(kind="entity", entity_kind="concept", name="...", properties={...})

4. For relationships:
   link(source=A, target=B, relation="extends")

5. For observations/insights:
   create(kind="note", note_kind="observation", content="...", annotates=[entity_id])

6. For structural queries:
   traverse(roots=[entity_id], max_depth=3, relations=["extends", "variant_of"])
```

---

## Entity naming conventions

- **Short canonical names**, not full titles: `LoRA` not
  `Low-Rank Adaptation of Large Language Models`
- **Papers**: entity name = short name (`Sinkhorn Distances`). Full title, authors, year, arxiv ID
  in `properties`
- **Algorithms**: the name people actually say: `GQA`, `RoPE`, `FlashAttention`
- **No prefixes/suffixes**: `Speculative Decoding` not `Speculative Decoding (concept)`

---

## Property conventions

Use these canonical property keys when applicable:

| Key       | Values                                                                                     | Purpose                          |
| --------- | ------------------------------------------------------------------------------------------ | -------------------------------- |
| `type`    | `paper`, `algorithm`, `technique`, `architecture`, `model`, `benchmark`, `dataset`, `tool` | Finer classification than `kind` |
| `domain`  | `attention`, `inference`, `training`, `fine-tuning`, `optimal-transport`, etc.             | Research area                    |
| `status`  | `concept`, `researched`, `prototyped`, `implemented`, `shipped`, `deprecated`              | Maturity                         |
| `source`  | `arxiv:2106.09685` or DOI/URL                                                              | Citation pointer                 |
| `summary` | One-paragraph description                                                                  | Human-readable explanation       |

For papers also include: `title`, `authors`, `year`.

---

## Edge density rules

Sparse graphs are useless. Every entity should have minimum edges:

| Entity kind                | Min edges | Required relations                                                                                                       |
| -------------------------- | --------- | ------------------------------------------------------------------------------------------------------------------------ |
| `concept` (algorithm)      | 4         | `instance_of` or `extends` (at least one parent), `introduced_by` if paper exists, `competes_with` if alternatives exist |
| `concept` (paper)          | 2         | `introduced_by` from concepts it introduced                                                                              |
| `project` (implementation) | 3         | `contains` or `part_of`, `implements` (what concept), `depends_on`                                                       |
| `person`                   | 1         | `introduced_by` from their work                                                                                          |

**Target**: 5+ edges per entity average. Below 3 = polish needed.

---

## GQL traverse examples

```gql
# What does LoRA derive from / what derives from LoRA?
MATCH (a)-[:extends|variant_of*1..3]->(b {name: 'LoRA'}) RETURN a, b

# Find all papers in the attention domain
MATCH (a:concept) WHERE a.domain = 'attention' AND a.type = 'paper' RETURN a

# What concepts does this implementation realize?
MATCH ({name: 'lattice-inference'})-[:implements]->(c:concept) RETURN c

# Multi-hop lineage: from a paper to current implementations
MATCH (p:concept)<-[:introduced_by]-(c)-[:implements*0..2]->(impl)
WHERE p.name = 'Attention Is All You Need'
RETURN c, impl
```

## SPARQL traverse examples

```sparql
# Same as first GQL example, SPARQL syntax
SELECT ?a ?b WHERE { ?a :extends+ ?b . ?b :name 'LoRA' . } LIMIT 10

# Find concepts in a specific domain
SELECT ?a WHERE { ?a a :concept . ?a :domain 'attention' . } LIMIT 20
```

Both syntaxes compile to the same SQL. Use whichever is natural.

---

## Self-expansion: let the graph grow with your work

khive isn't a passive database — it's designed for the graph to grow as you research:

- **Extract**: feed papers in, entities + edges fall out automatically
- **Gap detection**: traverse finds structural holes — "these clusters should connect"
- **Frontier discovery**: identify leaf nodes worth exploring next
- **Annotate**: notes attach observations to entities, creating cross-substrate navigation

Don't think of yourself as a curator. Think of yourself as a researcher whose work happens to leave
structural traces.

---

## Common mistakes

| Mistake                                           | Why it's wrong                                     |
| ------------------------------------------------- | -------------------------------------------------- |
| Storing findings only as notes, never as entities | Notes are for context; entities are for structure  |
| Creating duplicate entities                       | Always `search` first — link to existing if found  |
| Using ad-hoc relations                            | Map to the closed 13-relation set or don't link    |
| Reversed `introduced_by` direction                | concept → paper (the paper introduces the concept) |
| One-hop neighbor queries when you need lineage    | Use `traverse` with `max_depth` for multi-hop      |
| Adding `version`/`date` to entity names           | Those are properties, not names                    |

---

## AI-assisted contribution policy

If you are an AI agent authoring PRs, issues, or comments via someone's CLI:

1. **Attribution**: start the body with a blockquote attribution line:
   `> _PR description authored by Claude (Anthropic agent) on behalf of @<handle>._`
2. **Verify claims**: every claim in your PR description must match the actual diff.
3. **Test evidence**: include `cargo test` output for behavior-changing code.
4. **ADR awareness**: link to relevant ADRs. Schema/interface changes require an ADR first.

---

## See also

- `CLAUDE.md` — for working on khive itself
- `docs/adr/` — Architecture Decision Records (the design contract)
- `docs/adr/README.md` — full ADR index
- khive.ai — public hosted instance (when live)
