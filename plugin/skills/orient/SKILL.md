---
description: Audit graph health — find orphans, low-degree nodes, missing edges — and navigate structure with GQL patterns.
---

# Orient

Explore graph structure and audit health. Use this before ingesting new material (to find duplicates) and after batches (to verify density).

## Health Audit Queries

### Find orphan entities (no edges at all)
```gql
MATCH (a:concept) WHERE NOT (a)-[]-() RETURN a.name, a.id
MATCH (a:project) WHERE NOT (a)-[]-() RETURN a.name, a.id
```
Orphans should be zero. If any exist, add `instance_of`/`extends` and `introduced_by` edges before finishing.

### Find low-degree nodes (candidates for enrichment)
```gql
MATCH (a:concept) RETURN a.name, a.id, COUNT {(a)-[]-()} as degree
ORDER BY degree ASC LIMIT 20
```
Concepts with degree < 4 need more edges. Projects with degree < 3 need `implements` + structural edges.

### Compute overall graph density
```gql
MATCH ()-[e]-() RETURN COUNT(e) as total_edges
MATCH (n) RETURN COUNT(n) as total_nodes
```
Divide edges/nodes. Target ≥ 5. Below 3 = graph needs a polish pass.

### Find disconnected clusters
```gql
MATCH (a:concept)-[*..2]-(b:concept) RETURN a.name, b.name
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
-- Paper → concept mapping (what a paper introduced)
MATCH (p:document)-[:introduced_by]-(c:concept) RETURN p.name, c.name

-- Implementation → concept mapping
MATCH (impl:project)-[:implements]->(c:concept) RETURN impl.name, c.name

-- Competitive landscape for a concept
MATCH (a:concept)-[:competes_with]-(b:concept) WHERE a.name = 'softmax attention'
RETURN b.name

-- All concepts in a domain
MATCH (a:concept) WHERE a.domain = 'attention' RETURN a.name, a.status

-- Find concepts ready for implementation (status=researched)
MATCH (a:concept) WHERE a.status = 'researched' RETURN a.name, a.domain
```

## After a Batch Ingest

Run in this order:
1. Orphan query → add edges to any orphans
2. Low-degree query → enrich nodes below minimum
3. Density check → total_edges / total_nodes
4. If density < 4, run a second edge pass before reporting complete
