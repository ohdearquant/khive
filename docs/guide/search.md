# Search and Retrieval

This guide covers how to find things in khive — from keyword search to vector
similarity to graph traversal — and when to use each approach.

## Five ways to retrieve

khive offers five retrieval verbs, each suited to a different question shape:

| Verb                  | Question shape                      | Example                                     |
| --------------------- | ----------------------------------- | ------------------------------------------- |
| `get(id)`             | "I have a UUID, give me the record" | Fetch a known entity after a link operation |
| `search(kind, query)` | "Find things about X"               | Discover entities matching a topic          |
| `list(kind, filters)` | "Show me all Y"                     | Browse all concepts, all edges from a node  |
| `neighbors(node_id)`  | "What connects to this?"            | One-hop graph exploration                   |
| `traverse(roots)`     | "What is reachable within N hops?"  | Multi-hop lineage, clusters                 |
| `query(gql)`          | "Pattern match over the graph"      | Complex structural queries                  |

## Text search: `search`

`search` combines full-text search (FTS5 trigram) with vector similarity
(embedding-based) using Reciprocal Rank Fusion (RRF).

### Basic search

```
request(ops="search(kind=\"entity\", query=\"memory efficient attention\")")
```

Returns a scored list:

```json
[
  {"id": "a1b2c3d4", "name": "FlashAttention", "score": 0.82, ...},
  {"id": "e5f6g7h8", "name": "PagedAttention", "score": 0.71, ...}
]
```

### Search notes

```
request(ops="search(kind=\"note\", query=\"tiling recomputation\")")
```

Note search automatically excludes superseded notes (notes targeted by a
`supersedes` edge). This is a view-layer filter — the old notes still exist.

### Filtered search

Narrow by entity kind, type, or tags:

```
request(ops="search(kind=\"entity\", query=\"attention\", entity_kind=\"concept\", tags=[\"ml\"])")
```

### Score interpretation

Scores from `search` are RRF fusion scores. Raw RRF values are typically small
(0.01-0.03).

A practical floor: results below 0.3 are usually noise. Results above 0.7 are
strong matches.

## Structured browse: `list`

`list` returns records matching structured filters, without text similarity:

```
request(ops="list(kind=\"entity\", entity_kind=\"concept\", limit=20)")
request(ops="list(kind=\"edge\", source_id=\"<uuid>\")")
request(ops="list(kind=\"note\", note_kind=\"decision\", limit=10)")
```

Use `list` when you want categorical browsing, not similarity ranking.

## Graph navigation: `neighbors` and `traverse`

### One-hop: neighbors

```
request(ops="neighbors(node_id=\"<uuid>\", direction=\"both\")")
```

Direction options: `out`, `in`, `both` (default). Omitting `direction`
returns edges in both directions; pass `out` or `in` when you specifically
want only one side.

Filter by relation:

```
request(ops="neighbors(node_id=\"<uuid>\", direction=\"in\", relations=[\"extends\", \"variant_of\"])")
```

### Multi-hop: traverse

```
request(ops="traverse(roots=[\"<uuid>\"], max_depth=3, relations=[\"extends\", \"variant_of\"])")
```

Returns paths — each path is a list of nodes from root to leaf. Use
`include_roots=false` to exclude the starting nodes from results.

Traverse is BFS-based. It respects `direction` (default: `both`) and
`relations` filters.

## Pattern matching: `query`

For complex structural questions, use GQL or SPARQL:

### GQL

```
request(ops="query(query=\"MATCH (a:concept)-[:extends]->(b:concept) WHERE b.name = 'LoRA' RETURN a\")")
```

```
request(ops="query(query=\"MATCH (p:document)<-[:introduced_by]-(c:concept)<-[:implements]-(impl:project) RETURN c.name, impl.name\")")
```

### SPARQL

```
request(ops="query(query=\"SELECT ?a WHERE { ?a :extends+ ?b . ?b :name 'LoRA' . } LIMIT 10\")")
```

Both syntaxes compile to the same SQL backend. Use whichever feels natural.

## Memory recall

`memory.recall` is a specialized search over memory notes with decay-weighted
scoring:

```
request(ops="memory.recall(query=\"attention optimization\", limit=5)")
```

See [Memory and Recall](memory.md) for the full scoring formula and usage
patterns.

## Choosing the right retrieval

| You want to...                | Use                                          |
| ----------------------------- | -------------------------------------------- |
| Find entities about a topic   | `search(kind="entity", query="...")`         |
| Find notes about a topic      | `search(kind="note", query="...")`           |
| Browse all entities of a kind | `list(kind="entity", entity_kind="concept")` |
| See what connects to a node   | `neighbors(node_id="...", direction="both")` |
| Explore multi-hop paths       | `traverse(roots=["..."], max_depth=3)`       |
| Structural pattern matching   | `query(query="MATCH ...")`                   |
| Recall agent memories         | `memory.recall(query="...")`                 |

## Performance notes

- **Cold start**: the first search in a session loads the ANN index and
  embedding model. The daemon keeps these warm for subsequent calls.
- **Daemon**: khive auto-spawns `kkernel mcp --daemon` on first request. The
  daemon keeps indexes hot across sessions.
- **Vector search without embeddings**: if running with `--no-embed`, only FTS
  results are returned (no vector similarity).

## See also

- [Prompt Cookbook](prompt-cookbook.md) — search patterns with full syntax
- [Memory and Recall](memory.md) — memory-specific recall with decay
- [AGENTS.md](../../AGENTS.md) — GQL and SPARQL examples
