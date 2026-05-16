---
description: Ingest research material into the knowledge graph — papers, concepts, implementations. Check→Create→Link→Note→Report.
---

# Knowledge Graph Digest

Batch-ingest research artifacts (papers, design docs, codebases) into the khive knowledge graph with full edge density.

## Core Loop (never skip steps)

```
For each entity to ingest:
  1. search(kind="entity", query="<name>")       ← ALWAYS first
  2. search(kind="entity", query="<aliases>")     ← check synonyms
  3. If found → get(id) and enrich; else create
  4. link() with all applicable relations
  5. create(kind="note") for key observations
  6. Verify edge count ≥ minimum for kind
```

## Entity Kinds and When to Use Each

| Kind | Use For | Name Convention |
|------|---------|-----------------|
| `concept` | Algorithms, techniques, architectures, models, theories | Short canonical: `LoRA`, `RoPE`, `FlashAttention` |
| `document` | Papers, preprints, ADRs, technical reports | Short title: `Sinkhorn Distances`, `Attention Is All You Need` |
| `dataset` | Benchmarks, corpora, evaluation sets | `MMLU`, `HumanEval`, `SWE-bench` |
| `project` | Codebases, libraries, tools, frameworks | `lattice-inference`, `khive-runtime` |
| `person` | Researchers, engineers, authors | `Hu et al.` or full name |
| `org` | Labs, companies, institutions | `DeepMind`, `Anthropic` |

**Never add version/date to entity names** — those are properties.

## Property Conventions (use canonical keys)

```python
# Concept/algorithm:
properties = {
    "type": "algorithm",           # paper|algorithm|technique|architecture|model|benchmark|tool
    "domain": "attention",         # attention|inference|training|fine-tuning|optimal-transport
    "status": "researched",        # concept|researched|prototyped|implemented|shipped|deprecated
    "source": "arxiv:2205.14135",  # citation pointer
    "summary": "One-paragraph description for human readability"
}

# Paper (document kind):
properties = {
    "type": "paper",
    "title": "Full paper title here",
    "authors": "Dao et al.",
    "year": "2022",
    "source": "arxiv:2205.14135",
    "domain": "attention"
}
```

## Edge Density Rules (minimum per kind)

| Kind | Min edges | Required |
|------|-----------|---------|
| `concept` (algorithm) | 4 | `instance_of` or `extends` (one parent), `introduced_by` if paper known, `competes_with` if alternatives exist |
| `document` (paper) | 2 | `introduced_by` pointing FROM concepts it introduced |
| `project` (implementation) | 3 | `contains`/`part_of`, `implements` (which concept), `depends_on` |
| `person` | 1 | `introduced_by` from their works |

**Target**: 5+ edges average across all entities. After a batch, count `total_edges / total_entities`. Below 3 = add more edges before finishing.

## Batch Ingestion Pattern (papers with multiple concepts)

```
Paper: "LoRA: Low-Rank Adaptation of Large Language Models"

Step 1: Create paper entity
  create(kind="entity", entity_kind="document", name="LoRA",
    properties={type:"paper", title:"LoRA: Low-Rank...", authors:"Hu et al.", year:"2021", source:"arxiv:2106.09685", domain:"fine-tuning"})

Step 2: Create technique entity
  search → create(kind="entity", entity_kind="concept", name="LoRA",
    properties={type:"technique", domain:"fine-tuning", status:"implemented"})

Step 3: Wire edges
  link(source=technique_id, target=paper_id, relation="introduced_by")   ← concept → paper
  link(source="QLoRA", target=technique_id, relation="variant_of")       ← if QLoRA already exists
  link(source=technique_id, target="PEFT", relation="instance_of")       ← parent category
  link(source=technique_id, target="full fine-tuning", relation="competes_with")

Step 4: Author entity
  search → create person → link(source=paper_id, target=person_id, relation="introduced_by")

Step 5: Insight note
  create(kind="note", note_kind="insight",
    content="LoRA achieves 99% of full fine-tuning quality at 0.1% parameter count by decomposing weight delta into two low-rank matrices",
    salience=0.85, annotates=[technique_id])
```

## Dedup Workflow

```
1. search(kind="entity", query="Low-Rank Adaptation")    ← semantic
2. search(kind="entity", query="LoRA")                   ← exact name
3. list(kind="entity", entity_kind="concept", limit=50)  ← browse if searches inconclusive

Found duplicate: merge(into_id=<canonical>, from_id=<duplicate>)
  strategy="prefer_into"  ← keep canonical's properties
  strategy="union"        ← merge properties (safe default for unknown which is better)
```

`merge` rewires all edges and removes the duplicate. Use `strategy="union"` when uncertain.

## Verification Checklist

After ingesting a batch:
```
query("MATCH (a:concept) RETURN a.name, a.id")
```
For each entity ID: `neighbors(node_id=<id>)` — count edges. Any with < min for kind → add edges before reporting done.
