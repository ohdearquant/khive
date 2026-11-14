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

Returns a ranked list. This abbreviated example shows the ranking fields:

```json
[
  {
    "id": "a1b2c3d4",
    "name": "FlashAttention",
    "rank_score": 0.0325,
    "rank_score_kind": "rrf",
    "signals": { "vector_similarity": 0.842, "keyword_score": 18.4 },
    "score": 0.0325
  },
  {
    "id": "e5f6a7b8",
    "name": "PagedAttention",
    "rank_score": 0.0161,
    "rank_score_kind": "rrf",
    "signals": { "keyword_score": 12.1 },
    "score": 0.0161
  }
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

`rank_score` is the deterministic ordering value for this query and strategy.
It is not a probability, a percentage match, or comparable across queries.
`rank_score_kind` names the strategy: `rrf`, `vector`, `keyword`, `weighted`,
or `union`. Entity exact-title boosts and note salience weighting affect the
rank score while retaining the `rrf` kind.

`signals` contains available component evidence before fusion and modifiers:
`vector_similarity` and/or `keyword_score`. Missing evidence has no key;
it is never filled with zero. Scores belong to the backend and embedding model
that produced them, so vector similarities from different models are not
comparable. Canonical responses carry an empty object when no evidence was
retained; Agent presentation omits that empty object and rounds numeric scores.

Use `min_rank_score` for an inclusive, strategy-local floor in `[0,1]`. The
server compares fixed-point scores after fusion and modifiers, before the
final result limit, including when sorting by creation or update time. This
input range does not make the score a calibrated relevance measure.

In v0.8, `score` is a deprecated exact alias of `rank_score`, and `min_score`
is a deprecated exact alias of `min_rank_score`. Supplying both parameter names
is an error, even when equal. Prefer the canonical names in new callers.
`knowledge.search` and `memory.recall` retain their separate scoring contracts.

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
`relations` filters. `limit` defaults to 100 non-root nodes per root and is
capped at 1,000; the full request also has fixed root, depth, work, and time
bounds documented in the [API reference](api-reference.md#traverse--assertive).

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

## Knowledge search: rerank and decompose

The `knowledge.search` verb adds two capabilities on top of base search:

### Reranking

```
request(ops="knowledge.search(query=\"memory efficient attention mechanisms\", rerank=true)")
```

Reranking uses a cross-encoder model to re-score results after the initial
retrieval pass. This produces clean [0,1] scores instead of raw RRF values.
Reranking is on by default for `knowledge.search`.

### Query decomposition

```
request(ops="knowledge.search(query=\"compare LoRA and QLoRA fine-tuning approaches\", decompose=true)")
```

Decomposition splits multi-concept queries into sub-queries, runs them
independently, and merges the results. This avoids FTS edge cases where
compound queries miss relevant documents.

Use `decompose=true` when your query mentions multiple distinct concepts.

## Memory recall

`memory.recall` is a specialized search over memory notes with decay-weighted
scoring:

```
request(ops="memory.recall(query=\"attention optimization\", limit=5)")
```

See [Memory and Recall](memory.md) for the full scoring formula and usage
patterns.

## Choosing the right retrieval

| You want to...                    | Use                                          |
| --------------------------------- | -------------------------------------------- |
| Find entities about a topic       | `search(kind="entity", query="...")`         |
| Find notes about a topic          | `search(kind="note", query="...")`           |
| Browse all entities of a kind     | `list(kind="entity", entity_kind="concept")` |
| See what connects to a node       | `neighbors(node_id="...", direction="both")` |
| Explore multi-hop paths           | `traverse(roots=["..."], max_depth=3)`       |
| Structural pattern matching       | `query(query="MATCH ...")`                   |
| Find knowledge atoms with scoring | `knowledge.search(query="...", rerank=true)` |
| Recall agent memories             | `memory.recall(query="...")`                 |

## Provisioning Qwen3-Embedding-0.6B

The `qwen3-embedding-0.6b` embedding provider loads local model files. Before
selecting it, download the four files below from Qwen's upstream repository at
revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3` into a fresh model directory.
These links pin that revision; the sizes and SHA256 hashes apply to the 0.6B
model.

| File                                                                                                                                             |         Bytes | SHA256                                                             |
| ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------: | ------------------------------------------------------------------ |
| [config.json](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B/resolve/97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3/config.json)                     |           727 | `b5bf1f51fc45be473a54718cef92448d90a1be001bf9b9a44b8c7f10a19feaa9` |
| [model.safetensors](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B/resolve/97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3/model.safetensors)         | 1,191,586,416 | `0437e45c94563b09e13cb7a64478fc406947a93cb34a7e05870fc8dcd48e23fd` |
| [tokenizer.json](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B/resolve/97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3/tokenizer.json)               |    11,423,705 | `def76fb086971c7867b829c23a26261e38d9d74e02139253b38aeb9df8b4b50a` |
| [tokenizer_config.json](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B/resolve/97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3/tokenizer_config.json) |         9,706 | `253153d0738ceb4c668d2eff957714dd2bea0b56de772a9fdccd96cbf517e6a0` |

The default directory is `$HOME/.lattice/models/qwen3-embedding-0.6b`.
To use another directory, set `LATTICE_QWEN_MODEL_DIR` to its path in the
environment of the process loading the model. For daemon-backed requests,
the daemon must receive that environment before model loading.
Start with only the four files above: an existing optional
`inference_config.json` can change loading settings.

In the model directory, verify the downloaded files:

```sh
shasum -a 256 config.json model.safetensors tokenizer.json tokenizer_config.json
```

Compare each result with the table before selecting `qwen3-embedding-0.6b`
in your [engine configuration](../khive-config-example.toml).
Provisioning and hash verification are manual steps; the loader does not
download these files or enforce these pinned hashes.

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
