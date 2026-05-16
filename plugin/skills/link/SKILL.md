---
description: Wire edges in the knowledge graph using the 13 canonical relations. Full ontology reference with examples and weight guidelines.
---

# Link

Edges are the value of the graph. An entity with no edges is a note in a notebook. An entity with edges is a node in a queryable structure. Use only the 13 canonical relations — ad-hoc relations fragment traversal.

## The 13 Relations

### Structure (how things compose)

| Relation | Direction | When | Example |
|----------|-----------|------|---------|
| `contains` | parent → child | System contains module | `lattice-inference` → `GQA module` |
| `part_of` | child → parent | Inverse of contains | `GQA module` → `lattice-inference` |
| `instance_of` | specific → general | One is a case of the other | `LoRA` → `parameter-efficient fine-tuning` |

`contains`/`part_of` are inverses — create one, the other is implied logically but not stored automatically. For `list(kind="edge")` to return both directions, create both.

### Derivation (intellectual lineage)

| Relation | Direction | When | Example |
|----------|-----------|------|---------|
| `extends` | child → parent | Builds on, generalizes | `FlashAttention-3` → `FlashAttention-2` |
| `variant_of` | variant → original | Modified version with different trade-offs | `QLoRA` → `LoRA` |
| `introduced_by` | concept → paper/person | First described in | `LoRA` → `Hu et al. 2021 paper` |
| `supersedes` | new → old | Entirely replaces | `FlashAttention-3` → `Flash Attention original` |

**`introduced_by` direction is concept → paper, not paper → concept.** A common mistake: reversing this makes traversal from "find what a paper introduced" require `direction="in"` instead of the intuitive "out".

`supersedes` is for complete replacement. `extends` is for "builds on but doesn't obsolete."

### Dependency (runtime/build requirements)

| Relation | Direction | When | Example |
|----------|-----------|------|---------|
| `depends_on` | consumer → dependency | Hard requirement to function | `lattice-embed` → `lattice-inference` |
| `enables` | prerequisite → outcome | Makes possible without hard coupling | `Sinkhorn algorithm` → `Wasserstein attention` |

`depends_on` = hard coupling (compile-time, runtime). `enables` = soft facilitation (the prerequisite makes the outcome practical/possible, but the outcome isn't strictly impossible without it).

### Implementation (code ↔ concept)

| Relation | Direction | When | Example |
|----------|-----------|------|---------|
| `implements` | code → concept | Code realizes an algorithm | `lattice-transformer` → `FlashAttention` |

One project can implement multiple concepts. One concept can be implemented by multiple projects.

### Lateral (peer relationships)

| Relation | Direction | When | Example |
|----------|-----------|------|---------|
| `competes_with` | A ↔ B | Alternative approaches to the same problem | `softmax attention` ↔ `linear attention` |
| `composed_with` | A ↔ B | Used together in a system | `GDN` ↔ `GQA` in Qwen3.5 |

Both lateral relations are symmetric — conventionally create one direction, query with `direction="both"`.

### Annotation (notes on entities)

| Relation | Direction | When | Example |
|----------|-----------|------|---------|
| `annotates` | note → entity | Note observes/comments on entity | observation note → `FlashAttention` |

`annotates` is created automatically when you use the `annotates` field in `create(kind="note", annotates=[...])`. You can also create it manually via `link`.

## Weight Guidelines

```
1.0     Definitional (this IS the relationship — LoRA introduced_by Hu 2021)
0.7-0.9 Strong evidence (well-documented, confident)
0.4-0.6 Plausible (believed true, not fully verified)
< 0.4   Speculative (hypothesis, needs investigation)
```

Default weight if omitted: 1.0. Use lower weights for hypothetical or uncertain connections.

## Link Examples

```python
# Core derivation chain
link(source_id=qlora_id, target_id=lora_id, relation="variant_of", weight=1.0)
link(source_id=lora_id, target_id=paper_id, relation="introduced_by", weight=1.0)
link(source_id=lora_id, target_id=peft_id, relation="instance_of", weight=0.9)
link(source_id=lora_id, target_id=full_finetune_id, relation="competes_with", weight=0.8)

# Implementation wiring
link(source_id=codebase_id, target_id=lora_id, relation="implements", weight=1.0)
link(source_id=codebase_id, target_id=parent_id, relation="part_of", weight=1.0)

# Dependency
link(source_id=app_id, target_id=library_id, relation="depends_on", weight=1.0)

# Cross-system composition
link(source_id=gdn_id, target_id=gqa_id, relation="composed_with", weight=0.8)
```

## What Is NOT a Relation

If the relationship doesn't fit the 13, it's either:
- **A property on the entity** (`status`, `domain`, `type`, `version`)
- **Not worth encoding** (don't force weak relationships into the graph)

Common mistakes mapped to correct relations:
- `uses` → `depends_on` or `composed_with`
- `related_to` → too vague; pick the actual relationship
- `loaded_by` → `part_of` or `depends_on`
- `based_on` → `extends` or `variant_of`
- `cites` → `introduced_by` (concept citing its source paper)

## Density Targets

After creating any entity, immediately add minimum edges:

| Kind | Min | First edges to add |
|------|-----|--------------------|
| `concept` (algorithm) | 4 | `instance_of` parent, `introduced_by` paper, `competes_with` alternative |
| `document` (paper) | 2 | `introduced_by` FROM each concept it introduced |
| `project` | 3 | `part_of` parent, `implements` concept, `depends_on` dependency |
| `person` | 1 | `introduced_by` FROM their work |
