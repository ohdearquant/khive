# Demo: research ingest — entities, edges, search, traverse

This is the transcript behind the README's ["Why typed edges, not just vector similarity"](../README.md#why-typed-edges-not-just-vector-similarity) section. It was run
against a scratch database (`KHIVE_DB=/tmp/khive-demo-research.db`), never the production
`khive.db`. Output is captured verbatim from `kkernel` 0.3.0.

Set up a scratch database first:

```bash
export KHIVE_DB=/tmp/khive-demo-research.db
rm -f "$KHIVE_DB"
```

## 1. Create a concept entity

```bash
kkernel exec 'create(kind="entity", entity_kind="concept", name="FlashAttention", description="IO-aware exact attention algorithm", properties={"domain": "attention", "year": 2022})'
```

```json
{
  "results": [
    {
      "ok": true,
      "result": {
        "created_at": "2026-07-04T04:04:35.317117Z",
        "deleted_at": null,
        "description": "IO-aware exact attention algorithm",
        "entity_type": null,
        "id": "88573626-9862-454b-8579-64e9c33e9d1f",
        "kind": "concept",
        "merge_event_id": null,
        "merged_into": null,
        "name": "FlashAttention",
        "namespace": "local",
        "properties": { "domain": "attention", "year": 2022 },
        "tags": [],
        "updated_at": "2026-07-04T04:04:35.317117Z"
      },
      "tool": "create"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

Note: object literals in the DSL need quoted keys — `properties={"domain": "attention"}`, not
`properties={domain: "attention"}`. The latter fails to parse.

## 2. Create a document entity (the paper that introduced it)

```bash
kkernel exec 'create(kind="entity", entity_kind="document", name="FlashAttention: Fast and Memory-Efficient Exact Attention", properties={"authors": "Dao et al.", "year": 2022, "source": "arxiv:2205.14135"})'
```

```json
{
  "results": [
    {
      "ok": true,
      "result": {
        "created_at": "2026-07-04T04:04:42.088887Z",
        "deleted_at": null,
        "description": null,
        "entity_type": null,
        "id": "aa367e75-bdca-41e5-9fdd-0dfc6ecd0050",
        "kind": "document",
        "merge_event_id": null,
        "merged_into": null,
        "name": "FlashAttention: Fast and Memory-Efficient Exact Attention",
        "namespace": "local",
        "properties": { "authors": "Dao et al.", "source": "arxiv:2205.14135", "year": 2022 },
        "tags": [],
        "updated_at": "2026-07-04T04:04:42.088887Z"
      },
      "tool": "create"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

## 3. Create a second concept (a variant)

```bash
kkernel exec 'create(kind="entity", entity_kind="concept", name="FlashAttention-2", description="Improved parallelism and work partitioning over FlashAttention")'
```

```json
{
  "results": [
    {
      "ok": true,
      "result": {
        "created_at": "2026-07-04T04:04:43.233475Z",
        "deleted_at": null,
        "description": "Improved parallelism and work partitioning over FlashAttention",
        "entity_type": null,
        "id": "42d47a12-d2b9-412a-9d84-87f2dc2a1d0b",
        "kind": "concept",
        "merge_event_id": null,
        "merged_into": null,
        "name": "FlashAttention-2",
        "namespace": "local",
        "properties": null,
        "tags": [],
        "updated_at": "2026-07-04T04:04:43.233475Z"
      },
      "tool": "create"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

## 4. Link them with typed edges

```bash
FLASH=88573626-9862-454b-8579-64e9c33e9d1f
PAPER=aa367e75-bdca-41e5-9fdd-0dfc6ecd0050
FA2=42d47a12-d2b9-412a-9d84-87f2dc2a1d0b

kkernel exec "link(source_id=\"$FLASH\", target_id=\"$PAPER\", relation=\"introduced_by\", weight=1.0)"
```

```json
{
  "results": [
    {
      "ok": true,
      "result": {
        "created_at": "2026-07-04T04:04:50.561409Z",
        "deleted_at": null,
        "id": "28510585-b83e-4afd-aa1b-04d6efc3ccea",
        "metadata": null,
        "namespace": "local",
        "relation": "introduced_by",
        "source_id": "88573626-9862-454b-8579-64e9c33e9d1f",
        "target_backend": null,
        "target_id": "aa367e75-bdca-41e5-9fdd-0dfc6ecd0050",
        "updated_at": "2026-07-04T04:04:50.561409Z",
        "weight": 1.0
      },
      "tool": "link"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

```bash
kkernel exec "link(source_id=\"$FA2\", target_id=\"$FLASH\", relation=\"extends\")"
```

```json
{
  "results": [
    {
      "ok": true,
      "result": {
        "created_at": "2026-07-04T04:04:50.614352Z",
        "deleted_at": null,
        "id": "2bb46171-60b4-40c2-9e2d-392b65880857",
        "metadata": null,
        "namespace": "local",
        "relation": "extends",
        "source_id": "42d47a12-d2b9-412a-9d84-87f2dc2a1d0b",
        "target_backend": null,
        "target_id": "88573626-9862-454b-8579-64e9c33e9d1f",
        "updated_at": "2026-07-04T04:04:50.614352Z",
        "weight": 1.0
      },
      "tool": "link"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

## 5. Hybrid search (FTS5 + vector, RRF-fused)

```bash
kkernel exec 'search(kind="entity", query="memory efficient attention")'
```

```json
{
  "results": [
    {
      "ok": true,
      "result": [
        {
          "entity_kind": "document",
          "id": "aa367e75-bdca-41e5-9fdd-0dfc6ecd0050",
          "score": 0.18181818164885044,
          "snippet": "FlashAttention: Fast and Memory-Efficient Exact Attention",
          "title": "FlashAttention: Fast and Memory-Efficient Exact Attention"
        },
        {
          "entity_kind": "concept",
          "id": "88573626-9862-454b-8579-64e9c33e9d1f",
          "score": 0.08333333325572312,
          "snippet": "IO-aware exact attention algorithm",
          "title": "FlashAttention"
        },
        {
          "entity_kind": "concept",
          "id": "42d47a12-d2b9-412a-9d84-87f2dc2a1d0b",
          "score": 0.07692307699471712,
          "snippet": "Improved parallelism and work partitioning over FlashAttention",
          "title": "FlashAttention-2"
        }
      ],
      "tool": "search"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

Search returns a ranked list of all three entities by relevance. It has no notion of which
entity came first or how they're related — that's what the graph is for.

## 6. Traverse the graph — this is what search cannot do

```bash
kkernel exec "traverse(roots=[\"$FLASH\"], max_depth=2, include_roots=true)"
```

```json
{
  "results": [
    {
      "ok": true,
      "result": [
        {
          "nodes": [
            {
              "depth": 0,
              "id": "88573626-9862-454b-8579-64e9c33e9d1f",
              "kind": "concept",
              "name": "FlashAttention",
              "via_edge": null
            },
            {
              "depth": 1,
              "id": "aa367e75-bdca-41e5-9fdd-0dfc6ecd0050",
              "kind": "document",
              "name": "FlashAttention: Fast and Memory-Efficient Exact Attention",
              "via_edge": "28510585-b83e-4afd-aa1b-04d6efc3ccea"
            }
          ],
          "root_id": "88573626-9862-454b-8579-64e9c33e9d1f",
          "total_weight": 1.0
        }
      ],
      "tool": "traverse"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

## 7. Neighbors in both directions

```bash
kkernel exec "neighbors(node_id=\"$FLASH\", direction=\"both\")"
```

```json
{
  "results": [
    {
      "ok": true,
      "result": [
        {
          "edge_id": "2bb46171-60b4-40c2-9e2d-392b65880857",
          "id": "42d47a12-d2b9-412a-9d84-87f2dc2a1d0b",
          "kind": "concept",
          "name": "FlashAttention-2",
          "relation": "extends",
          "weight": 1.0
        },
        {
          "edge_id": "28510585-b83e-4afd-aa1b-04d6efc3ccea",
          "id": "aa367e75-bdca-41e5-9fdd-0dfc6ecd0050",
          "kind": "document",
          "name": "FlashAttention: Fast and Memory-Efficient Exact Attention",
          "relation": "introduced_by",
          "weight": 1.0
        }
      ],
      "tool": "neighbors"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

`neighbors` shows both the `extends` edge from FlashAttention-2 and the `introduced_by` edge to
the paper, each carrying a direction and a type. That's the structure a pure vector index
doesn't have.

## See also

- [Getting Started](../docs/guide/getting-started.md)
- [Knowledge Graph Modeling](../docs/guide/knowledge-graph.md)
