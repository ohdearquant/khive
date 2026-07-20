# Prompt Cookbook

Ready-to-use patterns for every common khive operation. Each pattern shows the
exact `request(ops="...")` syntax, expected response shape, and when to use it.

All examples use the function-call DSL form. JSON form is equivalent; use it
when the DSL string would be hard to escape.

---

## Create and link

### Create an entity

```
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention\", description=\"IO-aware exact attention algorithm\", properties={\"domain\": \"attention\", \"year\": 2022})")
```

Response:

```json
{"ok": true, "result": {"id": "a1b2c3d4", "kind": "concept", "name": "FlashAttention", ...}}
```

Use when: you encounter a new algorithm, paper, project, or any named thing
worth tracking. Always `search` first to avoid duplicates.

### Create a note

```
request(ops="create(kind=\"note\", note_kind=\"observation\", content=\"FlashAttention reduces memory from O(N^2) to O(N) by tiling and recomputation\", salience=0.7)")
```

Use when: you want to record a finding, insight, or decision. Notes are
temporal; entities are structural.

### Create an annotated note

```
request(ops="create(kind=\"note\", note_kind=\"insight\", content=\"Tiling is the common technique across all IO-aware attention methods\", annotates=[\"<entity_id>\"])")
```

Use when: your observation is about a specific entity. The `annotates` edge
makes it discoverable via `neighbors`.

### Link two entities

```
request(ops="link(source_id=\"<concept_id>\", target_id=\"<paper_id>\", relation=\"introduced_by\", weight=1.0)")
```

Use when: you discover a relationship. Direction matters, so check the
[edge relation guide](knowledge-graph.md) for source/target conventions.

### Batch create

```
request(ops="[create(kind=\"entity\", entity_kind=\"concept\", name=\"GQA\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"MQA\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"MHA\")]")
```

Use when: you have multiple independent entities to create. Batched ops run in
parallel with no ordering guarantee.

---

## Search and discover

### Search entities

```
request(ops="search(kind=\"entity\", query=\"memory efficient attention\")")
```

### Search with filters

```
request(ops="search(kind=\"entity\", query=\"attention\", entity_kind=\"concept\", tags=[\"ml\"])")
```

### Search notes

```
request(ops="search(kind=\"note\", query=\"tiling recomputation\")")
```

Automatically excludes superseded notes.

---

## Navigate the graph

### One-hop neighbors

```
request(ops="neighbors(node_id=\"<uuid>\", direction=\"both\")")
```

Use when: you want to see everything connected to a node. The default
direction is `both`; pass `out` or `in` only when you need one side.

### Filtered neighbors

```
request(ops="neighbors(node_id=\"<uuid>\", direction=\"in\", relations=[\"extends\", \"variant_of\"])")
```

Use when: you want only specific relationship types.

### Multi-hop traverse

```
request(ops="traverse(roots=[\"<uuid>\"], max_depth=3, relations=[\"extends\", \"variant_of\"], include_roots=false)")
```

Use when: you want to explore lineage: what extends what, multi-hop dependency
chains, reachability analysis.

### GQL query

```
request(ops="query(query=\"MATCH (a:concept)-[:implements]->(b:project) RETURN a.name, b.name LIMIT 10\")")
```

### SPARQL query

```
request(ops="query(query=\"SELECT ?c WHERE { ?c :extends+ ?b . ?b :name 'LoRA' . } LIMIT 10\")")
```

Use GQL or SPARQL when: you need pattern matching over the graph structure, for
example "find all concepts that extend something introduced by a specific paper".

---

## Memory

### Store a memory

```
request(ops="memory.remember(content=\"khive uses RRF fusion for hybrid search scoring\", salience=0.8, memory_type=\"semantic\")")
```

`memory_type`: `episodic` (default) or `semantic` only. Salience: 0.0-1.0
(higher = more important for recall ranking).

### Recall memories

```
request(ops="memory.recall(query=\"hybrid search scoring\", limit=5)")
```

### Tag-filtered recall

```
request(ops="memory.recall(query=\"search optimization\", limit=5, tags=[\"khive\"], tag_mode=\"any\")")
```

### Store a memory linked to an entity

```
request(ops="memory.remember(content=\"FlashAttention-3 uses asynchronous tiling on H100\", salience=0.7, source_id=\"<entity_id>\")")
```

