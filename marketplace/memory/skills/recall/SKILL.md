---
description: Retrieve prior memory before acting - query durable context, filter by memory type, and use results without overclaiming.
---

# Recall

Recall retrieves memory notes, not every note in the graph. Use it before answering from remembered
context, resuming a project, planning work, or deciding whether something has been seen before.

## Workflow

### 1. Start with a concrete query

Use the words future memory authors were likely to store:

```
request(ops="memory.recall(query=\"marketplace memory plugin remember recall\", limit=5)")
```

Prefer distinctive nouns over generic prompts. "KG agent task queue syntax" is better than "what
happened with agents".

### 2. Narrow by memory type when useful

Use `semantic` for durable facts and preferences:

```
request(ops="memory.recall(query=\"user preference fix specs\", memory_type=\"semantic\", limit=5)")
```

Use `episodic` for session history and prior outcomes:

```
request(ops="memory.recall(query=\"previous marketplace sweep findings\", memory_type=\"episodic\", limit=5)")
```

### 3. Read the result shape

Each recall result includes:

- `score` — absolute relevance (`[0.0, 1.0]`); raw cosine similarity when a vector model is active,
  otherwise composite score.
- `rank_score` — composite ordering score (`[0.0, 1.0]`) used to sort results; combines relevance,
  decayed salience, and temporal recency.
- `raw_score` — pre-fusion vector cosine similarity (`[0.0, 1.0]`), or `null` for text-only hits.
- `salience`, `decay_factor`, `memory_type`, `created_at`, `content`.

All three score fields are bounded to `[0.0, 1.0]`. Pass `include_breakdown=true` to include a
per-component `breakdown` field (relevance, salience contributions, temporal).

Treat higher-ranked hits as more relevant, not automatically true. When a hit matters, carry forward
its `id` in your notes or response so it can be inspected later.

### 4. Adjust thresholds only after the first pass

Recall scores are decay-aware hybrid scores (combining weighted fusion of relevance, salience with
decay, and temporal recency by default). Recent notes with normal salience typically score
0.10–0.25; older or low-salience notes may drop to 0.02–0.08. Start without `min_score` and inspect
the returned scores before setting a threshold:

```
request(ops="memory.recall(query=\"memory pack source_id annotates\", limit=5)")
```

If the results include low-quality hits, set `min_score` just below the score of the last useful
result:

```
request(ops="memory.recall(query=\"memory pack source_id annotates\", min_score=0.02, limit=5)")
```

If important memories may be low-salience, keep `min_score` unset and refine the query instead.

### 5. Act on absence carefully

No recall result means no matching memory was found under the current pack, namespace, query, and
thresholds. It does not prove the fact is false or that no related knowledge exists in KG notes.

For project research, follow a failed recall with KG search if the `kg` pack is available:

```
request(ops="search(kind=\"note\", query=\"<topic>\", limit=10)")
```

## Patterns

### Resume a project

```
request(ops="memory.recall(query=\"<project name> decisions blockers next steps\", limit=10)")
```

Read the hits before creating new tasks or making claims about project state.

### Check user preferences

```
request(ops="memory.recall(query=\"user prefers\", memory_type=\"semantic\", limit=10)")
```

Use this before choosing output format, tone, or workflow when the user has previously expressed
durable preferences.

### Recall by provenance keywords

If memories were tagged with source or domain words, include them:

```
request(ops="memory.recall(query=\"ADR-036 memory recall decay ranking\", memory_type=\"semantic\", limit=5)")
```

### Boost results for known entities

Pass `entity_names` to apply a 1.3× boost to memories associated with those entity names:

```
request(ops="memory.recall(query=\"project decisions\", entity_names=[\"khive-mcp\",\"ADR-036\"], limit=10)")
```

### Filter by tag

```
request(ops="memory.recall(query=\"user preference\", tags=[\"user-preference\"], tag_mode=\"all\", limit=10)")
```

### Choose a fusion strategy

Default is `weighted`. Switch to `vector_only` when keyword matches are noisy, or `rrf` for
balanced precision:

```
request(ops="memory.recall(query=\"embedding recall decay\", fusion_strategy=\"vector_only\", limit=5)")
```

### Get per-component score breakdown

```
request(ops="memory.recall(query=\"project context\", include_breakdown=true, limit=5)")
```

## Anti-patterns

- **Assuming recall covers all knowledge.** It covers memory notes. Use KG search for general graph
  notes and entities.
- **Treating high score as proof.** A recalled memory is evidence of prior stored context, not
  independent verification.
- **Using broad queries first.** Broad queries bury the useful hit under generic memories.
- **Ignoring memory type.** Filtering to `semantic` or `episodic` often removes irrelevant hits.
- **Overwriting the user's current instruction with old memory.** Current explicit instruction wins
  over recalled context.
