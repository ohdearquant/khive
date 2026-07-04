# Tips and Tricks

Practical usage knowledge for agents and operators working with the `request`
MCP tool. This page collects gotchas that are easy to hit once but hard to
find documented in one place.

## Query craft

### Long, keyword-rich queries beat short ones

`search` and `knowledge.search` fuse full-text (FTS5 trigram) and vector
similarity results with RRF. A short query (one or two words) gives both
signals little to work with: FTS5 trigram matching has few n-grams to match
against, and the embedding has little context to place in vector space. A
longer query that names the language, framework, domain terms, and the shape
of the question gives both retrieval legs more to latch onto and produces
tighter fusion rankings. Prefer a query built from the same nouns you would
use in the document itself over a terse keyword fragment. See
[Search and Retrieval](search.md#score-interpretation) for how the fused
scores are computed.

### `neighbors` and `traverse` default to both directions

As of the fix for issues #445/#480 (`crates/khive-pack-kg/src/handlers/common.rs`,
`parse_direction`), omitting `direction` on `neighbors` or `traverse` resolves
to `Direction::Both`, not outgoing-only. An unrecognized direction string
(a typo like `"inbound"`) is now a rejected `InvalidInput`, not silently
coerced to any particular direction. This is covered by regression tests in
`crates/khive-pack-kg/tests/integration.rs` asserting that a node reachable
only via an incoming edge is still surfaced when `direction` is omitted.

Some older documentation and ADR prose (predating this fix) describes an
`out`-only default as a live footgun — that description is stale as of this
writing. Pass `direction="both"` explicitly anyway when you want to be
unambiguous about intent, or when you specifically want only one direction,
pass `direction="out"` / `direction="in"`:

```
request(ops="neighbors(node_id=\"<uuid>\", direction=\"in\", relations=[\"extends\"])")
```

### `traverse` vs `context`

Both walk the graph, but they answer different question shapes:

- `traverse(roots=[...], max_depth=N)` takes explicit root UUIDs and returns
  paths reachable via BFS, filtered by relation. Use it when you already have
  anchor entities and want lineage, dependency chains, or reachability.
- `context(query=..., entity_ids=...)` resolves anchors from a natural
  language query (via `hybrid_search`), expands 1-2 hops, and assembles the
  result under a character budget in a single call. Use it when you do not
  already have anchor UUIDs and want a budgeted neighborhood summary — for
  example, injecting graph context into a model turn without a
  `search`-then-`neighbors` round trip.

`context` composes the same runtime ops as `search` and `neighbors` (it adds
no new storage or index); see
[ADR-089](../adr/ADR-089-context-verb.md) for the full parameter and
ordering contract. `context`'s `direction` also defaults to `both`, matching
`neighbors`' current default (see the direction note above).

## DSL round-trip tricks

### `$prev` chains only the immediately preceding op

In a chain (`v1(...) | v2(...)`), `$prev` resolves to the result of the op
directly before it, not to any earlier op in the chain. Each step in a chain
overwrites the value `$prev` will resolve to on the next step. If op C needs
a value produced by op A but not passed through by op B, that value is gone
by the time C runs — restructure the chain, or split the calls and pass the
value explicitly.

```
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"NewConcept\") | link(source_id=$prev.id, target_id=\"<existing_id>\", relation=\"extends\")")
```

Path extraction supports `$prev` (the whole result), `$prev.field` (nested
object field), `$prev.items[0].id` (array index into a field), and
`$prev[2]` (top-level array index).

### Parallel batches, max 100 ops

```
request(ops="[search(kind=\"entity\", query=\"LoRA\"), search(kind=\"note\", query=\"LoRA\"), stats()]")
```

Batches run with no ordering guarantee between ops, and a failed op does not
abort the others — each result carries its own `ok`/`error`. The batch size
ceiling is 100 ops; a larger array is rejected rather than silently
truncated.

### Create and link in one round trip

Chaining a `create` into a `link` avoids the two-call round trip shown in
the [Prompt Cookbook](prompt-cookbook.md#two-step-create-then-link):

```
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"GQA\", description=\"Grouped-query attention\") | link(source_id=$prev.id, target_id=\"<existing_paper_id>\", relation=\"introduced_by\")")
```

This only works when the value you need is produced by the op immediately
before the one that consumes it — see the `$prev` scoping note above.

## Param gotchas

A handful of verbs use a parameter name that is easy to guess wrong. These
are enforced by `#[serde(deny_unknown_fields)]` on the underlying param
structs, so a wrong name fails the call outright rather than silently being
ignored:

| Verb                     | Correct param | Common wrong guess |
| ------------------------ | ------------- | ------------------ |
| `search`, `suggest`      | `query`       | `q`                |
| `comm.thread`            | `id`          | `thread_id`        |
| `knowledge.delete_atoms` | `ids`         | `slugs`            |

String values in the function-call DSL must be double-quoted JSON string
literals. The parser reads the raw value slice and feeds it to
`serde_json::from_str`, so a bareword value (an unquoted identifier) fails
JSON parsing and the op errors out, even as a standalone argument:

```
# Wrong — bareword value, fails to parse
request(ops="get(id=abc123)")

# Right — double-quoted string literal
request(ops="get(id=\"abc123\")")
```

## Indexing latency

Writes to the knowledge corpus (`knowledge.upsert_atoms`) land in SQLite and
are visible to `knowledge.search`'s FTS leg on the next call. The Vamana ANN
vector index that provides the semantic leg is different: it is an
in-memory, per-namespace structure that is loaded once and cached, and is
only invalidated by an explicit `knowledge.index()` call (or a daemon
restart) — not automatically after every `upsert_atoms`. A newly written atom
will not surface via the ANN path of `knowledge.search` until a reindex runs.
Treat writes as helping the _next_ indexing pass, not as immediately
recallable through the vector leg: batch writes, then call
`knowledge.index()` (or run `kkernel reindex`) before relying on semantic
recall over what you just wrote.

## Troubleshooting

- `request(ops="verbs()")` and `help=true` on any verb are the live ground
  truth for what is actually registered on the server you are talking to —
  prefer them over any cached doc (including this one) when behavior looks
  off. `help=true` short-circuits before the pack's handler runs, so it never
  has side effects.
- If an MCP client fails to connect to `kkernel mcp` with an opaque error,
  see [Troubleshooting a connect failure](configuration.md#troubleshooting-a-connect-failure)
  in the configuration guide for the stderr probe that surfaces the real
  startup error.

## See also

- [Prompt Cookbook](prompt-cookbook.md): full verb syntax reference
- [Search and Retrieval](search.md): scoring, reranking, and decompose
- [Configuration](configuration.md): config resolution and connect-failure
  troubleshooting
- [ADR-089](../adr/ADR-089-context-verb.md): `context` verb design
- [Proof-Graph Case Study](proof-graph-case-study.md): khive at scale
