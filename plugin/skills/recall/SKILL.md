---
description: Retrieve from the knowledge graph — choose between search, query, traverse, and neighbors based on what you're looking for.
---

# Recall

Six retrieval verbs serve different questions. Picking the wrong one wastes tokens and misses results.

## Retrieval Verb Selection

| Question type | Use | Why |
|---------------|-----|-----|
| "Find things about X" | `search` | Hybrid FTS5+vector, tolerates paraphrase |
| "Show all entities of kind Y" | `list` | Structured filter, no ranking |
| "What connects to this ID?" | `neighbors` | One-hop graph, cheap |
| "What's reachable from X within N hops?" | `traverse` | Multi-hop BFS, returns subgraph |
| "I have a UUID, show me the record" | `get` | Single fetch, auto-detects type |
| "Pattern match across the graph" | `query` | GQL/SPARQL → SQL |

## Search: Hybrid FTS5 + Vector

```python
search(kind="entity", query="memory efficient attention kernel")
search(kind="note", query="design decision about namespace isolation", limit=5)
```

**Ranking behavior**: RRF fuses two ranked lists — FTS5 trigram matches and AllMiniLmL6V2 cosine similarity. Short queries (1-3 words) are dominated by FTS5. Long natural-language queries lean on vectors. No threshold floor by default — on small graphs, even low-relevance results appear.

**Notes additionally weight by salience**: `score *= (0.5 + 0.5 * salience)`. High-salience decisions surface above low-salience observations even if the text match is weaker.

**Superseded notes are excluded**: notes targeted by a `supersedes` edge don't appear in search results. This is automatic.

## Neighbors: One-Hop Graph

```python
neighbors(node_id="b70dd157", direction="both")
neighbors(node_id="b70dd157", direction="out", relations=["implements", "depends_on"])
neighbors(node_id="b70dd157", direction="in", relations=["introduced_by"])
```

Use `direction="in"` to find things that point TO a concept (e.g., who cites this paper).
Use `direction="out"` to find things this concept points to (e.g., what it extends).
Use `direction="both"` for full local context before deciding where to traverse.

## Traverse: Multi-Hop Lineage

```python
traverse(roots=["b70dd157"], max_depth=3, direction="out")
traverse(roots=["b70dd157"], max_depth=2, relations=["extends", "variant_of"])
traverse(roots=["<id1>", "<id2>"], max_depth=2, include_roots=True)
```

Use `traverse` when you need lineage, clusters, or reachability — not just immediate context.
`relations` filter limits which edge types to follow (recommended for large graphs).
Multiple `roots` lets you find the shared frontier of two concepts.

## Query: Pattern Matching

```gql
-- Who derives from LoRA? (up to 3 hops)
MATCH (a)-[:extends|variant_of*1..3]->(b {name: 'LoRA'}) RETURN a.name, b.name

-- All papers in the attention domain
MATCH (a:concept) WHERE a.domain = 'attention' AND a.type = 'paper' RETURN a.name

-- Concept → paper → person chain
MATCH (p:concept)<-[:introduced_by]-(c)<-[:implements]-(impl)
WHERE p.name = 'Attention Is All You Need' RETURN c.name, impl.name

-- Concepts without any edges (orphans)
MATCH (a:concept) WHERE NOT (a)-[]-() RETURN a.name

-- Low-degree nodes needing enrichment
MATCH (a:concept) RETURN a.name, COUNT {(a)-[]-()} as degree
ORDER BY degree ASC LIMIT 10
```

SPARQL syntax works for the same queries:
```sparql
SELECT ?a ?b WHERE { ?a :extends+ ?b . ?b :name 'LoRA' . } LIMIT 10
```

## Combined Exploration Pattern

```
1. search(kind="entity", query="<topic>")              ← find anchor entity
2. neighbors(node_id=<anchor_id>, direction="both")    ← see immediate context
3. traverse(roots=[<anchor_id>], max_depth=2,
            relations=["extends", "variant_of"])        ← trace lineage
4. search(kind="note", query="<topic>")                ← find attached observations
```

## Short IDs

All UUID-accepting params accept 8+ hex prefix: `get(id="b70dd157")`. The server resolves to the matching record in your namespace. If ambiguous, extend the prefix.
