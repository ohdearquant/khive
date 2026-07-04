# ADR-089: `context` verb — entity-anchored graph context in one call

**Status**: Proposed
**Date**: 2026-07-04
**Depends on**: ADR-002 (edge ontology), ADR-016 (request DSL), ADR-017 (pack standard), ADR-049 (daemon warm state), ADR-081 (recall retune driver)

## Context

Agents that inject khive context into every model turn need entity-anchored context: the
entities matched by the turn's query, plus their immediate graph neighborhood, assembled
under a hard output budget. Today that requires a caller-side chain of two or more calls
(`search` then `neighbors` per hit, or `search | traverse`), which multiplies MCP
round-trips, moves budget enforcement to the caller, and re-ranks without the graph
signal the runtime already has.

The building blocks exist and are warm-path:

- `hybrid_search` (khive-runtime/src/retrieval.rs) resolves a query to ranked entities
  with one embedding inference against the daemon-warm embedder (ADR-049).
- `neighbors_with_query` (the runtime op behind `neighbors`) provides bounded 1-hop
  neighbor expansion with direction, relation, weight, and limit filters; query-aware
  neighbor reranking is a follow-up (see Amendment below).
- `traverse` provides bounded multi-hop BFS.

No existing verb composes them. `traverse` takes explicit roots without semantic anchor
selection; `knowledge.compose` ranks knowledge sections, not KG neighborhoods. The
per-turn hook consumer (the fleet's prefetch integration) and khivedb end users both need
the composed form, so this is product surface, not an internal convenience.

## Decision

Add one KG-pack verb:

```
context(query?, entity_ids?, hops?, budget?, relations?, direction?, limit?, namespace?)
```

At least one of `query`, `entity_ids` is required; both may be supplied.

### Parameters

| Param        | Type     | Default | Semantics                                                                                        |
| ------------ | -------- | ------- | ------------------------------------------------------------------------------------------------ |
| `query`      | string   | —       | Semantic anchor selection via hybrid search. Does not rank neighbors in v1 (see Amendment below) |
| `entity_ids` | [string] | —       | Explicit anchors (UUID, short prefix, or slug per ADR-046 rules)                                 |
| `hops`       | int      | 1       | Expansion depth; closed range 0..=2. 0 = anchors only                                            |
| `budget`     | int      | 4096    | Output budget in characters, clamped 256..=65536                                                 |
| `relations`  | [string] | all     | Edge-relation filter applied during expansion                                                    |
| `direction`  | string   | "both"  | `outgoing` / `incoming` / `both`                                                                 |
| `limit`      | int      | 5       | Max anchors taken from `query` search, clamped 1..=20                                            |
| `fanout`     | int      | 10      | Max neighbors returned per expanded node per hop, clamped 1..=50                                 |
| `namespace`  | string   | "local" | Standard multi-record namespace default (ADR-007)                                                |

`direction` defaults to `both` for this verb. The `neighbors` verb's `outgoing` default
is a known agent footgun in the context-assembly use case; a new verb is not bound by
the old default and the divergence is documented in both verbs' help text.

### Semantics

1. **Anchor selection.** `entity_ids` resolve directly (each through the standard
   slug-then-prefix resolution), are verified to name an existing, visible entity, and
   are honored in full — caller-supplied ids are never clamped by `limit`; an id that
   does not resolve to a visible entity is a request error (`NotFound`/`InvalidInput`
   naming the offending id), not a silently dropped anchor. `query` runs one
   `hybrid_search` over entities and takes hits until `limit` anchors not already present
   among the explicit ids have been added, fetching a wider candidate window than `limit`
   so that overlap with explicit anchors does not under-fill the query leg (bounded by a
   documented cap — see Amendment below). When both are supplied, explicit ids come
   first and search fills up to `limit` additional anchors; duplicates collapse.
2. **Expansion.** For each anchor, `neighbors_with_query` runs with the verb's
   `direction`/`relations` filters and a per-call result cap of `fanout`. Per-hop
   neighbor ordering is edge-weight descending (see Amendment below for the ordering
   contract). `hops=2` expands each first-hop node once more with the same filters and
   the same `fanout` cap; visited-set dedup prevents cycles. Work done is therefore
   bounded independently of `budget`: at most `anchors × (fanout + fanout²)` neighbor
   records are fetched (defaults: 5 × 110 = 550). `budget` governs output size; `fanout`
   and `hops` govern expansion work.
3. **Hop-2 representation.** Second-hop records are flattened into their anchor's single
   `neighbors` list, marked `hop: 2`, carrying the `relation`/`direction`/`weight` of the
   edge that discovered them and `via` set to the id of their hop-1 parent (hop-1 records
   carry `hop: 1`, `via: null`). Under visited-set dedup, a node reachable from multiple
   anchors or parents appears exactly once: under the first anchor in selection order,
   via the first discovering parent in the deterministic order below.
4. **Assembly.** The response groups by anchor: anchor record (name, kind, description,
   properties), then its neighbor list (name, kind, relation, direction, weight, hop,
   via, one-line description). Deterministic order: anchors in selection order; within an
   anchor, hop-1 before hop-2, each stratum by edge weight descending, ties by UUID
   (see Amendment below).
5. **Budget enforcement.** Assembly appends records in the deterministic order until the
   next record would push the running total past `budget`, then stops and sets
   `truncated: true` with counts of dropped anchors/neighbors. The counted quantity is
   the number of Unicode scalar values in the compact (no-whitespace) JSON serialization
   of each appended record — the same serialization the response returns — so the count
   is deterministic across clients. Truncation is a view decision; nothing is mutated or
   re-queried.

### Response shape

```json
{
  "anchors": [
    {
      "entity": { "id": "…", "name": "…", "kind": "concept", "description": "…" },
      "neighbors": [
        {
          "id": "…",
          "name": "…",
          "relation": "extends",
          "direction": "outgoing",
          "weight": 0.9,
          "hop": 1,
          "via": null,
          "description": "…"
        },
        {
          "id": "…",
          "name": "…",
          "relation": "implements",
          "direction": "incoming",
          "weight": 0.7,
          "hop": 2,
          "via": "<hop-1 parent id>",
          "description": "…"
        }
      ]
    }
  ],
  "truncated": false,
  "dropped": { "anchors": 0, "neighbors": 0 }
}
```

### Latency budget

One embedding inference (query path only; skipped for pure `entity_ids` calls) plus
graph reads. No new index, no new storage. The verb must not regress `search` or
`neighbors` latency; it reuses their runtime ops unchanged. Per-stage timing follows the
memory-pack instrumentation pattern so the serve cost is measurable from day one.

### Attribution

The verb participates in serve-time attribution the same way `memory.recall` does after
ADR-081 §5 lands: responses carry `served_by_profile_id` when a profile resolves, and
serves append to the ledger asynchronously. This rides the #394 mechanism; it is not a
separate design.

## Alternatives rejected

1. **Caller-side chain (`search | neighbors`)** — works today and remains supported, but
   the chain cannot dedup across anchors, cannot enforce a global budget server-side,
   pays N+1 MCP ops, and every consumer reimplements assembly ordering. Kept as the
   interim path, rejected as the product answer.
2. **Extending `traverse` with a `query` parameter** — conflates two contracts:
   `traverse` returns paths from explicit roots; context returns a budgeted neighborhood
   summary from semantic anchors. Overloading would make `traverse`'s response shape
   polymorphic on input, which ADR-023's surface rules discourage.
3. **A memory-pack verb** — context is graph surface over entities, owned by the KG
   pack's vocabulary and edge rules. The memory pack consumes recall, not graph
   expansion; placing it there would add a second cross-pack seam for no gain.
4. **Token-denominated budget** — character budget is deterministic and
   tokenizer-agnostic; the runtime has no tokenizer dependency and must not grow one for
   a view concern. Callers convert at ~4 chars/token.

## Consequences

- One new `Visibility::Verb` handler in `khive-pack-kg` (composition of existing runtime
  ops; no migration, no new storage trait).
- ADR-023 surface amendment: the verb catalog gains `context`; AGENTS.md and the verb
  reference document it.
- The per-turn hook can replace its flat recall+search injection with one `context` call
  once shipped; measured wall-time comparison against the 2.2 s baseline is part of the
  implementation PR's acceptance evidence.

## Amendment (2026-07-04): neighbor ordering

The implementation PR found that `neighbors_with_query` (the runtime op behind both the
`neighbors` verb and this verb's expansion step) has no query-aware reranking of neighbors
today: `NeighborQuery` carries only `direction`, `relations`, `limit`, and `min_weight`, with
no query text input. The original text above ("query also ranks neighbors", "participates in
neighbor ranking exactly as in the neighbors verb", "fused score descending") described a
capability that does not exist in the shipped `neighbors` op, so `context` cannot inherit it.

This amendment makes the shipped behavior the contract, not a documented gap:

- **Per-hop neighbor ordering is edge-weight descending, ties broken by node UUID
  ascending.** This applies uniformly whether or not `query` is supplied. `query` selects
  and orders anchors (via `hybrid_search`) but does not rerank neighbors in v1.
- This ordering is enforced at the storage layer: `neighbors()` in
  `khive-db/src/stores/graph.rs` applies `ORDER BY weight DESC, node_id ASC` before its SQL
  `LIMIT`, so a `fanout` (or the `neighbors` verb's `limit`) narrower than the full neighbor
  set keeps the highest-weight edges rather than an arbitrary row-order subset. Prior to
  this fix, `LIMIT` was applied with no `ORDER BY` and could silently drop high-weight
  neighbors in favor of low-weight ones.
- **Named upgrade path**: query-aware fused-score neighbor reranking remains the intended
  future direction described in the Context section above. Shipping it requires adding a
  query-text input to `NeighborQuery` and a fused dense+edge-weight scoring path in the
  `neighbors_with_query` runtime op; that is out of scope for this verb's implementation
  PR and tracked as a follow-up ADR/implementation against the `neighbors` op itself, not
  against `context`. Once it lands, `context` inherits it automatically since it calls the
  same op.
- **Query-fill window.** Anchor selection's "search fills up to `limit` additional anchors"
  promise (§1 above) is implemented by fetching a candidate window wider than `limit` from
  `hybrid_search` (4× `limit + len(entity_ids)`) so that hits which collapse into explicit
  `entity_ids` duplicates do not under-fill the query leg. This multiplier is an
  implementation detail of the handler, not a caller-visible parameter; it is documented
  here because it is the mechanism that keeps the "up to `limit` additional anchors"
  contract true when overlap occurs.
- **Explicit anchor validation.** `entity_ids` resolution additionally verifies each
  resolved id names an existing, visible entity (not a note, an edge, or a deleted/absent
  row) before expansion begins; a non-entity or unresolvable id is a request error, not a
  silently absent anchor. This was implicit in "honored in full" (§1) but not stated as a
  validation requirement; this amendment makes it explicit.