The `source_id` creates an `annotates` edge from the memory note to the entity.

---

## Tasks (GTD)

### Create a task

```
request(ops="gtd.assign(title=\"Implement FlashAttention-3 in lattice\", priority=\"p1\", status=\"next\")")
```

Defaults: `status=inbox`, `priority=p2`.

### Create a task linked to an entity

```
request(ops="gtd.assign(title=\"Benchmark attention variants\", priority=\"p1\", context_entity_id=\"<entity_id>\")")
```

### Get next actions

```
request(ops="gtd.next(limit=5)")
```

Returns tasks with `status` in `[next, active]`, sorted by priority.

### Transition a task

```
request(ops="gtd.transition(id=\"<task_id>\", status=\"active\", note=\"started implementation\")")
```

Lifecycle: `inbox` -> `next` -> `active` -> `done` (or `cancelled`). Also
available: `waiting`, `someday`.

### Complete a task

```
request(ops="gtd.transition(id=\"<task_id>\", status=\"done\")")
```

### List tasks by status

```
request(ops="gtd.tasks(status=\"active\", limit=10)")
```

---

## Brain (Bayesian profiles)

Bayesian profile tuning of recall ranking from feedback signals (`brain.*` verbs) is a
commercially licensed extension distributed separately; it is not part of this
distribution.

---

## Communication

### Send a message

```
request(ops="comm.send(to=\"local\", content=\"Task completed: attention benchmarks ready\")")
```

### Check inbox

```
request(ops="comm.inbox(limit=5)")
```

### Reply in a thread

```
request(ops="comm.reply(id=\"<message_id>\", content=\"Acknowledged, will review\")")
```

### Read a full thread

```
request(ops="comm.thread(id=\"<message_id>\")")
```

---

## Schedule

### Set a reminder

```
request(ops="schedule.remind(content=\"Check benchmark results\", at=\"2026-06-01T09:00:00\")")
```

### Schedule a future verb dispatch

```
request(ops="schedule.schedule(action=\"memory.recall(query='weekly review')\", at=\"2026-06-02T10:00:00\", repeat=\"weekly\")")
```

The `action` parameter is a DSL verb string, not plain text.

### Check agenda

```
request(ops="schedule.agenda()")
```

### Cancel a scheduled event

```
request(ops="schedule.cancel(id=\"<event_id>\")")
```

---

## Curation

### Update an entity

```
request(ops="update(id=\"<uuid>\", description=\"Updated description\", tags=[\"attention\", \"inference\"])")
```

### Merge duplicate entities

```
request(ops="merge(into_id=\"<keep_uuid>\", from_id=\"<duplicate_uuid>\", strategy=\"prefer_into\")")
```

Strategies: `prefer_into` (default), `prefer_from`, `union`.

### Delete a record

```
request(ops="delete(id=\"<uuid>\")")
```

Soft-delete by default. Pass `hard=true` for permanent deletion (cascades
edges for entities).

### Check graph health

```
request(ops="stats()")
```

Returns entity, edge, note, and event counts. Check `total_edges /
total_entities`; below 4 means the graph needs more linking.

---

## Batch and chain patterns

### Parallel batch

Multiple independent operations in one call:

```
request(ops="[search(kind=\"entity\", query=\"LoRA\"), search(kind=\"note\", query=\"LoRA\"), stats()]")
```

Each op runs independently. A failed op does not abort the batch; each entry
has its own `ok`/`error` field.

### Two-step create-then-link

When op B depends on op A's output, use two calls:

```
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"NewConcept\")")
# Read the id from the response, then:
request(ops="link(source_id=\"<new_id>\", target_id=\"<existing_id>\", relation=\"extends\")")
```

### Dedup-before-create pattern

Always search before creating to avoid duplicates:

```
request(ops="search(kind=\"entity\", query=\"FlashAttention\")")
# If found: link to existing. If not found: create.
```

---

## See also

- [Getting Started](getting-started.md): installation and first session
- [Knowledge Graph Modeling](knowledge-graph.md): when to use each entity kind
  and relation
- [Search and Retrieval](search.md): how scoring, reranking, and decompose work
- [Memory and Recall](memory.md): memory-specific patterns
- [GTD Task Management](tasks.md): task lifecycle details
