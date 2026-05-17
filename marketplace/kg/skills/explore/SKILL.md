---
description: Discover what the knowledge graph knows about a topic. Traverse, narrate connections, surface gaps.
---

# Explore

You want to know what the graph contains about a topic. This skill retrieves, traverses,
and narrates — giving you a grounded picture of existing knowledge and where the gaps are.

## Workflow

### 1. Search for entry points

```
search(kind="entity", query="<topic>")
search(kind="note", query="<topic>")
```

Entity search finds concepts/papers/projects by name and description.
Note search finds observations/insights/decisions by content (excludes superseded notes automatically).

### 2. Expand from hits

For each relevant entity found:

```
neighbors(node_id="<entity-id>", direction="both")
```

This gives immediate connections. For deeper structure:

```
traverse(roots=["<entity-id>"], max_depth=3, direction="out",
  relations=["extends", "variant_of", "instance_of"])
```

Common traversal patterns:
- **Lineage** (what does X build on): `direction="out"`, `relations=["extends", "variant_of", "instance_of"]`
- **Descendants** (what builds on X): `direction="in"`, `relations=["extends", "variant_of", "implements"]`
- **Notes about X**: `direction="in"`, `relations=["annotates"]`
- **What X enables**: `direction="in"`, `relations=["depends_on", "enables"]`

### 3. Pattern matching (when structure matters)

For complex structural queries, use GQL:

```
query("MATCH (a:concept)-[:extends]->(b:concept) WHERE b.name = 'LoRA' RETURN a.name, a.id LIMIT 20")
```

**GQL constraints** (the parser is limited):
- Properties in WHERE use `a.name`, `a.id`, `a.entity_kind` (top-level fields only)
- For JSON properties: use `a.domain`, `a.type` etc. (accessed via json_extract internally)
- `RETURN a.properties` gets the full JSON blob
- NOT supported: `WHERE NOT`, `COUNT`, `ORDER BY`, `[*..N]` variable-length without min
- Relations in patterns: use the 13 canonical relation names

### 4. Narrate

Synthesize what you found into a coherent picture:
- What concepts exist and how they relate
- What the derivation chain looks like (X extends Y which extends Z)
- What notes say (observations, insights, decisions)
- What competing approaches exist

### 5. Surface gaps

Identify what's missing:
- Concepts mentioned but not in the graph
- Entities with low edge count (underdeveloped)
- Questions filed but unresolved
- Expected relationships that don't exist

Report gaps as actionable next steps (e.g., "X exists but has no `introduced_by` edge — find the source paper").

## Choosing the right verb

| Want to... | Use |
|------------|-----|
| Find by content/similarity | `search(kind="entity\|note", query="...")` |
| Immediate connections | `neighbors(node_id, direction, relations)` |
| Multi-hop reachability | `traverse(roots, max_depth, direction, relations)` |
| Structural patterns | `query("MATCH ... RETURN ...")` |
| Browse a category | `list(kind="entity", entity_kind="concept", limit=50)` |

## Stop condition

Topic coverage saturated — you've traversed the relevant subgraph, narrated the connections, and identified actionable gaps. Don't chase every thread; report gaps for follow-up ingestion.
