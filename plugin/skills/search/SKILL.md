---
description: Search mechanics — hybrid FTS5+vector for similarity, GQL/SPARQL for patterns, list for structured filtering.
---

# Search

Three search mechanisms serve different information needs. Choose correctly or you'll either miss results (FTS5 for concept queries) or get noise (semantic for exact ID lookups).

## Mechanism Selection

| Need | Verb | Mechanism |
|------|------|-----------|
| "Find things semantically like X" | `search` | Hybrid FTS5 + vector (RRF) |
| "Give me all entities of kind Y" | `list` | SQL filter — no ranking |
| "Match this structural pattern" | `query` | GQL/SPARQL → SQL |

## Hybrid Search: FTS5 + Vector + RRF

```python
search(kind="entity", query="memory efficient attention kernel")
search(kind="note", query="decision about scoring parameters", limit=10)
```

**How it works**:
1. FTS5 trigram matches the query against name, description, tags, properties
2. AllMiniLmL6V2 computes cosine similarity against stored embeddings
3. Reciprocal Rank Fusion combines: `score = 1/(k + rank_fts) + 1/(k + rank_vec)`, k=60
4. Notes apply salience multiplier: `score *= (0.5 + 0.5 * salience)`
5. Superseded notes (targeted by `supersedes` edge) are excluded from results

**When FTS5 dominates**: short queries (1-3 words), exact term matches, technical identifiers (`RoPE`, `GQA`, `FlashAttention`). FTS5 trigram indexes every substring ≥ 3 chars.

**When vector dominates**: long natural-language queries, paraphrases, concept descriptions. Vector search finds semantically related entities even when exact words don't match.

**No minimum score floor by default.** On small graphs, even unrelated results appear. If precision matters, inspect scores and filter mentally — or use `query` with explicit WHERE conditions instead.

## Structured Filtering with List

```python
list(kind="entity", entity_kind="concept", limit=50)
list(kind="entity", entity_kind="project")
list(kind="edge", source_id="<uuid>")
list(kind="edge", target_id="<uuid>", relations=["implements"])
list(kind="edge", relations=["introduces_by"], min_weight=0.7)
list(kind="note", note_kind="decision", limit=20)
```

`list` is exact filtering — use it when you know the kind and want to browse, not when you need semantic similarity.

## GQL Pattern Matching

GQL queries compile to SQL. Use when `search` would return too much noise or when you need structural patterns across the graph.

```gql
-- Entities with specific property values
MATCH (a:concept) WHERE a.domain = 'attention' AND a.status = 'implemented' RETURN a.name

-- Chain through multiple edge types
MATCH (c:concept)<-[:introduced_by]-(d:document) WHERE d.year = '2022' RETURN c.name, d.name

-- Property projection in RETURN
MATCH (a:concept)-[:competes_with]->(b:concept) RETURN a.name, b.name, a.domain

-- Find all implementations of a concept
MATCH (p:project)-[:implements]->(c:concept) WHERE c.name = 'FlashAttention' RETURN p.name

-- Concepts with no parent (potential orphaned entries needing instance_of)
MATCH (a:concept) WHERE NOT (a)-[:instance_of|extends]->() RETURN a.name
```

## SPARQL Alternative

```sparql
SELECT ?name ?domain WHERE {
  ?a a :concept .
  ?a :domain ?domain .
  ?a :name ?name .
  FILTER(?domain = 'attention')
} LIMIT 20
```

SPARQL and GQL compile to the same SQL. Use whichever syntax is natural — GQL for imperative traversal patterns, SPARQL for triple-pattern matching.

## Combined Search + Graph Pattern

The most powerful retrieval pattern: use `search` to find an anchor, then expand via graph:

```python
# 1. Find anchor
results = search(kind="entity", query="attention efficient")
anchor_id = results[0].id

# 2. See what extends it
traverse(roots=[anchor_id], max_depth=3, direction="in",
         relations=["extends", "variant_of"])

# 3. Find related notes
neighbors(node_id=anchor_id, direction="in", relations=["annotates"])
```

## Search vs Query Decision Tree

```
Is the query natural language or a paraphrase? → search
Is the query an exact property value match?    → query (WHERE a.domain = 'attention')
Do you need multi-hop structural patterns?     → query (MATCH path)
Do you need ranked results by relevance?       → search
Do you need all edges of a specific type?      → list(kind="edge", relations=[...])
```
