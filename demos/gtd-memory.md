# Demo: GTD tasks + memory recall

This transcript exercises the `gtd` and `memory` packs against a scratch database
(`KHIVE_DB=/tmp/khive-demo-gtd.db`), never the production `khive.db`. Output is captured
verbatim from `kkernel` 0.3.0.

```bash
export KHIVE_DB=/tmp/khive-demo-gtd.db
rm -f "$KHIVE_DB"
```

## 1. Create a task

```bash
kkernel exec 'gtd.assign(title="Benchmark FlashAttention-3 on H100", priority="p1", status="next")'
```

```json
{
  "results": [
    {
      "ok": true,
      "result": {
        "assignee": null,
        "context_entity_id": null,
        "created_at": "2026-07-04T04:04:57.188845Z",
        "due": null,
        "full_id": "b4d10534-d1d2-4fa2-869b-f5639a3e9b91",
        "id": "b4d10534",
        "kind": "task",
        "namespace": "local",
        "priority": "p1",
        "properties": { "priority": "p1", "status": "next" },
        "status": "next",
        "title": "Benchmark FlashAttention-3 on H100",
        "updated_at": "2026-07-04T04:04:57.188845Z"
      },
      "tool": "gtd.assign"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

Tasks are notes with `kind="task"`. The GTD pack adds a status lifecycle
(`inbox` → `next` → `active` → `done`/`cancelled`) on top of the same note substrate everything
else in khive uses.

## 2. Store a memory

```bash
kkernel exec 'memory.remember(content="khive uses RRF fusion for hybrid search scoring across FTS5 and vector similarity", salience=0.7, memory_type="semantic")'
```

```json
{
  "results": [
    {
      "ok": true,
      "result": {
        "created_at": "2026-07-04T04:04:58.210957Z",
        "decay_factor": 0.005,
        "id": "2b7c13da-409b-4530-bb02-4478300402b3",
        "kind": "memory",
        "memory_type": "semantic",
        "salience": 0.7
      },
      "tool": "memory.remember"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

`memory_type` accepts exactly two values: `episodic` (the default) or `semantic`.

## 3. List actionable tasks

```bash
kkernel exec 'gtd.next(limit=5)'
```

```json
{
  "results": [
    {
      "ok": true,
      "result": [
        {
          "assignee": null,
          "context_entity_id": null,
          "created_at": "2026-07-04T04:04:57.188845Z",
          "due": null,
          "full_id": "b4d10534-d1d2-4fa2-869b-f5639a3e9b91",
          "id": "b4d10534",
          "kind": "task",
          "namespace": "local",
          "priority": "p1",
          "properties": { "priority": "p1", "status": "next" },
          "status": "next",
          "title": "Benchmark FlashAttention-3 on H100",
          "updated_at": "2026-07-04T04:04:57.188845Z"
        }
      ],
      "tool": "gtd.next"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

`gtd.next` lists tasks with `status` in `[next, active]`, sorted by priority. This is the "what
should I do now" query.

## 4. Recall the memory

```bash
kkernel exec 'memory.recall(query="hybrid search scoring", limit=5)'
```

```json
{
  "results": [
    {
      "ok": true,
      "result": [
        {
          "content": "khive uses RRF fusion for hybrid search scoring across FTS5 and vector similarity",
          "created_at": "2026-07-04T04:04:58.210957Z",
          "decay_factor": 0.005,
          "id": "2b7c13da-409b-4530-bb02-4478300402b3",
          "memory_type": "semantic",
          "rank_score": 0.7197960019111633,
          "raw_score": 0.6888881921768188,
          "salience": 0.7,
          "score": 0.6888881921768188
        }
      ],
      "tool": "memory.recall"
    }
  ],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

Recall ranking combines the query-relevance score with the memory's stored salience and its
decay factor, so a highly salient memory outranks a marginally more relevant but low-salience one.

## 5. Check graph health

```bash
kkernel exec 'stats()'
```

```json
{
  "results": [{ "ok": true, "result": { "edges": 0, "entities": 0, "notes": 2 }, "tool": "stats" }],
  "summary": { "aborted": 0, "failed": 0, "succeeded": 1, "total": 1 }
}
```

Two notes: the task and the memory. No entities or edges in this database, because this demo
doesn't touch the KG substrate at all. That's the point: GTD and memory work standalone if
that's all an agent needs.

## See also

- [GTD Task Management](../docs/guide/tasks.md)
- [Memory and Recall](../docs/guide/memory.md)
