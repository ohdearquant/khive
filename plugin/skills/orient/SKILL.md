---
name: orient
description: Audit graph health — find orphans, low-degree nodes, missing edges — and navigate structure with GQL patterns.
---

# Orient

Explore graph structure and audit health. Use this before ingesting new material (to find duplicates) and after batches (to verify density).

## Health Audit Procedures

GQL does not support `WHERE NOT`, `COUNT`, or `ORDER BY`. Use multi-step procedures for aggregates.

### Find orphan entities (no edges at all)

```python
# 1. List all concept entities
concepts = list(kind="entity", entity_kind="concept", limit=50)

# 2. For each, check neighbors
for entity in concepts:
    nbrs = neighbors(node_id=entity.id, direction="both")
    if len(nbrs.edges) == 0:
        print(f"Orphan concept: {entity.name} ({entity.id})")

# Repeat for projects
projects = list(kind="entity", entity_kind="project", limit=50)
for entity in projects:
    nbrs = neighbors(node_id=entity.id, direction="both")
    if len(nbrs.edges) == 0:
        print(f"Orphan project: {entity.name} ({entity.id})")
```

Orphans should be zero. If any exist, add `instance_of`/`extends` and `introduced_by` edges before finishing.

### Find low-degree nodes (candidates for enrichment)

```python
# List all concepts, check each for edge count
concepts = list(kind="entity", entity_kind="concept", limit=50)
for entity in concepts:
    nbrs = neighbors(node_id=entity.id, direction="both")
    degree = len(nbrs.edges)
    if degree < 4:
        print(f"Under-linked: {entity.name} — {degree} edges (need ≥ 4)")
```

Concepts with degree < 4 need more edges. Projects with degree < 3 need `implements` + structural edges.

### Compute overall graph density

```python
# Count all edges and nodes
all_edges = list(kind="edge", limit=500)
all_nodes = list(kind="entity", limit=500)
density = len(all_edges) / max(len(all_nodes), 1)
print(f"Density: {density:.1f} edges/node (target ≥ 5)")
```

Below 3 = graph needs a polish pass.

### Find disconnected clusters

```gql
MATCH (a:concept)-[*1..2]-(b:concept) RETURN a.name, b.name LIMIT 50
```

Compare against `list(kind="entity", entity_kind="concept")`. Concepts not appearing in any traversal path are isolated.

## Traversal Patterns

### Full lineage of a concept (out = what it builds on)
```python
traverse(roots=["<FlashAttention-id>"], max_depth=4, direction="out",
         relations=["extends", "variant_of", "instance_of"])
```

### Downstream implementations (in = what builds on it)
```python
traverse(roots=["<LoRA-id>"], max_depth=3, direction="in",
         relations=["extends", "variant_of", "implements"])
```

### Cross-substrate navigation: entity → notes
```python
# Find notes attached to a concept
neighbors(node_id="<id>", direction="in", relations=["annotates"])

# Then fetch each note
get(id="<note-id>")
```

### What a paper introduced
```python
# introduced_by points FROM concept TO paper
# So direction="in" finds concepts that introduced_by this paper
neighbors(node_id=paper_id, direction="in", relations=["introduced_by"])
```

## Browsing Without a Query

```python
list(kind="entity", entity_kind="concept", limit=50)    # all concepts
list(kind="entity", entity_kind="project", limit=20)    # all projects
list(kind="edge", source_id="<id>")                     # edges from a node
list(kind="edge", target_id="<id>")                     # edges to a node
list(kind="edge", relations=["implements"])              # all edges of a type
list(kind="note", note_kind="decision", limit=20)       # all decisions
```

## Common GQL Patterns

```gql
-- What concepts does a paper introduce (via incoming introduced_by edges)
MATCH (c:concept)-[:introduced_by]->(p:document) WHERE p.name = 'LoRA paper' RETURN c.name

-- Implementation → concept mapping
MATCH (impl:project)-[:implements]->(c:concept) RETURN impl.name, c.name

-- Competitive landscape for a concept
MATCH (a:concept)-[:competes_with]->(b:concept) WHERE a.name = 'softmax attention'
RETURN b.name

-- Derivation chains (up to 3 hops)
MATCH (a)-[:extends|variant_of*1..3]->(b) WHERE b.name = 'LoRA' RETURN a.name, b.name LIMIT 20

-- Concepts with a specific property value (use a.properties, not a.domain)
MATCH (a:concept) WHERE a.domain = 'attention' RETURN a.name, a.id LIMIT 20
```

## After a Batch Ingest

Run in this order:
1. Orphan procedure → add edges to any orphans
2. Low-degree procedure → enrich nodes below minimum
3. Density check → total_edges / total_nodes
4. If density < 4, run a second edge pass before reporting complete
