---
description: Wire a new concept into existing knowledge. Find what it relates to, create the edges, reach minimum density.
---

# Connect

You just encountered something new and want to integrate it into your existing graph.
This skill finds what it relates to and wires the edges.

## Workflow

### 1. Check if it already exists

```
search(kind="entity", query="<the new thing>")
```

If it exists: skip to step 3 (enriching edges). If it's a close match but different (e.g., a variant): create it and link via `variant_of` or `extends`.

### 2. Create the entity

```
create(kind="entity", entity_kind="<kind>", name="<short canonical name>",
  description="<what it is>", properties={...})
```

Pick from 6 kinds: `concept`, `document`, `dataset`, `project`, `person`, `org`.

### 3. Find what it connects to

Search broadly for related entities:

```
search(kind="entity", query="<related terms, parent concept, enabling technique>")
```

Think about these relationship dimensions:
- **What is it a kind of?** → `instance_of` (specific → general)
- **What does it build on?** → `extends` (child → parent)
- **Who introduced it?** → `introduced_by` (concept → paper/person)
- **What does it need?** → `depends_on` (consumer → dependency)
- **What does it compete with?** → `competes_with` (A ↔ B)
- **What does it enable?** → look for entities that `depends_on` prerequisites this fulfills
- **Where is it implemented?** → `implements` (code → concept)
- **What is it used with?** → `composed_with` (A ↔ B)

### 4. Create the edges

For each identified relationship:

```
link(source_id="<from>", target_id="<to>", relation="<relation>", weight=<0.4-1.0>)
```

**Direction rules** (the most common mistakes):

| Relation | Points FROM → TO | Mnemonic |
|----------|------------------|----------|
| `introduced_by` | concept → paper | "LoRA was introduced_by Hu 2021" |
| `extends` | child → parent | "QLoRA extends LoRA" |
| `instance_of` | specific → general | "GQA is an instance_of grouped attention" |
| `implements` | code → concept | "lattice-inference implements GQA" |
| `depends_on` | consumer → dependency | "quantization depends_on calibration data" |

If the relationship doesn't fit any of the 13 relations, it's probably a **property** on the entity (e.g., "published in 2021" → `properties.year: "2021"`, not an edge).

### 5. Verify density

```
neighbors(node_id="<new-entity-id>", direction="both")
```

**Targets**: concepts ≥ 4 edges, projects ≥ 3, documents ≥ 2. If below, actively seek more connections — especially `instance_of`/`extends` (every concept has a parent) and `introduced_by` (most concepts have a source).

### 6. Report

State what was connected: the new entity, each edge created (with direction and rationale), and final edge count. Flag if density target wasn't met and why.

## Stop condition

Entity is at or above minimum density. Each edge has clear rationale. If you can't reach the density target, note what's missing (e.g., "no source paper found for this concept — needs research").

## What is NOT a relation

These are properties, not edges:
- "published in 2021" → `properties.year`
- "uses Python" → `properties.language`
- "has 7B parameters" → `properties.params`
- "achieves 95% accuracy on X" → `properties.benchmark_results`
- "is popular" / "is important" → not an edge; maybe `salience` on a note about it
